//! The shared HTTP client (Phase 3).
//!
//! Every network operation of the engine goes through one configured
//! [`reqwest::Client`] wrapped by [`HttpClient`] (`DOCS/architecture.md`
//! §5, HTTP Download Engine): explicit connection and overall-request
//! timeouts, a consistent redirect policy, and reqwest's connection pool
//! for reuse. No transparent content transformation is possible: no
//! compression feature is enabled, and requests carry
//! `Accept-Encoding: identity` so ranged representations keep byte offsets.
//!
//! URLs must never be logged raw; anything written to diagnostics goes
//! through [`crate::logging::redact_url`] (see [`http_error`], which
//! documents exactly where a raw URL from `reqwest`'s error could leak).

// Phase 3 wiring note: the CLI dispatch stays not-implemented until the
// transfer-engine phases arrive (Phase 11 wires commands), so these items
// are exercised by tests first and become load-bearing when Phase 4 calls
// into them. This is the sanctioned narrow `dead_code` waiver citing the
// future phase (see AGENTS.md, same pattern as `src/error.rs`).
#![allow(dead_code)]

use std::time::Duration;

use crate::error::{AppError, ErrorCategory};
use crate::logging::redact_url;
use reqwest::header::{HeaderValue, ACCEPT_ENCODING, RANGE};
use reqwest::{Method, Url};

/// Connection timeout mandated by `DOCS/architecture.md` §6.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Overall per-request timeout for probe-phase operations.
///
/// Provisional (the architecture table fixes only the connection timeout);
/// large-body streaming reads will get their own bounded policy in later
/// phases, so this stays a conservative guard for metadata exchanges.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The configured redirect policy: follow, bounded by [`REDIRECT_LIMIT`].
const REDIRECT_LIMIT: usize = 10;

/// The `Range` request header shape used for probing, byte-exact.
pub(crate) const PROBE_RANGE: &str = "bytes=0-0";

/// TCP and total-request timeouts for probe-phase operations.
///
/// Tests shorten these to keep timing scenarios fast; production defaults
/// follow the architecture's connection timeout plus a provisional overall
/// guard. Nothing user-facing reads these values yet (the CLI surface for
/// network tuning belongs to a later phase).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HttpTimeouts {
    pub connect: Duration,
    pub overall: Duration,
}

impl Default for HttpTimeouts {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            overall: REQUEST_TIMEOUT,
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
}

impl HttpClient {
    /// Builds the configured client (connection reuse via reqwest's pool).
    pub(crate) fn new(timeouts: HttpTimeouts) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .timeout(timeouts.overall)
            .redirect(reqwest::redirect::Policy::limited(REDIRECT_LIMIT))
            .build()
            .map_err(|err| {
                AppError::new(
                    ErrorCategory::Http,
                    "the HTTP client could not be initialized",
                )
                .with_source(err)
            })?;
        Ok(Self { client })
    }

    /// The production default client.
    pub(crate) fn default_client() -> Result<Self, AppError> {
        Self::new(HttpTimeouts::default())
    }

    /// One request through the shared client, with `Accept-Encoding:
    /// identity` always set; `range` carries the exact `Range` header value
    /// when a byte range is requested.
    pub(crate) async fn send(
        &self,
        method: Method,
        url: &Url,
        range: Option<&str>,
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
        builder.send().await.map_err(http_error)
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
    AppError::new(ErrorCategory::Http, message).with_source(err)
}

#[cfg(test)]
mod tests {
    use super::{HttpClient, HttpTimeouts, CONNECT_TIMEOUT, REQUEST_TIMEOUT};
    use std::time::Duration;

    #[test]
    fn timeout_defaults_follow_the_documented_values() {
        let timeouts = HttpTimeouts::default();
        assert_eq!(timeouts.connect, Duration::from_secs(15));
        assert_eq!(timeouts.overall, Duration::from_secs(30));
        // The constants are the architecture's documented values; changing
        // them without updating this divider of truth must fail loudly.
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(15));
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn the_default_client_builds_successfully() {
        let _client = HttpClient::default_client().expect("defaults build a client");
    }
}
