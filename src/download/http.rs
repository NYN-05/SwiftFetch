//! The shared HTTP client (Phase 3, extended by Phase 4's streaming policy).
//!
//! Every network operation of the engine goes through one configured
//! [`reqwest::Client`] wrapped by [`HttpClient`] (`DOCS/architecture.md`
//! §5, HTTP Download Engine): an explicit connection timeout, a bounded
//! metadata/header exchange, an idleness bound on streamed bodies, and a
//! consistent redirect policy, with reqwest's connection pool for reuse.
//! No transparent content transformation is possible: no compression
//! feature is enabled, and requests carry `Accept-Encoding: identity` so
//! ranged representations keep byte offsets.
//!
//! Timeout shape (ADR-0015): the bounds are applied by the engine around
//! the request phases instead of as a reqwest client-level overall cap.
//! reqwest 0.12.28 offers no per-request way to *remove* a client-level
//! timeout (`RequestBuilder::timeout` takes a `Duration` only), so a
//! client-level cap would silently limit every large download's whole body
//! to the metadata budget. Instead: `send` bounds connect-through-headers,
//! and [`read_chunk`] bounds each body read by idleness while the total
//! stream length stays unbounded (large downloads are legitimate);
//! `connect_timeout` remains the client's own knob.
//!
//! URLs must never be logged raw; anything written to diagnostics goes
//! through [`crate::logging::redact_url`] (see [`http_error`], which
//! documents exactly where a raw URL from `reqwest`'s error could leak).

// Phase wiring note: commands are wired in Phase 11; until then these items
// are exercised by tests and became load-bearing with the Phase 4 sequential
// transfer. This is the sanctioned narrow `dead_code` waiver (see AGENTS.md).
#![allow(dead_code)]

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use bytes::Bytes;

use crate::error::{AppError, ErrorCategory};
use crate::logging::redact_url;
use reqwest::header::{HeaderValue, ACCEPT_ENCODING, IF_RANGE, RANGE, USER_AGENT};
use reqwest::{Method, StatusCode, Url};

/// Connection timeout mandated by `DOCS/architecture.md` §6.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on the metadata/header exchange (connect through response headers,
/// including the redirect chain). Provisional value — the architecture
/// table fixes only the connection timeout.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// Idleness bound for streamed body reads (Phase 4).
///
/// Every body read must make progress within this window; deliberately,
/// there is no overall body cap (a legitimate download may take as long as
/// it keeps streaming).
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The configured redirect policy: follow, bounded by [`REDIRECT_LIMIT`].
const REDIRECT_LIMIT: usize = 10;

/// The `Range` request header shape used for probing, byte-exact.
pub(crate) const PROBE_RANGE: &str = "bytes=0-0";

/// Timeout bounds of the shared client, in engine-applied phases.
///
/// Tests shorten these to keep timing scenarios fast; production defaults
/// follow the architecture's connection timeout plus provisional exchange
/// bounds. Nothing user-facing reads these values yet (the CLI surface for
/// network tuning belongs to a later phase).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HttpTimeouts {
    pub connect: Duration,
    /// Bound on connect-through-headers for one request.
    pub metadata: Duration,
    /// Idleness bound per body read of a streamed response.
    pub body_idle: Duration,
}

impl Default for HttpTimeouts {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            metadata: HEADER_TIMEOUT,
            body_idle: BODY_IDLE_TIMEOUT,
        }
    }
}

/// The single reusable HTTP client.
///
/// All engine network operations must run through an instance of this
/// wrapper, never through a fresh `reqwest::Client`, so connection reuse
/// and timeout policy stay consistent.
///
/// Cloning shares the one underlying `reqwest::Client` (its connection
/// pool included): that is how the Phase 7 batch hands the same configured
/// client to every worker task without building another one.
#[derive(Clone)]
pub(crate) struct HttpClient {
    client: reqwest::Client,
    timeouts: HttpTimeouts,
    /// Optional `User-Agent` header value applied to every request.
    /// `None` means no custom header (the HTTP library's default).
    user_agent: Option<String>,
}

