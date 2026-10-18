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
use reqwest::header::{HeaderValue, ACCEPT_ENCODING, IF_RANGE, RANGE};
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
pub(crate) struct HttpClient {
    client: reqwest::Client,
    timeouts: HttpTimeouts,
}

impl HttpClient {
    /// Builds the configured client (connection reuse via reqwest's pool).
    pub(crate) fn new(timeouts: HttpTimeouts) -> Result<Self, AppError> {
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
        Ok(Self { client, timeouts })
    }

    /// The production default client.
    pub(crate) fn default_client() -> Result<Self, AppError> {
        Self::new(HttpTimeouts::default())
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
pub(crate) fn http_status_error(status: StatusCode, message: String) -> AppError {
    AppError::new(ErrorCategory::Http, message)
        .with_source(HttpFailure::with_status(status.as_u16()))
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
    reason: Option<Box<dyn StdError + Send + Sync>>,
}

impl HttpFailure {
    /// A transport-level failure: no HTTP status was involved.
    pub(crate) fn transport() -> Self {
        Self {
            status: None,
            reason: None,
        }
    }

    /// A failure that names an HTTP status (the server answered).
    pub(crate) fn with_status(status: u16) -> Self {
        Self {
            status: Some(status),
            reason: None,
        }
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
        http_status_error, transport_failure_error, HttpClient, HttpFailure, HttpTimeouts,
        BODY_IDLE_TIMEOUT, CONNECT_TIMEOUT, HEADER_TIMEOUT,
    };
    use crate::common::fixture::Fixture;
    use crate::common::server::{Behavior, MockServer};
    use crate::error::{AppError, ErrorCategory};
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
        let client = HttpClient::new(HttpTimeouts {
            connect: Duration::from_secs(5),
            metadata: Duration::from_millis(250),
            body_idle: Duration::from_secs(5),
        })
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
        let client = HttpClient::new(HttpTimeouts {
            connect: Duration::from_secs(5),
            metadata: Duration::from_secs(5),
            body_idle: Duration::from_millis(250),
        })
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
}
