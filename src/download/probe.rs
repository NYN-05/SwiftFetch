//! Resource probing (Phase 3).
//!
//! Before a transfer starts, the engine must know what the server actually
//! offers. [`probe`] inspects one HTTP/HTTPS URL with a `HEAD` request and
//! one bounded ranged request, and records *only facts the responses
//! support* (`DOCS/architecture.md` §8):
//!
//! - the expected size, when a single, integral `Content-Length` was seen
//!   and no other response contradicts it;
//! - range support only after a correct `206 Partial Content` answer to a
//!   `bytes=0-0` request — a full `200` to a range request means ranges are
//!   ignored, and a mismatched `Content-Range` is never evidence of support;
//! - `ETag` and `Last-Modified` validators, when supplied, as raw strings
//!   (resume logic in later phases decides their use);
//! - the effective URL after permitted redirects.
//!
//! Probing never buffers whole bodies: no probe response body is read at
//! all (metadata lives in headers), and a server that streams against a
//! `bytes=0-0` request has the connection abandoned right after the
//! headers. A probe performs at most one primary request (`HEAD`, or a
//! ranged GET when HEAD is unsupported) plus at most one secondary
//! evidence request.
//!
//! Contradictory metadata is never silently trusted: every contradiction
//! downgrades the affected facts to "unknown" and is logged as a warning.
//! All URLs in diagnostics go through [`crate::logging::redact_url`]; the
//! one place a raw URL could leak (`reqwest`'s error `Display`) is mapped
//! away by `http::http_error`, which keeps that detail in the hidden
//! diagnostic source.
//!
//! Phase 3 wiring note (dead-code waiver): commands are wired in Phase 11;
//! until then these items are exercised by tests and become load-bearing
//! with the Phase 4 sequential transfer.
#![allow(dead_code)]

use crate::download::http::{http_status_error, HttpClient, PROBE_RANGE};
use crate::error::AppError;
use crate::logging::redact_url;
use reqwest::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED};
use reqwest::{Method, StatusCode, Url};

/// Range support as determined by evidence, not advertisement:
/// `Yes` = proven by a correct ranged response, `No` = proven absent (the
/// server answered a well-formed range request with a full response),
/// `Unknown` = no decisive evidence (read as "do not rely on ranges").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeSupport {
    /// A correct `206 Partial Content` answered the exact requested range.
    Yes,
    /// The server answered a well-formed range request with a full response.
    No,
    /// Inconclusive, contradictory, or a server error on the evidence path.
    Unknown,
}

/// The facts a probe could establish about one remote resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    /// Expected total size when trustworthy metadata declared one.
    pub expected_size: Option<u64>,
    /// Evidence-based range support.
    pub range: RangeSupport,
    /// Raw `ETag`, when the server supplied one.
    pub etag: Option<String>,
    /// Raw `Last-Modified`, when the server supplied one.
    pub last_modified: Option<String>,
    /// Final URL after permitted redirects (never logged raw).
    pub effective_url: String,
}

/// The states a `Content-Length` value can be in after strict parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentLength {
    Missing,
    Valid(u64),
    /// Unparseable, conflicting duplicates, or integral overflow: never trusted.
    Invalid,
}

/// A parsed `Content-Range` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RangeSpec {
    pub(crate) first: u64,
    pub(crate) last: u64,
    /// `Some(total)` for the explicit form, `None` when the server wrote `*`.
    pub(crate) total: Option<u64>,
}

/// Content-transform state of a response body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContentEncoding {
    /// No `Content-Encoding` header, or explicitly `identity`.
    Identity,
    /// Any other value: a content transform invalidates byte offsets.
    Transform,
}

/// The metadata carried by any one response of the probe exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResponseMeta {
    pub(crate) cl: ContentLength,
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
    pub(crate) encoding: ContentEncoding,
}

/// One ranged response as understood by the probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RangeAnswer {
    /// Correct, exact `206 Partial Content` for `bytes=0-0`.
    Exact {
        /// The representation total from `Content-Range`, when stated.
        total: Option<u64>,
    },
    /// The server ignored the range and answered with a full response.
    Ignored,
    /// `206` whose `Content-Range`/`Content-Length` cannot be reconciled
    /// with the exact `bytes=0-0` request (or with the known HEAD size), or
    /// one carrying a content transform. Everything such a response claims
    /// about size or ranges becomes untrusted.
    Contradicted,
    /// `416` unsatisfiable: not usable as range evidence; the total, when
    /// present via `bytes */<total>`, is still an advertised fact.
    Unsatisfiable { total: Option<u64> },
    /// Any other HTTP status on the ranged request. This is not range
    /// evidence and not a contradiction — e.g. a 5xx blip after a good
    /// HEAD keeps the HEAD-side facts intact.
    Other,
}

/// All `Content-Length` headers of a response, parsed strictly.
///
/// The value is [`ContentLength::Valid`] only when the header is present
/// and every declared value parses identically; anything else (missing,
/// malformed, conflicting duplicates, overflow) is not a fact.
fn content_length_of(headers: &reqwest::header::HeaderMap) -> ContentLength {
    let mut state = ContentLength::Missing;
    for value in headers.get_all(CONTENT_LENGTH) {
        let parsed = match value.to_str() {
            Ok(text) => parse_content_length_value(text),
            Err(_) => ContentLength::Invalid,
        };
        state = match (state, parsed) {
            // 1. First value decides (Missing or unchanged).
            (ContentLength::Missing, parsed) => parsed,
            // 2. Identical repeated values stay valid.
            (previous, parsed) if previous == parsed => previous,
            // 3. Any disagreement or second anomaly breaks trust entirely.
            (_, _) => return ContentLength::Invalid,
        };
    }
    state
}