impl HttpClient {
    /// Builds the configured client (connection reuse via reqwest's pool).
    ///
    /// `user_agent`, when `Some`, is attached as the `User-Agent` header on
    /// every request through `send` / `send_with_validator`.
    pub(crate) fn new(
        timeouts: HttpTimeouts,
        user_agent: Option<String>,
    ) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .redirect(reqwest::redirect::Policy::limited(REDIRECT_LIMIT))
            .build()
            .map_err(|err| {
                AppError::new(
                    ErrorCategory::Http,
                    "the HTTP client could not be initialized",
                )
                .with_source(err)
            })?;
        Ok(Self {
            client,
            timeouts,
            user_agent,
        })
    }

    /// The production default client (no custom `User-Agent`).
    pub(crate) fn default_client() -> Result<Self, AppError> {
        Self::new(HttpTimeouts::default(), None)
    }

    /// One request through the shared client, with `Accept-Encoding:
    /// identity` always set; `range` carries the exact `Range` header value
    /// when a byte range is requested.
    ///
    /// The returned future is bounded by the metadata budget: connecting,
    /// the redirect chain, and response headers must all arrive in time.
    /// Status handling and body reading stay with the caller (probing reads
    /// no bodies; transfers validate status, then stream via [`read_chunk`]).
    pub(crate) async fn send(
        &self,
        method: Method,
        url: &Url,
        range: Option<&str>,
    ) -> Result<reqwest::Response, AppError> {
        self.send_with_validator(method, url, range, None).await
    }

    /// [`HttpClient::send`] plus the conditional-revalidation header for
    /// resume: `if_range` carries the stored validator (`ETag` or raw
    /// `Last-Modified` value) of a ranged request, so a compliant server
    /// answers `206` only when the representation still matches.
    ///
    /// A mocked or revalidating-hostile server may ignore `If-Range`; retry
    /// classification and the engine's own response cross-checks are what
    /// make the answer safe.
    pub(crate) async fn send_with_validator(
        &self,
        method: Method,
        url: &Url,
        range: Option<&str>,
        if_range: Option<&str>,
    ) -> Result<reqwest::Response, AppError> {
        let mut builder = self
            .client
            .request(method, url.as_str())
            .header(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        if let Some(agent) = &self.user_agent {
            builder = builder.header(
                USER_AGENT,
                HeaderValue::from_str(agent)
                    .expect("the configured User-Agent is a valid header value"),
            );
        }
        if let Some(range) = range {
            builder = builder.header(
                RANGE,
                HeaderValue::from_str(range)
                    .expect("internal range header values are always valid ASCII"),
            );
        }
        if let Some(validator) = if_range {
            builder = builder.header(
                IF_RANGE,
                // The resume planner validates representability before the
                // value is ever handed here (unrepresentable validators
                // select a full restart instead of a request).
                HeaderValue::from_str(validator)
                    .expect("the resume planner validated the validator as a header value"),
            );
        }
        match tokio::time::timeout(self.timeouts.metadata, builder.send()).await {
            Ok(response) => response.map_err(http_error),
            Err(_elapsed) => Err(transport_failure_error(
                format!(
                    "the remote server did not answer in time: {}",
                    redact_url(url.as_str())
                ),
                None,
            )),
        }
    }

    /// One body read from a streamed response, bounded by the idle policy.
    ///
    /// `Ok(None)` is the server-defined end of the stream. Streaming the
    /// whole body through these bounded reads is how a multi-gigabyte
    /// download avoids any overall cap while never trusting an idle
    /// connection (ADR-0015). A stalled read is a transport failure, and
    /// the error text is log-safe the same way [`http_error`]'s is.
    pub(crate) async fn read_chunk(
        &self,
        response: &mut reqwest::Response,
    ) -> Result<Option<Bytes>, AppError> {
        match tokio::time::timeout(self.timeouts.body_idle, response.chunk()).await {
            Ok(chunk) => chunk.map_err(http_error),
            Err(_elapsed) => Err(transport_failure_error(
                format!(
                    "the remote server stopped sending data during the transfer: {}",
                    redact_url(response.url().as_str())
                ),
                None,
            )),
        }
    }
}

/// Builds the user-facing HTTP-status failure: the typed category and
/// message plus a classification marker in the hidden source (Phase 6
/// retry classification reads the marker, not the message).
///
/// `headers`, when available, is inspected for a `Content-Type` that looks
/// like HTML (a likely landing page instead of a direct file link) and for
/// a numeric `Retry-After` on 429 responses. Neither is treated as
/// definitive proof: the message uses cautious wording ("may be", "appears
/// to be"), and a non-numeric `Retry-After` (HTTP-date form) is never
/// interpreted into a wait.
pub(crate) fn http_status_error(
    status: StatusCode,
    message: String,
    headers: Option<&reqwest::header::HeaderMap>,
) -> AppError {
    let enhanced = status_specific_message(status, &message, headers);
    // The base message is log-safe by construction (redacted URL), so one
    // structured warn line gives the incident a searchable status record.
    tracing::warn!("{enhanced}");
    let mut marker = HttpFailure::with_status(status.as_u16());
    if status == StatusCode::TOO_MANY_REQUESTS {
        if let Some(seconds) = retry_after_seconds(headers) {
            marker = marker.with_retry_after(seconds);
        }
    }
    AppError::new(ErrorCategory::Http, enhanced).with_source(marker)
}

/// The numeric `Retry-After` value of a response, in seconds.
///
/// Only the delay-seconds form is read; the HTTP-date form carries no
/// canonical wait for the bounded policy to honor, and is therefore left
/// unparsed (`None`).
fn retry_after_seconds(headers: Option<&reqwest::header::HeaderMap>) -> Option<u64> {
    let value = headers?.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse::<u64>().ok()
}

/// Produces a status-specific user-facing message.
///
/// Client errors (4xx) are permanent facts about the URL; server errors
/// (5xx) are transient availability problems. HTML `Content-Type` on an
/// error response suggests a landing page rather than a direct file link.
fn status_specific_message(
    status: StatusCode,
    base: &str,
    headers: Option<&reqwest::header::HeaderMap>,
) -> String {
    let html_note = html_landing_page_note(headers);
    let status_note = match status.as_u16() {
        401 => " authentication is required; check credentials or permissions for this URL".to_owned(),
        403 => " access is forbidden; the server refuses this URL (it may be bot-protected, require permissions, or not allow direct downloads)".to_owned(),
        404 => " not found; check the URL (the file may have been removed or moved)".to_owned(),
        429 => match retry_after_seconds(headers) {
            Some(seconds) => {
                format!(" rate limited; the server suggests retrying after {seconds} seconds")
            }
            None => " rate limited; try again later".to_owned(),
        },
        code if (500..=599).contains(&code) => {
            " server error; this is a transient failure and may be retried automatically".to_owned()
        }
        _ => String::new(),
    };
    format!("{base}{status_note}{html_note}")
}

/// Cautious note when the response `Content-Type` looks like HTML.
///
/// `Content-Type` alone is not definitive proof of a landing page — some
/// file servers mislabel content — so the wording says "may be" rather
/// than asserting it.
fn html_landing_page_note(headers: Option<&reqwest::header::HeaderMap>) -> String {
    let Some(headers) = headers else {
        return String::new();
    };
    let Some(content_type) = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return String::new();
    };
    let lowered = content_type.to_lowercase();
    if lowered.contains("text/html") || lowered.contains("application/xhtml") {
        return "; the response appears to be an HTML page rather than a file download — this URL may be a landing page, not a direct file link."
            .to_owned();
    }
    String::new()
}