/// Parses one `Content-Length` value: plain ASCII digits, once, only.
fn parse_content_length_value(raw: &str) -> ContentLength {
    let trimmed = raw.trim();
    if !count_only(trimmed) {
        return ContentLength::Invalid;
    }
    match trimmed.parse::<u64>() {
        Ok(value) => ContentLength::Valid(value),
        Err(_) => ContentLength::Invalid,
    }
}

/// Parses a raw `Content-Range`, e.g. `bytes 0-0/4096` or `bytes 0-499/*`.
///
/// The `416` form (`bytes */N`) is rejected: a `206` answer to an exact
/// range must name an exact, non-empty interval in byte units. The resume
/// engine reuses this same strict parser for its own ranged responses
/// (one parsing source, one trust policy).
pub(crate) fn parse_content_range(raw: &str) -> Option<RangeSpec> {
    let rest = raw.trim().strip_prefix("bytes ")?;
    let (interval, total_text) = rest.split_once('/')?;
    let (first_text, last_text) = interval.trim().split_once('-')?;
    if !count_only(first_text) || !count_only(last_text) {
        return None;
    }
    let first = first_text.parse::<u64>().ok()?;
    let last = last_text.parse::<u64>().ok()?;
    let total = match total_text.trim() {
        "*" => None,
        digits if count_only(digits) => Some(digits.parse::<u64>().ok()?),
        _ => return None,
    };
    Some(RangeSpec { first, last, total })
}

/// `true` when every character is an ASCII digit (empty string rejected).
fn count_only(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// Parses the total of a `416`-form `Content-Range` (`bytes */N`).
///
/// On an unsatisfiable-range answer the server's only statement is the
/// complete length; the interval part is intentionally absent.
fn parse_unsatisfiable_total(raw: &str) -> Option<u64> {
    let rest = raw.trim().strip_prefix("bytes ")?;
    let (interval, total_text) = rest.split_once('/')?;
    if interval.trim() != "*" {
        return None;
    }
    let digits = total_text.trim();
    if !count_only(digits) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// Aggregates the facts of one response (status-agnostic).
///
/// pub(crate) so the transfer phase reuses the exact same header-parsing
/// policy for its own response (one parsing source, one trust policy).
pub(crate) fn response_meta(response: &reqwest::Response) -> ResponseMeta {
    let cl = content_length_of(response.headers());
    let encoding = match response
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => ContentEncoding::from_str(value),
        None => ContentEncoding::Identity,
    };
    ResponseMeta {
        cl,
        etag: header_string(response, ETAG),
        last_modified: header_string(response, LAST_MODIFIED),
        encoding,
    }
}

impl ContentEncoding {
    fn from_str(value: &str) -> Self {
        if value.eq_ignore_ascii_case("identity") {
            ContentEncoding::Identity
        } else {
            ContentEncoding::Transform
        }
    }
}

fn header_string(
    response: &reqwest::Response,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(std::string::ToString::to_string)
}

/// Validators plus the effective URL carried by one response.
fn response_identity(response: &reqwest::Response) -> (Option<String>, Option<String>, String) {
    let etag = header_string(response, ETAG);
    let last_modified = header_string(response, LAST_MODIFIED);
    let effective_url = response.url().as_str().to_owned();
    (etag, last_modified, effective_url)
}

/// Classifies a ranged-GET response against the exact `bytes=0-0` request.
///
/// `head_size` participates in contradiction checks (pass
/// [`ContentLength::Missing`] when the HEAD phase produced no usable
/// values, e.g. in the HEAD-unsupported fallback path).
fn classify_ranged_response(
    response: &reqwest::Response,
    head_size: ContentLength,
) -> (RangeAnswer, ResponseMeta) {
    let meta = response_meta(response);
    if meta.encoding != ContentEncoding::Identity {
        // A transformed body cannot be addressed by entity byte offsets:
        // the whole answer is rejected as evidence (architecture §13).
        return (RangeAnswer::Contradicted, meta);
    }
    match response.status() {
        StatusCode::PARTIAL_CONTENT => {
            let spec = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range);
            let exact_interval = spec.is_some_and(|spec| spec.first == 0 && spec.last == 0);
            if let Some(spec) = spec.filter(|_| exact_interval) {
                // A `Content-Length` on a 206 must equal the interval length.
                let interval_len = spec.last - spec.first + 1;
                if matches!(meta.cl, ContentLength::Valid(n) if n != interval_len) {
                    return (RangeAnswer::Contradicted, meta);
                }
                // A known HEAD size that disagrees with the representation
                // total is exactly the "contradictory metadata" case.
                if let (ContentLength::Valid(head), Some(total)) = (head_size, spec.total) {
                    if head != total {
                        return (RangeAnswer::Contradicted, meta);
                    }
                }
                (RangeAnswer::Exact { total: spec.total }, meta)
            } else {
                (RangeAnswer::Contradicted, meta)
            }
        }
        StatusCode::OK => (RangeAnswer::Ignored, meta),
        StatusCode::RANGE_NOT_SATISFIABLE => {
            // Total may be present via the `bytes */<total>` form.
            let total = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_unsatisfiable_total);
            (RangeAnswer::Unsatisfiable { total }, meta)
        }
        _ => (RangeAnswer::Other, meta),
    }
}