/// Whether a response's `Content-Type` claims an HTML media type.
///
/// A header claim is only a lead ("not definitive proof"): the transfer
/// engine pairs this signal with the bounded body inspection in
/// [`bytes_look_like_html`] before refusing a 200 response as a landing
/// page, and a mislabeled non-HTML body downloads normally.
pub(crate) fn content_type_is_html(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(media_type_is_html)
}

/// Whether a media-type string claims an HTML media type (`text/html` or
/// an XHTML type, prefix-tolerant of parameters).
pub(crate) fn media_type_is_html(media_type: &str) -> bool {
    let lowered = media_type.to_lowercase();
    lowered.contains("text/html") || lowered.contains("application/xhtml")
}

/// Whether raw bytes begin like HTML markup at offset 0.
///
/// Anchored at the very start of the body (a UTF-8 BOM or ASCII
/// whitespace prefix is tolerated), case-insensitive, and deliberately
/// narrow: `<?xml` is *not* a marker, because an XML document can be a
/// legitimate file, and only HTML documents open with these signals.
pub(crate) fn bytes_look_like_html(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let bytes = bytes.trim_ascii_start();
    ["<!doctype", "<html", "<!--"].iter().any(|marker| {
        bytes.len() >= marker.len() && bytes[..marker.len()].eq_ignore_ascii_case(marker.as_bytes())
    })
}

/// The refusal emitted when a transfer's `200 OK` body begins like HTML,
/// before any byte reaches disk. The message names the HTTP status and,
/// when the server claimed it, the HTML media type — safe metadata only.
///
/// The category alone carries no transient marker, so this failure is
/// permanent: retrying an unchanged landing page would be futile, and a
/// deliberate `retry` remains available without any promise of access.
pub(crate) fn landing_page_error(redacted: &str, content_type_html: bool) -> AppError {
    let claim = if content_type_html {
        ", and the server claimed an HTML media type"
    } else {
        ""
    };
    let message = format!(
        "the transfer of {redacted} received what looks like an HTML page \
         instead of the file: the server answered HTTP 200 and the response \
         body begins with HTML markup{claim}; this URL probably serves a \
         landing page rather than a direct file link — use the link that \
         points at the file itself"
    );
    AppError::new(ErrorCategory::Http, message)
}

/// Builds a transport-level HTTP failure (timeouts, connection loss,
/// mid-body drops): transient as a class once classified.
fn transport_failure_error(message: String, reason: Option<reqwest::Error>) -> AppError {
    let marker = HttpFailure::transport();
    AppError::new(ErrorCategory::Http, message).with_source(match reason {
        Some(err) => marker.with_reason(err),
        None => marker,
    })
}

/// Diagnostic marker attached as the hidden source of engine-known HTTP
/// failures so Phase 6's retry classification reads structure instead of
/// parsing error messages (`DOCS/phases.md` Phase 6: "bounded retries for
/// eligible transient errors").
///
/// `status` is the HTTP status when the failure came from one; `None` marks
/// transport-level failures (timeouts, connection loss, mid-body drops),
/// which are transient as a class. The optional `reason` keeps the original
/// diagnostic error (e.g. the `reqwest::Error`) reachable in the source
/// chain; the user-facing message never gains it.
#[derive(Debug)]
pub(crate) struct HttpFailure {
    status: Option<u16>,
    /// `Retry-After` in whole seconds, parsed only when the server supplied
    /// a valid numeric value on a 429 response. This is the server's own
    /// suggested wait, carried for the bounded retry policy (429 responses
    /// naming such a wait retry after it; see `retry.rs`).
    retry_after: Option<u64>,
    reason: Option<Box<dyn StdError + Send + Sync>>,
}

impl HttpFailure {
    /// A transport-level failure: no HTTP status was involved.
    pub(crate) fn transport() -> Self {
        Self {
            status: None,
            retry_after: None,
            reason: None,
        }
    }