/// The pure decision core: combines HEAD size state and the ranged answer
/// into [`RangeSupport`] and a trusted expected size.
///
/// Unit-tested without network for every combination the server modes can
/// produce (honored, ignored, broken, contradictory, unknown-size, …).
fn classify(
    head_size: ContentLength,
    answer: RangeAnswer,
    get_meta: &ResponseMeta,
) -> (RangeSupport, Option<u64>) {
    let head_known = match head_size {
        ContentLength::Valid(size) => Some(size),
        _ => None,
    };
    let get_known = match get_meta.cl {
        ContentLength::Valid(size) => Some(size),
        _ => None,
    };
    match answer {
        RangeAnswer::Exact { total } => match (head_known, total) {
            // HEAD and Content-Range agree.
            (Some(head), Some(t)) if head == t => (RangeSupport::Yes, Some(head)),
            // HEAD contradicts the ranged representation: trust neither.
            (Some(_), Some(_)) => (RangeSupport::Unknown, None),
            // HEAD stated a size the server chose not to repeat (`*` form).
            (Some(head), None) => (RangeSupport::Yes, Some(head)),
            // HEAD was inconclusive; the Content-Range total is evidence.
            (None, Some(t)) => (RangeSupport::Yes, Some(t)),
            (None, None) => (RangeSupport::Yes, None),
        },
        RangeAnswer::Ignored => {
            // The server ignored the range: ranges do not work, but size
            // metadata (HEAD's, or the full GET's when HEAD was silent)
            // can still hold when the two agree.
            let size = match (head_known, get_known) {
                (Some(head), Some(get)) if head == get => Some(head),
                (Some(_), Some(_)) => None,
                (head, None) => head,
                (None, get) => get,
            };
            (RangeSupport::No, size)
        }
        RangeAnswer::Unsatisfiable { total } => {
            // Keep HEAD's size unless the server's `*` total contradicts it.
            let size = match total {
                Some(t) if head_known.is_none_or(|head| head == t) => Some(t),
                Some(_) => None,
                None => head_known,
            };
            (RangeSupport::Unknown, size)
        }
        // Malformed or contradictory metadata: trust nothing here.
        RangeAnswer::Contradicted => (RangeSupport::Unknown, None),
        // The ranged request itself failed for an unrelated reason (server
        // error, unusual status): keep the HEAD-side facts, no range claim.
        RangeAnswer::Other => (RangeSupport::Unknown, head_known),
    }
}

/// Probes one URL and records only response-backed facts.
pub(crate) async fn probe(client: &HttpClient, url: &str) -> Result<ProbeResult, AppError> {
    let base_url = validate_url(url)?;
    tracing::info!("probing {}", redact_url(url));

    // 1. Primary metadata request: HEAD.
    let head_response = client.send(Method::HEAD, &base_url, None).await?;
    let head_status = head_response.status();
    tracing::debug!("HEAD answered {}", head_status.as_u16());

    if !head_status.is_success() {
        // 405 is the signal that HEAD is simply unsupported; 501 marks a
        // server (e.g. proxies) that does not implement it. Other statuses
        // (404, 403, …) are genuine answers about the resource.
        if matches!(
            head_status,
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            tracing::debug!(
                "HEAD is unsupported for {}; falling back to one bounded ranged request",
                redact_url(url)
            );
            return probe_by_ranged_get(client, &head_response.url().clone()).await;
        }
        let redacted = redact_url(base_url.as_str());
        return Err(http_status_error(
            head_status,
            format!("the remote server responded HTTP {head_status} for {redacted}"),
        ));
    }

    // 2. HEAD succeeded: gather the advertised metadata.
    let head_meta = response_meta(&head_response);
    let (head_etag, head_modified, effective_url) = response_identity(&head_response);
    let head_effective = head_response.url().clone();
    // head_response is abandoned here: a HEAD body carries no bytes worth
    // keeping, and no probe response body is ever read.

    // Empty resource: no range probe can be meaningful (any range of a
    // zero-length representation is unsatisfiable).
    if let ContentLength::Valid(0) = head_meta.cl {
        tracing::debug!("the remote resource is empty (Content-Length 0)");
        return Ok(ProbeResult {
            expected_size: Some(0),
            range: RangeSupport::Unknown,
            etag: head_etag,
            last_modified: head_modified,
            effective_url,
        });
    }

    // 3. Secondary evidence: one bounded ranged request on the proven URL.
    let head_size = head_meta.cl;
    let get_response = match client
        .send(Method::GET, &head_effective, Some(PROBE_RANGE))
        .await
    {
        Ok(response) => response,
        // The primary request succeeded, so a secondary failure only costs
        // range evidence; it must not erase the HEAD facts. The error's own
        // URL is stripped before the (safe-to-log) reason is recorded.
        Err(err) => {
            // The primary request succeeded, so a secondary failure only
            // costs range evidence; it must not erase the HEAD facts. The
            // typed error is fully log-safe: its message already carries a
            // redacted URL and a transport-only reason.
            tracing::warn!(
                "the range evidence request for {} failed: {} ; ranges recorded as unknown",
                redact_url(effective_url.as_str()),
                err
            );
            return Ok(ProbeResult {
                expected_size: match head_size {
                    ContentLength::Valid(size) => Some(size),
                    _ => None,
                },
                range: RangeSupport::Unknown,
                etag: head_etag,
                last_modified: head_modified,
                effective_url,
            });
        }
    };
    let (answer, get_meta) = classify_ranged_response(&get_response, head_size);
    let (range, expected_size) = classify(head_size, answer, &get_meta);
    if matches!(answer, RangeAnswer::Contradicted) {
        tracing::warn!(
            "ranged response metadata contradicted the request or HEAD for {}; size and ranges recorded as unknown",
            redact_url(effective_url.as_str())
        );
    }
    Ok(ProbeResult {
        expected_size,
        range,
        etag: get_meta.etag.or(head_etag),
        last_modified: get_meta.last_modified.or(head_modified),
        effective_url,
    })
}