    /// A failure that names an HTTP status (the server answered).
    pub(crate) fn with_status(status: u16) -> Self {
        Self {
            status: Some(status),
            retry_after: None,
            reason: None,
        }
    }

    /// Attaches a server-suggested wait in seconds to the marker.
    pub(crate) fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    /// Keeps the original diagnostic error behind the marker.
    pub(crate) fn with_reason(mut self, reason: impl StdError + Send + Sync + 'static) -> Self {
        self.reason = Some(Box::new(reason));
        self
    }

    /// The HTTP status when the failure came from one.
    pub(crate) fn status(&self) -> Option<u16> {
        self.status
    }

    /// The server-suggested wait, when one was validly supplied.
    pub(crate) fn retry_after(&self) -> Option<u64> {
        self.retry_after
    }
}

impl fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "HTTP status {status}"),
            None => write!(f, "transport failure"),
        }
    }
}

impl StdError for HttpFailure {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.reason
            .as_deref()
            .map(|err| err as &(dyn StdError + 'static))
    }
}

/// Converts a reqwest failure into a typed application error.
///
/// The user-facing message contains only a redacted URL; `reqwest` embeds
/// the raw request URL in its own `Display`, so that detail lives in the
/// (hidden) diagnostic source instead of the user output.
pub(crate) fn http_error(err: reqwest::Error) -> AppError {
    let reason = if err.is_timeout() {
        "the remote server did not answer in time"
    } else if err.is_redirect() {
        "the server redirected too many times"
    } else if err.is_connect() {
        "the connection to the remote server failed"
    } else if err.is_body() || err.is_decode() {
        "the server response could not be read"
    } else {
        "the request to the remote server failed"
    };
    let message = match err.url().map(|url| redact_url(url.as_str())) {
        Some(redacted) => format!("{reason}: {redacted}"),
        None => reason.to_owned(),
    };
    AppError::new(ErrorCategory::Http, message)
        .with_source(HttpFailure::transport().with_reason(err))
}

#[cfg(test)]
mod tests {
    use super::{
        bytes_look_like_html, content_type_is_html, http_status_error, media_type_is_html,
        retry_after_seconds, transport_failure_error, HttpClient, HttpFailure, HttpTimeouts,
        BODY_IDLE_TIMEOUT, CONNECT_TIMEOUT, HEADER_TIMEOUT,
    };
    use crate::common::fixture::Fixture;
    use crate::common::server::{Behavior, MockServer};
    use crate::error::{AppError, ErrorCategory};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use reqwest::{Method, StatusCode};
    use std::error::Error as _;
    use std::time::{Duration, Instant};

    const DEFAULT_LEN: u64 = 4_096;