/// Probes a resource whose HEAD was rejected (`405`/`501`): the ranged GET
/// becomes the primary request and answers with the current representation.
async fn probe_by_ranged_get(
    client: &HttpClient,
    head_effective: &Url,
) -> Result<ProbeResult, AppError> {
    let response = client
        .send(Method::GET, head_effective, Some(PROBE_RANGE))
        .await?;
    let status = response.status();
    tracing::debug!("the ranged fallback GET answered {}", status.as_u16());
    if !status.is_success() {
        // The fallback failed too. Whether HEAD or the ranged GET is
        // unusable, there is no probe fact worth reporting: this resource
        // cannot be downloaded through the established paths.
        let redacted = redact_url(head_effective.as_str());
        return Err(http_status_error(
            status,
            format!(
                "the remote server responded HTTP {status} for {redacted} (HEAD and the ranged fallback both rejected)"
            ),
        ));
    }
    let (etag, last_modified, effective_url) = response_identity(&response);
    let (answer, meta) = classify_ranged_response(&response, ContentLength::Missing);
    let (range, expected_size) = classify(ContentLength::Missing, answer, &meta);
    if matches!(answer, RangeAnswer::Contradicted) {
        tracing::warn!(
            "ranged fallback response metadata was contradictory for {}; size and ranges recorded as unknown",
            redact_url(head_effective.as_str())
        );
    }
    Ok(ProbeResult {
        expected_size,
        range,
        etag,
        last_modified,
        effective_url,
    })
}