    #[test]
    fn timeout_defaults_follow_the_documented_values() {
        assert_eq!(
            HttpTimeouts::default(),
            HttpTimeouts {
                connect: Duration::from_secs(15),
                metadata: Duration::from_secs(30),
                body_idle: Duration::from_secs(30),
            },
        );
        // The constants are the architecture's documented values; changing
        // them without updating this divider of truth must fail loudly.
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(15));
        assert_eq!(HEADER_TIMEOUT, Duration::from_secs(30));
        assert_eq!(BODY_IDLE_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn the_default_client_builds_successfully() {
        let _client = HttpClient::default_client().expect("defaults build a client");
    }

    /// Phase 6 retry classification reads the hidden diagnostic chain, so
    /// the diagnostic sites must keep their `HttpFailure` marker reachable
    /// through `AppError::source` while the user-facing text stays the same.
    #[test]
    fn engine_failures_carry_the_classification_marker_in_the_hidden_source() {
        let status_err = http_status_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the remote server responded HTTP 503 for /x".to_owned(),
            None,
        );
        assert_eq!(status_err.category(), ErrorCategory::Http);
        let marker = status_err
            .source()
            .and_then(|source| source.downcast_ref::<HttpFailure>());
        assert_eq!(marker.map(|failure| failure.status()), Some(Some(503)));

        // Transport failures (the timeout/idle sites) mark no status: the
        // whole class is transient for classification purposes.
        let transport_err = transport_failure_error(
            "the remote server did not answer in time: /x".to_owned(),
            None,
        );
        let marker = transport_err
            .source()
            .and_then(|source| source.downcast_ref::<HttpFailure>());
        assert_eq!(marker.map(|failure| failure.status()), Some(None));

        // The original diagnostic stays reachable two hops deep
        // (`AppError` -> `HttpFailure` -> reason); `http_error` embeds the
        // `reqwest::Error` itself this way.
        let inner = std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer",
        );
        let wrapped = AppError::new(
            ErrorCategory::Http,
            "the request to the remote server failed: /x".to_owned(),
        )
        .with_source(HttpFailure::transport().with_reason(inner));
        let reason = wrapped
            .source()
            .and_then(|source| source.downcast_ref::<HttpFailure>())
            .and_then(|failure| failure.source())
            .and_then(|reason| reason.downcast_ref::<std::io::Error>());
        assert_eq!(
            reason.map(|err| err.kind()),
            Some(std::io::ErrorKind::ConnectionReset)
        );
    }

    /// A body streamed slower than the metadata budget still completes:
    /// the metadata bound covers connect-through-headers only (ADR-0015).
    #[tokio::test]
    async fn streamed_bodies_outlive_the_metadata_budget() {
        let server = MockServer::start().await;
        // The mock streams a default body in 512-byte frames, each delayed:
        // 4_096 / 512 = 8 frames × 200 ms ≈ 1.6 s of body time, far past the
        // 250 ms metadata budget, yet within the 5 s idle budget.
        server.mount_route("/slow-body", Behavior::default().with_delay_body_ms(200));
        let url = server.url("/slow-body");
        let client = HttpClient::new(
            HttpTimeouts {
                connect: Duration::from_secs(5),
                metadata: Duration::from_millis(250),
                body_idle: Duration::from_secs(5),
            },
            None,
        )
        .expect("custom timeouts build a client");

        let send_started = Instant::now();
        let mut response = client
            .send(Method::GET, &url.parse().expect("test url"), None)
            .await
            .expect("headers arrive within the metadata budget");
        assert!(
            send_started.elapsed() < Duration::from_millis(250),
            "the header phase must be bounded by the metadata budget alone ({:?}), \
             which is exactly why a client-level overall cap cannot exist",
            send_started.elapsed()
        );

        let read_started = Instant::now();
        let mut streamed = Vec::new();
        while let Some(chunk) = client
            .read_chunk(&mut response)
            .await
            .expect("idle reads keep progressing")
        {
            streamed.extend_from_slice(&chunk);
        }
        let read_elapsed = read_started.elapsed();

        let reference = Fixture::default().bytes();
        assert_eq!(streamed.len() as u64, DEFAULT_LEN);
        assert_eq!(streamed.as_slice(), reference.as_slice());
        assert!(
            read_elapsed >= Duration::from_millis(800),
            "the body really was streamed across delayed frames, not drained \
             in one buffered burst (body reads took {read_elapsed:?})"
        );
    }

    #[tokio::test]
    async fn an_idle_body_read_is_aborted_at_the_idle_budget() {
        let server = MockServer::start().await;
        // Per-frame delays arm the server-side sleep *after* a frame is
        // handed out, so the first frame is effectively free and the next
        // poll stalls for the whole delay: 900 ms behind a 250 ms budget.
        server.mount_route("/stalled", Behavior::default().with_delay_body_ms(900));
        let url = server.url("/stalled");
        let client = HttpClient::new(
            HttpTimeouts {
                connect: Duration::from_secs(5),
                metadata: Duration::from_secs(5),
                body_idle: Duration::from_millis(250),
            },
            None,
        )
        .expect("custom timeouts build a client");

        let mut response = client
            .send(Method::GET, &url.parse().expect("test url"), None)
            .await
            .expect("headers arrive quickly");

        client
            .read_chunk(&mut response)
            .await
            .expect("the first read is not delayed above the idle budget")
            .expect("a frame of body data arrives free of delay");

        // The second read stalls at the server for the (remaining) armed
        // delay; the idle budget must abort it long before that.
        let started = Instant::now();
        let err = client
            .read_chunk(&mut response)
            .await
            .expect_err("a stalled body read must hit the idle budget");
        let elapsed = started.elapsed();

        assert!(
            err.message().contains("stopped sending data"),
            "the idle abort must be reported as such: {}",
            err.message()
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the read must fail bounded, not wait out the server delay (took {elapsed:?})"
        );
    }

    // -- User-Agent configuration --------------------------------------------------------

    #[tokio::test]
    async fn a_configured_user_agent_is_sent_on_every_request() {
        let mut server = MockServer::start().await;
        server.mount_route("/ua", Behavior::default());
        let client = HttpClient::new(
            HttpTimeouts::default(),
            Some("swiftfetch-test/1.0 (unit-test)".to_owned()),
        )
        .expect("a client with a User-Agent builds");

        client
            .send(
                Method::GET,
                &server.url("/ua").parse().expect("test url"),
                None,
            )
            .await
            .expect("the request completes");

        let requests = server.requests();
        let sent = requests.last().expect("the request was recorded");
        assert_eq!(
            sent.headers.get("user-agent").map(String::as_str),
            Some("swiftfetch-test/1.0 (unit-test)"),
            "the configured User-Agent is carried verbatim: {:?}",
            sent.headers
        );
        // Byte correctness of the answer stays intact beside identity tests.
        assert_eq!(sent.path, "/ua", "the request reached the asked route");
    }

    #[tokio::test]
    async fn no_user_agent_header_is_sent_without_configuration() {
        let mut server = MockServer::start().await;
        server.mount_route("/plain", Behavior::default());
        let client = HttpClient::default_client().expect("the default client builds");

        client
            .send(
                Method::GET,
                &server.url("/plain").parse().expect("test url"),
                None,
            )
            .await
            .expect("the request completes");

        let requests = server.requests();
        let sent = requests.last().expect("the request was recorded");
        assert!(
            !sent.headers.contains_key("user-agent"),
            "absent --user-agent, no User-Agent header is sent (no impersonation by default): {:?}",
            sent.headers
        );
    }

    // -- status-specific diagnostics and Retry-After --------------------------------------