/// Validates that `url` is a direct HTTP/HTTPS resource.
pub(crate) fn validate_url(url: &str) -> Result<Url, AppError> {
    let parsed = Url::parse(url).map_err(|err| {
        AppError::input(format!("the URL could not be parsed: {url}")).with_source(err)
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(AppError::input(format!(
            "URLs must start with http:// or https:// (got {url})"
        )));
    }
    Ok(parsed)
}

/// Pure parser and classification tests run without any network; the
/// network-bound tests reuse the Phase 2 mock server (see the note in
/// `src/download/mod.rs`).
#[cfg(test)]
mod tests {
    use super::{
        classify, content_length_of, parse_content_length_value, parse_content_range,
        parse_unsatisfiable_total, validate_url, ContentEncoding, ContentLength, RangeAnswer,
        RangeSpec, ResponseMeta,
    };
    use crate::common::fixture::Fixture;
    use crate::common::server::{Behavior, MockServer, RangeMode, Validators};
    use crate::download::http::{HttpClient, HttpTimeouts};
    use crate::download::probe::probe;
    use crate::download::probe::RangeSupport;
    use crate::error::ErrorCategory;
    use reqwest::header::{HeaderMap, HeaderValue, CONTENT_LENGTH};
    use std::time::Duration;

    const DEFAULT_LEN: u64 = 4_096;

    /// A production-default client for network tests.
    fn client() -> HttpClient {
        HttpClient::default_client().expect("default client builds")
    }

    /// A client with a shortened overall timeout for timeout tests.
    fn quick_client(ms: u64) -> HttpClient {
        HttpClient::new(HttpTimeouts {
            connect: Duration::from_secs(5),
            metadata: Duration::from_millis(ms),
            body_idle: Duration::from_secs(30),
        })
        .expect("custom timeouts build a client")
    }

    fn meta(cl: ContentLength) -> ResponseMeta {
        ResponseMeta {
            cl,
            etag: None,
            last_modified: None,
            encoding: ContentEncoding::Identity,
        }
    }

    fn cl_map(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(
                CONTENT_LENGTH,
                HeaderValue::from_str(value).expect("test header value"),
            );
        }
        map
    }

    // -- parse_content_length_value ------------------------------------------------------

    #[test]
    fn content_length_values_parse_strictly() {
        assert_eq!(
            parse_content_length_value("4096"),
            ContentLength::Valid(4_096)
        );
        assert_eq!(
            parse_content_length_value("  4096  "),
            ContentLength::Valid(4_096),
            "surrounding whitespace is tolerable and trimmed"
        );
        assert_eq!(parse_content_length_value("0"), ContentLength::Valid(0));
        assert_eq!(parse_content_length_value(""), ContentLength::Invalid);
        assert_eq!(parse_content_length_value("   "), ContentLength::Invalid);
        assert_eq!(parse_content_length_value("+4096"), ContentLength::Invalid);
        assert_eq!(parse_content_length_value("-4096"), ContentLength::Invalid);
        assert_eq!(parse_content_length_value("4 096"), ContentLength::Invalid);
        assert_eq!(parse_content_length_value("banana"), ContentLength::Invalid);
        assert_eq!(
            parse_content_length_value("99999999999999999999999"),
            ContentLength::Invalid,
            "integral overflow is not a trustworthy fact"
        );
    }

    #[test]
    fn missing_conflicting_or_malformed_content_length_is_not_a_fact() {
        // No header at all.
        assert_eq!(content_length_of(&cl_map(&[])), ContentLength::Missing);
        // A single valid value.
        assert_eq!(
            content_length_of(&cl_map(&["4096"])),
            ContentLength::Valid(4_096)
        );
        // Identical duplicates stay valid (harmlessly repeated header).
        assert_eq!(
            content_length_of(&cl_map(&["4096", "4096"])),
            ContentLength::Valid(4_096)
        );
        // Conflicting duplicates are never trusted.
        assert_eq!(
            content_length_of(&cl_map(&["4096", "8192"])),
            ContentLength::Invalid
        );
        // One malformed duplicate poisons the whole header set.
        assert_eq!(
            content_length_of(&cl_map(&["4096", "banana"])),
            ContentLength::Invalid
        );
        // A single malformed value.
        assert_eq!(
            content_length_of(&cl_map(&["banana"])),
            ContentLength::Invalid
        );
    }

    #[test]
    fn non_utf8_content_length_is_invalid() {
        let mut map = HeaderMap::new();
        map.insert(
            CONTENT_LENGTH,
            HeaderValue::from_bytes(b"4\xff9").expect("bytes header"),
        );
        assert_eq!(content_length_of(&map), ContentLength::Invalid);
    }

    // -- parse_content_range -------------------------------------------------------------

    #[test]
    fn exact_content_ranges_parse() {
        assert_eq!(
            parse_content_range("bytes 0-0/4096"),
            Some(RangeSpec {
                first: 0,
                last: 0,
                total: Some(4_096),
            })
        );
        assert_eq!(
            parse_content_range("bytes 100-199/8192"),
            Some(RangeSpec {
                first: 100,
                last: 199,
                total: Some(8_192),
            })
        );
        assert_eq!(
            parse_content_range("bytes 0-499/*"),
            Some(RangeSpec {
                first: 0,
                last: 499,
                total: None,
            }),
            "the `*`-total form is legal and means an unknown total"
        );
    }

    #[test]
    fn malformed_or_unitless_content_ranges_are_rejected() {
        assert_eq!(
            parse_content_range("bytes */4096"),
            None,
            "the 416 form is not a 206 fact"
        );
        assert_eq!(parse_content_range("items 0-0/5"), None);
        assert_eq!(parse_content_range("bytes 0-0"), None);
        assert_eq!(parse_content_range("bytes -0/4096"), None);
        assert_eq!(parse_content_range("bytes 0-1e3/4096"), None);
        assert_eq!(parse_content_range("bytes 0-0/"), None);
        assert_eq!(parse_content_range("garbage"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn unsatisfiable_totals_parse_only_the_416_form() {
        assert_eq!(parse_unsatisfiable_total("bytes */0"), Some(0));
        assert_eq!(parse_unsatisfiable_total("bytes */4096"), Some(4_096));
        assert_eq!(parse_unsatisfiable_total("bytes 0-0/4096"), None);
        assert_eq!(parse_unsatisfiable_total("bytes */"), None);
        assert_eq!(parse_unsatisfiable_total("bytes */1e3"), None);
        assert_eq!(parse_unsatisfiable_total("garbage"), None);
    }

    // -- classify: the decision matrix ---------------------------------------------------

    #[test]
    fn exact_206_with_agreeing_metadata_proves_range_support() {
        // HEAD and Content-Range agree.
        assert_eq!(
            classify(
                ContentLength::Valid(4_096),
                RangeAnswer::Exact { total: Some(4_096) },
                &meta(ContentLength::Valid(1)),
            ),
            (RangeSupport::Yes, Some(4_096))
        );
        // HEAD stated a size, server wrote `bytes 0-0/*`.
        assert_eq!(
            classify(
                ContentLength::Valid(4_096),
                RangeAnswer::Exact { total: None },
                &meta(ContentLength::Valid(1)),
            ),
            (RangeSupport::Yes, Some(4_096))
        );
        // HEAD was silent/inaccurate without contradiction; the interval total is evidence.
        assert_eq!(
            classify(
                ContentLength::Missing,
                RangeAnswer::Exact { total: Some(4_096) },
                &meta(ContentLength::Valid(1)),
            ),
            (RangeSupport::Yes, Some(4_096))
        );
        assert_eq!(
            classify(
                ContentLength::Invalid,
                RangeAnswer::Exact { total: Some(4_096) },
                &meta(ContentLength::Valid(1)),
            ),
            (RangeSupport::Yes, Some(4_096)),
            "an unusable HEAD length is absent evidence, not contradictory evidence"
        );
        // HEAD silent and no total stated: ranges proven, size stays unknown.
        assert_eq!(
            classify(
                ContentLength::Missing,
                RangeAnswer::Exact { total: None },
                &meta(ContentLength::Valid(1)),
            ),
            (RangeSupport::Yes, None)
        );
    }

    #[test]
    fn contradictory_sizes_are_never_silently_resolved() {
        assert_eq!(
            classify(
                ContentLength::Valid(4_096),
                RangeAnswer::Exact { total: Some(8_192) },
                &meta(ContentLength::Valid(1)),
            ),
            (RangeSupport::Unknown, None),
            "HEAD size vs Content-Range total conflict: trust neither"
        );
    }

    #[test]
    fn an_ignored_range_is_never_misclassified_as_support() {
        // HEAD and the full GET agree on size.
        assert_eq!(
            classify(
                ContentLength::Valid(4_096),
                RangeAnswer::Ignored,
                &meta(ContentLength::Valid(4_096)),
            ),
            (RangeSupport::No, Some(4_096))
        );
        // The full GET contradicts the HEAD.
        assert_eq!(
            classify(
                ContentLength::Valid(4_096),
                RangeAnswer::Ignored,
                &meta(ContentLength::Valid(8_192)),
            ),
            (RangeSupport::No, None)
        );
        // HEAD was silent; the full GET's Content-Length still counts.
        assert_eq!(
            classify(
                ContentLength::Missing,
                RangeAnswer::Ignored,
                &meta(ContentLength::Valid(4_096)),
            ),
            (RangeSupport::No, Some(4_096))
        );
        assert_eq!(
            classify(
                ContentLength::Missing,
                RangeAnswer::Ignored,
                &meta(ContentLength::Missing),
            ),
            (RangeSupport::No, None)
        );
    }

    #[test]
    fn unsatisfiable_and_other_answers_keep_honest_states() {
        // 416 with a stated total: the total is a fact, ranges are unusable.
        assert_eq!(
            classify(
                ContentLength::Missing,
                RangeAnswer::Unsatisfiable { total: Some(0) },
                &meta(ContentLength::Missing),
            ),
            (RangeSupport::Unknown, Some(0))
        );
        // 416 without a total keeps HEAD's size.
        assert_eq!(
            classify(
                ContentLength::Valid(1_024),
                RangeAnswer::Unsatisfiable { total: None },
                &meta(ContentLength::Missing),
            ),
            (RangeSupport::Unknown, Some(1_024))
        );
        // A contradictory `bytes */<total>` erases trust.
        assert_eq!(
            classify(
                ContentLength::Valid(1_024),
                RangeAnswer::Unsatisfiable { total: Some(2_048) },
                &meta(ContentLength::Missing),
            ),
            (RangeSupport::Unknown, None)
        );
        // Any other status: HEAD facts survive, ranges unknown.
        assert_eq!(
            classify(
                ContentLength::Valid(4_096),
                RangeAnswer::Other,
                &meta(ContentLength::Missing),
            ),
            (RangeSupport::Unknown, Some(4_096))
        );
        // Contradicted: trust nothing.
        assert_eq!(
            classify(
                ContentLength::Valid(4_096),
                RangeAnswer::Contradicted,
                &meta(ContentLength::Valid(1)),
            ),
            (RangeSupport::Unknown, None)
        );
    }

    // -- validate_url -------------------------------------------------------------------

    #[test]
    fn http_and_https_schemes_are_accepted() {
        assert!(validate_url("http://127.0.0.1:8080/fixture").is_ok());
        assert!(validate_url("https://example.com/fixtures/fixture.bin").is_ok());
    }

    #[test]
    fn non_http_urls_are_rejected_as_input() {
        let err = validate_url("ftp://example.com/fixture.bin").expect_err("ftp must be rejected");
        assert_eq!(err.category(), ErrorCategory::Input);
        assert!(
            err.message().contains("https://"),
            "the error must explain the accepted schemes: {}",
            err.message()
        );
    }

    #[test]
    fn unparseable_urls_are_rejected_as_input() {
        let err = validate_url("not a url at all").expect_err("garbage must be rejected");
        assert_eq!(err.category(), ErrorCategory::Input);
        assert!(
            err.message().contains("not a url at all"),
            "echoing the caller's own input in the actionable message is intended"
        );
    }

    // -- probe over the controlled HTTP mock server --------------------------------------

    #[tokio::test]
    async fn a_probe_is_exactly_one_head_and_one_ranged_get_with_identity_encoding() {
        let server = MockServer::start().await;
        server.mount_route("/fixture", Behavior::default());
        let url = server.url("/fixture");

        let result = probe(&client(), &url)
            .await
            .expect("default route probes cleanly");

        assert_eq!(result.expected_size, Some(DEFAULT_LEN));
        assert_eq!(result.range, RangeSupport::Yes);
        assert_eq!(result.effective_url, url);

        let log = server.requests();
        assert_eq!(
            log.len(),
            2,
            "bounded probing: one HEAD plus one ranged GET"
        );
        assert_eq!(log[0].method, "HEAD");
        assert_eq!(log[1].method, "GET");
        assert_eq!(log[1].range.as_deref(), Some("bytes=0-0"));
        assert_eq!(log[0].path, "/fixture");
        assert_eq!(log[1].path, "/fixture");
        for request in &log {
            assert_eq!(
                request.headers.get("accept-encoding").map(String::as_str),
                Some("identity"),
                "every probe request must forbid content transformations"
            );
        }
        assert_eq!(server.route_hits("/fixture"), 2);
    }

    #[tokio::test]
    async fn static_validators_are_recorded_verbatim() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default()
                .with_etag(Validators::Static)
                .with_last_modified(Validators::Static),
        );
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe succeeds");

        assert_eq!(
            result.etag.as_deref(),
            Some(format!("\"fixture-1-{DEFAULT_LEN}\"").as_str())
        );
        assert_eq!(
            result.last_modified.as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
    }

    #[tokio::test]
    async fn absent_validators_are_recorded_as_absent() {
        let server = MockServer::start().await;
        server.mount_route("/fixture", Behavior::default());
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe succeeds");

        assert_eq!(result.etag, None);
        assert_eq!(result.last_modified, None);
    }

    #[tokio::test]
    async fn changing_validators_are_observed_across_probes() {
        let fixture = Fixture::default();
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default()
                .with_etag(Validators::Changing)
                .with_last_modified(Validators::Changing),
        );
        let url = server.url("/fixture");

        let first = probe(&client(), &url).await.expect("first probe succeeds");
        let second = probe(&client(), &url).await.expect("second probe succeeds");

        let first_etag = first.etag.expect("changing validators are always supplied");
        let second_etag = second
            .etag
            .expect("changing validators are always supplied");
        assert_ne!(
            first_etag, second_etag,
            "the remote object must be seen changing"
        );
        // Size does not drift with validators in this fixture.
        assert_eq!(first.expected_size, Some(fixture.len));
        assert_eq!(second.expected_size, Some(fixture.len));
        assert_eq!(
            first.last_modified.map_or(first_etag.len(), |lm| lm.len()),
            second
                .last_modified
                .map_or(second_etag.len(), |lm| lm.len()),
            "the changing Last-Modified cycle is recorded too (lengths stay stable by design)"
        );
    }

    #[tokio::test]
    async fn redirects_are_followed_and_the_effective_url_is_recorded() {
        let server = MockServer::start().await;
        server.mount_route(
            "/old",
            Behavior::default().with_redirect(301, server.url("/fixture")),
        );
        server.mount_route("/fixture", Behavior::default());
        let url = server.url("/old");

        let result = probe(&client(), &url)
            .await
            .expect("redirect resolves to the file");

        assert_eq!(result.effective_url, server.url("/fixture"));
        assert_eq!(result.expected_size, Some(DEFAULT_LEN));
        assert_eq!(result.range, RangeSupport::Yes);
        // HEAD once on the redirecting route; HEAD + ranged GET on the target.
        assert_eq!(server.route_hits("/old"), 1);
        assert_eq!(server.route_hits("/fixture"), 2);
    }

    #[tokio::test]
    async fn redirect_loops_fail_with_the_redirect_limit() {
        let server = MockServer::start().await;
        server.mount_route(
            "/loop",
            Behavior::default().with_redirect(302, server.url("/loop")),
        );
        let url = server.url("/loop");

        let err = probe(&client(), &url)
            .await
            .expect_err("a self-redirect never resolves");
        assert_eq!(err.category(), ErrorCategory::Http);
        assert!(
            err.message().contains("redirect"),
            "the error must explain the redirect problem: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn redirect_chains_beyond_the_limit_are_rejected() {
        let server = MockServer::start().await;
        // 12 chained hops: strictly more than the configured limit of 10.
        for hop in 0..12 {
            let target = if hop < 11 {
                server.url(&format!("/hop{}", hop + 1))
            } else {
                server.url("/fixture")
            };
            server.mount_route(
                &format!("/hop{hop}"),
                Behavior::default().with_redirect(302, target),
            );
        }
        server.mount_route("/fixture", Behavior::default());

        let err = probe(&client(), &server.url("/hop0"))
            .await
            .expect_err("a >10 hop chain must stop at the redirect limit");
        assert_eq!(err.category(), ErrorCategory::Http);
        assert!(
            err.message().contains("redirect"),
            "the error must name the redirect problem: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn slow_responses_fail_with_the_timeout_error() {
        let server = MockServer::start().await;
        server.mount_route("/slow", Behavior::default().with_delay_headers_ms(2_000));
        let url = server.url("/slow");
        let started = std::time::Instant::now();

        let err = probe(&quick_client(250), &url)
            .await
            .expect_err("a 2s header delay cannot survive a 250ms budget");
        let elapsed = started.elapsed();

        assert_eq!(err.category(), ErrorCategory::Http);
        assert!(
            err.message().contains("did not answer in time"),
            "the timeout must be reported as such: {}",
            err.message()
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the probe must fail bounded, not wait out the server delay (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn head_unsupported_405_falls_back_to_one_ranged_get() {
        let server = MockServer::start().await;
        server.mount_route("/fixture", Behavior::default().with_head_status(405));
        let url = server.url("/fixture");

        let result = probe(&client(), &url)
            .await
            .expect("the fallback probed the file");

        assert_eq!(result.expected_size, Some(DEFAULT_LEN));
        assert_eq!(result.range, RangeSupport::Yes);
        assert_eq!(result.effective_url, url);
        let log = server.requests();
        assert_eq!(log.len(), 2, "HEAD, then exactly one ranged GET");
        assert_eq!(log[0].method, "HEAD");
        assert_eq!(log[1].method, "GET");
        assert_eq!(log[1].range.as_deref(), Some("bytes=0-0"));
    }

    #[tokio::test]
    async fn head_unsupported_501_falls_back_identically() {
        let server = MockServer::start().await;
        server.mount_route("/fixture", Behavior::default().with_head_status(501));
        let url = server.url("/fixture");

        let result = probe(&client(), &url)
            .await
            .expect("the fallback probed the file");

        assert_eq!(result.expected_size, Some(DEFAULT_LEN));
        assert_eq!(result.range, RangeSupport::Yes);
    }

    #[tokio::test]
    async fn a_lying_head_content_length_is_distrusted() {
        let server = MockServer::start().await;
        // HEAD claims 4_103 while the ranged representation says 4_096.
        server.mount_route(
            "/fixture",
            Behavior::default().with_head_length(DEFAULT_LEN + 7),
        );
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(
            result.range,
            RangeSupport::Unknown,
            "no range claim on contradictions"
        );
        assert_eq!(
            result.expected_size, None,
            "neither the lying HEAD nor the total is trusted"
        );
    }

    #[tokio::test]
    async fn an_omitted_head_content_length_lets_the_content_range_speak() {
        let server = MockServer::start().await;
        server.mount_route("/fixture", Behavior::default().with_head_length_omitted());
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(result.range, RangeSupport::Yes);
        assert_eq!(result.expected_size, Some(DEFAULT_LEN));
    }

    #[tokio::test]
    async fn a_failing_ranged_request_degrades_to_unknown_ranges_but_keeps_head_size() {
        let server = MockServer::start().await;
        for code in [500, 404] {
            let path = format!("/code{code}");
            server.mount_route(&path, Behavior::default().with_get_status(code));
            let url = server.url(&path);

            let result = probe(&client(), &url)
                .await
                .expect("HEAD facts survive a failed GET");

            assert_eq!(result.range, RangeSupport::Unknown);
            assert_eq!(result.expected_size, Some(DEFAULT_LEN));
        }
    }

    #[tokio::test]
    async fn a_range_ignoring_server_is_never_classified_as_supporting_ranges() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default().with_range(RangeMode::Ignore),
        );
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(result.range, RangeSupport::No);
        assert_eq!(result.expected_size, Some(DEFAULT_LEN));
    }

    #[tokio::test]
    async fn mismatched_range_headers_produce_unknown_not_support() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default().with_range(RangeMode::BrokenHeaders),
        );
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(
            result.range,
            RangeSupport::Unknown,
            "a 206 whose Content-Range answers a different interval is not range evidence"
        );
        assert_eq!(
            result.expected_size, None,
            "its claimed total (length + 7) contradicts the HEAD and is not trusted"
        );
    }

    #[tokio::test]
    async fn header_consistent_answers_are_accepted_even_when_body_bytes_cannot_be_judged() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default().with_range(RangeMode::BrokenBody),
        );
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe completes");

        // Deliberate boundary of the probe phase: `BrokenBody` serves a 206
        // whose headers are entirely self-consistent (`bytes 0-0/4096` +
        // `Content-Length: 1`), but the single body byte is from a different
        // fixture. No header can detect the substitution, so the probe
        // records range support "Yes" — content authentication is the
        // transfer phase's validation job (Phase 4/8), which compares bytes
        // against expectations instead of trusting headers.
        assert_eq!(result.range, RangeSupport::Yes);
        assert_eq!(result.expected_size, Some(DEFAULT_LEN));
    }

    #[tokio::test]
    async fn a_content_transform_invalidates_range_evidence() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default().with_content_encoding("gzip"),
        );
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(
            result.range,
            RangeSupport::Unknown,
            "a transformed body cannot be addressed by entity byte offsets"
        );
        assert_eq!(result.expected_size, None);
    }

    #[tokio::test]
    async fn an_explicit_identity_encoding_is_normal_evidence() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default().with_content_encoding("identity"),
        );
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(result.range, RangeSupport::Yes);
        assert_eq!(result.expected_size, Some(DEFAULT_LEN));
    }

    #[tokio::test]
    async fn a_206_without_content_length_is_tolerated_via_content_range() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default().with_omit_content_length(true),
        );
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(result.range, RangeSupport::Yes);
        assert_eq!(result.expected_size, Some(DEFAULT_LEN));
    }

    #[tokio::test]
    async fn unknown_size_is_recorded_honestly_when_no_header_declares_one() {
        let server = MockServer::start().await;
        server.mount_route(
            "/fixture",
            Behavior::default()
                .with_range(RangeMode::Ignore)
                .with_omit_content_length(true)
                .with_head_length_omitted(),
        );
        let url = server.url("/fixture");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(result.range, RangeSupport::No);
        assert_eq!(
            result.expected_size, None,
            "no dependable Content-Length anywhere: size is unknown, never guessed"
        );
    }

    #[tokio::test]
    async fn a_zero_length_head_short_circuits_the_range_request() {
        let server = MockServer::start().await;
        // `Content-Length: 0` on the HEAD (manual header — hyper omits an
        // exact zero size hint) is a complete size fact for an empty file.
        server.mount_route(
            "/empty",
            Behavior::default()
                .with_fixture(Fixture::new(7, 0))
                .with_head_length(0),
        );
        let url = server.url("/empty");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(result.expected_size, Some(0));
        assert_eq!(
            result.range,
            RangeSupport::Unknown,
            "ranges are meaningless on a zero-length representation"
        );
        let log = server.requests();
        assert_eq!(
            log.len(),
            1,
            "no range request is issued for a HEAD-declared empty resource"
        );
        assert_eq!(log[0].method, "HEAD");
    }

    #[tokio::test]
    async fn an_empty_resource_recovers_its_size_through_the_unsatisfiable_range_answer() {
        let server = MockServer::start().await;
        server.mount_route(
            "/empty",
            Behavior::default().with_fixture(Fixture::new(7, 0)),
        );
        let url = server.url("/empty");

        let result = probe(&client(), &url).await.expect("probe completes");

        assert_eq!(
            result.expected_size,
            Some(0),
            "the `bytes */0` total of the 416 answer is a usable size fact"
        );
        assert_eq!(
            result.range,
            RangeSupport::Unknown,
            "ranges are meaningless on a zero-length representation"
        );
        // HEAD (no Content-Length: hyper omits an exact zero size hint)
        // plus the ranged request that answers 416 with the full length.
        assert_eq!(server.requests().len(), 2);
    }

    #[tokio::test]
    async fn server_error_statuses_are_reported_as_http_errors() {
        let server = MockServer::start().await;
        let url = server.url("/never-mounted");

        let err = probe(&client(), &url)
            .await
            .expect_err("a 404 is a failure, not silence");
        assert_eq!(err.category(), ErrorCategory::Http);
        assert!(
            err.message().contains("404"),
            "the message must name the failing status: {}",
            err.message()
        );
        assert!(
            err.message().contains(&redacted_of(&url)),
            "the URL in the message is the log-safe form: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn connection_failures_are_reported_as_http_errors() {
        let mut server = MockServer::start().await;
        let url = server.url("/fixture");
        server.shutdown_wait().await;

        let err = probe(&client(), &url)
            .await
            .expect_err("nothing listens on the port anymore");
        assert_eq!(err.category(), ErrorCategory::Http);
        assert!(
            err.message().contains("connection"),
            "the error must explain the transport failure: {}",
            err.message()
        );
    }

    /// Log-safe rendering of a URL, mirroring the probe's own discipline.
    fn redacted_of(url: &str) -> String {
        crate::logging::redact_url(url)
    }
}