    fn retry_after_map(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(value).expect("test Retry-After value"),
        );
        headers
    }

    #[test]
    fn status_messages_name_the_common_failures_with_next_steps() {
        let base = "the remote server responded HTTP {code} for http://h/f".to_owned();
        let cases: [(u16, &[&str]); 4] = [
            (401, &["401", "authentication"]),
            (403, &["403", "forbidden", "bot-protected"]),
            (404, &["404", "not found"]),
            (521, &["521", "server error"]),
        ];
        for (code, needles) in cases {
            let status = StatusCode::from_u16(code).expect("test status");
            let error = http_status_error(status, base.replace("{code}", &code.to_string()), None);
            for needle in needles {
                assert!(
                    error.message().contains(needle),
                    "the {code} message must contain {needle:?}: {}",
                    error.message()
                );
            }
        }
    }

    #[test]
    fn a_429_message_names_a_valid_retry_after_wait() {
        let status = StatusCode::TOO_MANY_REQUESTS;
        let headers = retry_after_map("12");
        let error = http_status_error(
            status,
            "the remote server responded HTTP 429 for http://h/f".to_owned(),
            Some(&headers),
        );
        assert!(
            error.message().contains("retrying after 12 seconds"),
            "a numeric Retry-After is reported: {}",
            error.message()
        );
        let marker = error
            .source()
            .and_then(|source| source.downcast_ref::<HttpFailure>())
            .expect("the status failure carries its marker");
        assert_eq!(marker.retry_after(), Some(12));
    }

    #[test]
    fn a_non_numeric_retry_after_is_never_interpreted() {
        let status = StatusCode::TOO_MANY_REQUESTS;
        let headers = retry_after_map("Wed, 21 Oct 2026 07:28:00 GMT");
        let error = http_status_error(
            status,
            "the remote server responded HTTP 429 for http://h/f".to_owned(),
            Some(&headers),
        );
        assert!(
            error.message().contains("try again later"),
            "a non-numeric Retry-After is not phrased as seconds: {}",
            error.message()
        );
        let marker = error
            .source()
            .and_then(|source| source.downcast_ref::<HttpFailure>())
            .expect("the marker exists");
        assert_eq!(
            marker.retry_after(),
            None,
            "an HTTP-date Retry-After carries no parsed wait"
        );
    }

    #[test]
    fn retry_after_seconds_parses_numerics_only() {
        assert_eq!(retry_after_seconds(Some(&retry_after_map("7"))), Some(7));
        assert_eq!(retry_after_seconds(Some(&retry_after_map(" 7 "))), Some(7));
        assert_eq!(
            retry_after_seconds(Some(&retry_after_map("Wed, 21 Oct 2026 07:28:00 GMT"))),
            None
        );
        assert_eq!(retry_after_seconds(Some(&retry_after_map("-3"))), None);
        assert_eq!(retry_after_seconds(None), None);
    }

    // -- landing-page evidence ------------------------------------------------------------

    #[test]
    fn html_media_type_detection_is_prefix_tolerant() {
        for accepted in ["text/html", "text/html; charset=utf-8", "application/xhtml+xml"] {
            assert!(
                media_type_is_html(accepted),
                "{accepted} claims HTML"
            );
        }
        for rejected in [
            "application/octet-stream",
            "application/json",
            "image/png",
            "text/csv",
            "text/htmlish-pretend",
        ] {
            assert!(
                !media_type_is_html(rejected),
                "{rejected} does not claim HTML"
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        assert!(content_type_is_html(&headers));
        assert!(!content_type_is_html(&HeaderMap::new()));
    }

    #[test]
    fn html_markup_evidence_is_anchored_and_case_insensitive() {
        for markup in [
            "<!DOCTYPE html><html>",
            "<!doctype html>",
            "<html lang=\"en\">",
            "<!-- server comment --><!DOCTYPE html>",
            &format!("﻿<html>"),
            "\n   <!doctype html>",
        ] {
            assert!(
                bytes_look_like_html(markup.as_bytes()),
                "{markup:?} begins like HTML"
            );
        }
        for plain in [
            "PK\x03\x04zip-bytes",
            "\x7fELF binary",
            "#!/usr/bin/env python",
            "#import <Foundation/Foundation.h>",
            "%PDF-1.7",
            "hello world",
            "<xhtmlist-of-names>",
        ] {
            assert!(
                !bytes_look_like_html(plain.as_bytes()),
                "{plain:?} must not be taken for HTML"
            );
        }
    }

    #[test]
    fn a_landing_page_refusal_is_permanent_and_self_explanatory() {
        let with_claim = super::landing_page_error("http://h/landing", true);
        assert_eq!(with_claim.category(), ErrorCategory::Http);
        assert!(
            with_claim.message().contains("landing page")
                && with_claim.message().contains("direct file link")
                && with_claim.message().contains("HTTP 200")
                && with_claim.message().contains("HTML media type"),
            "the refusal explains what happened and the next step: {}",
            with_claim.message()
        );
        let without_claim = super::landing_page_error("http://h/landing", false);
        assert!(
            !without_claim.message().contains("HTML media type"),
            "the media-type claim is reported only when the server declared it: {}",
            without_claim.message()
        );
    }
}
