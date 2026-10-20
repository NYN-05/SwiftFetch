//! Bounded retry policy for transient transfer errors (Phase 6).
//!
//! `DOCS/phases.md` Phase 6: "Implement a bounded retry policy with
//! exponential backoff and jitter for eligible transient errors. Do not
//! retry permanent errors indefinitely." The policy is deliberately small
//! and explicit:
//!
//! - attempts are bounded once per invocation; the `retry_count` the
//!   download record shows accumulates across invocations instead;
//! - classification reads the hidden diagnostic chain of a typed
//!   [`AppError`] (never the message text): HTTP 500–599 statuses and
//!   transport-level failures (timeouts, connection loss, mid-body drops)
//!   are transient; every other status and every other category
//!   (validation, filesystem, database, cancellation, …) is permanent;
//! - delays grow exponentially from a base toward a cap, and the effective
//!   wait is full-jitter within `[0, nominal]` so simultaneously failing
//!   transfers do not retry in lockstep.
//!
//! Jitter needs randomness without adding a dependency (ADR-0003): a fresh
//! per-process-seeded `std::collections::hash_map::RandomState` mixed
//! through `DefaultHasher` supplies a well-distributed, non-cryptographic
//! salt per call, which is exactly the honesty jitter needs.

#![allow(dead_code)] // wired into production paths from Phase 6 onward

use std::collections::hash_map::RandomState;
use std::error::Error as StdError;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use crate::download::http::HttpFailure;
use crate::error::{AppError, ErrorCategory};

/// Whether one failure deserves a bounded retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryVerdict {
    /// The error is eligible: spend an attempt within the budget.
    Transient,
    /// The error is permanent: fail the transfer, whatever budget remains.
    Permanent,
}

/// The largest server-suggested wait (`Retry-After`) the bounded policy
/// will honor automatically for a 429 response.
///
/// A rate-limit wait is server-directed, so its shape differs from the
/// engine's own backoff: it retries after *exactly* the requested delay
/// (no jitter below the server's floor), but only while that wait stays
/// within this ceiling — "valid and consistent with the configured retry
/// limits". An unbounded server wait would violate the project's
/// no-unbounded-waits rule and freeze a worker slot for arbitrary times;
/// anything above the ceiling is a permanent failure whose message names
/// the suggested wait, for a deliberate manual `retry` later.
pub(crate) const RETRY_AFTER_MAX: Duration = Duration::from_secs(60);

/// Reads the hidden diagnostic chain of a typed error and classifies it.
///
/// The chain is walked structurally: an [`HttpFailure`] marker names either
/// an HTTP status (the server answered) or nothing (transport-level); a
/// wrapped `reqwest::Error` is recognized as transport by its own flags.
/// Anything unmarked is permanent — the bounded policy must never
/// extrapolate "maybe transient" onto an unclassified failure.
///
/// `max_server_wait` bounds how large a server-directed `Retry-After` wait
/// may be for a 429 to still classify as transient; larger suggested waits
/// are permanent, with the wait named in the failure's message.
pub(crate) fn classify_with_delay(
    err: &AppError,
    max_server_wait: Duration,
) -> (RetryVerdict, Option<Duration>) {
    // Shutdown is never an error to retry: stopping *is* the requested work.
    if err.category() == ErrorCategory::Cancelled {
        return (RetryVerdict::Permanent, None);
    }
    if let Some(marker) = chained::<HttpFailure>(err) {
        return match marker.status() {
            // A 429 that names a valid numeric wait retries after exactly
            // that wait (the server's floor, honored verbatim), while the
            // wait itself is within the configured bound.
            Some(429)
                if marker
                    .retry_after()
                    .is_some_and(|secs| Duration::from_secs(secs) <= max_server_wait) =>
            {
                (
                    RetryVerdict::Transient,
                    marker.retry_after().map(Duration::from_secs),
                )
            }
            // Server-side availability problems are the canonical transient
            // set; every other named answer is a permanent fact about the
            // URL (4xx) or the request that produced it.
            Some(status) if (500..=599).contains(&status) => (RetryVerdict::Transient, None),
            Some(_) => (RetryVerdict::Permanent, None),
            None => (RetryVerdict::Transient, None),
        };
    }
    if let Some(reqwest_error) = chained::<reqwest::Error>(err) {
        let verdict = if reqwest_error.is_timeout()
            || reqwest_error.is_connect()
            || reqwest_error.is_body()
            || reqwest_error.is_decode()
        {
            RetryVerdict::Transient
        } else {
            RetryVerdict::Permanent
        };
        return (verdict, None);
    }
    (RetryVerdict::Permanent, None)
}

/// Reads the hidden diagnostic chain of a typed error and classifies it,
/// with the module's standard bound on server-directed waits.
pub(crate) fn classify(err: &AppError) -> RetryVerdict {
    classify_with_delay(err, RETRY_AFTER_MAX).0
}

/// Walks `err`'s hidden source chain looking for `T`.
fn chained<T: StdError + 'static>(err: &AppError) -> Option<&T> {
    let mut current: &(dyn StdError + 'static) = err.source()?;
    loop {
        if let Some(found) = current.downcast_ref::<T>() {
            return Some(found);
        }
        current = current.source()?;
    }
}

/// The bounded retry budget and backoff shape for one transfer invocation.
///
/// The defaults follow `DOCS/architecture.md` §6 ("retry attempts 5", where
/// the number names *retries*; 0 means one attempt, no retries) with
/// provisional backoff constants in the same spirit as Phase 4's timing
/// bounds: the architecture fixes the policy's existence, not its numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetryPolicy {
    /// Retries allowed after the first attempt (0 = one attempt, then stop).
    pub(crate) max_retries: u32,
    /// The wait before the first retry.
    pub(crate) base_delay: Duration,
    /// The wait multiplier between consecutive retries (intended shape: ≥ 2).
    pub(crate) factor: u32,
    /// The upper bound of any single wait, whatever the exponent computes.
    pub(crate) max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 5,
            base_delay: Duration::from_millis(500),
            factor: 2,
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Builds the policy from the configured retry-attempt count.
    pub(crate) fn from_retries(retries: u32) -> Self {
        Self {
            max_retries: retries,
            ..Self::default()
        }
    }

    /// The total attempt budget for one invocation (first attempt included).
    pub(crate) const fn attempt_budget(&self) -> u32 {
        self.max_retries.saturating_add(1)
    }

    /// The nominal (pre-jitter) wait before retry number `attempt`
    /// (0-based). Pure and test-locked: `base × factor^attempt`, saturated
    /// at [`RetryPolicy::max_delay`].
    pub(crate) fn nominal_delay(&self, attempt: u32) -> Duration {
        let mut delay = self.base_delay;
        for _ in 0..attempt {
            if delay >= self.max_delay {
                break;
            }
            delay = delay.saturating_mul(self.factor);
        }
        delay.min(self.max_delay)
    }

    /// The effective wait before retry number `attempt`: full jitter over
    /// the nominal delay — uniformly sampled within `[0, nominal]` — so
    /// simultaneously failing transfers do not synchronize their retries.
    pub(crate) fn delay_for(&self, attempt: u32) -> Duration {
        let nominal = self.nominal_delay(attempt);
        if nominal.is_zero() {
            return nominal;
        }
        // Non-cryptographic but well-distributed: per-process random keys
        // (std's `RandomState`) hash an empty stream into a per-call salt.
        // No randomness crate is added for jitter (ADR-0003).
        let salt = RandomState::new().build_hasher().finish();
        let fraction = (salt % 1_000_000) as f64 / 1_000_000.0;
        Duration::from_secs_f64(nominal.as_secs_f64() * fraction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport_error() -> AppError {
        // The shape the timeout/idle diagnostic sites produce: a hidden
        // marker with no status.
        AppError::new(
            ErrorCategory::Http,
            "the remote server did not answer in time: /x".to_owned(),
        )
        .with_source(HttpFailure::transport())
    }

    fn status_error(status: u16) -> AppError {
        let code = reqwest::StatusCode::from_u16(status).expect("test status");
        crate::download::http::http_status_error(
            code,
            format!("the remote server responded HTTP {code} for /x"),
            None,
        )
    }

    fn plain(category: ErrorCategory) -> AppError {
        AppError::new(category, "a plain categorized failure".to_owned())
    }

    #[test]
    fn transient_transport_and_server_outages_are_retryable() {
        assert_eq!(classify(&transport_error()), RetryVerdict::Transient);
        for status in [500, 502, 503, 504, 599] {
            assert_eq!(
                classify(&status_error(status)),
                RetryVerdict::Transient,
                "HTTP {status} is a server-side availability failure"
            );
        }
    }

    #[test]
    fn client_statuses_and_every_other_category_are_permanent() {
        for status in [400, 403, 404, 409, 410, 416, 429, 451] {
            assert_eq!(
                classify(&status_error(status)),
                RetryVerdict::Permanent,
                "HTTP {status} is not an availability failure"
            );
        }
        for category in [
            ErrorCategory::Input,
            ErrorCategory::Filesystem,
            ErrorCategory::Database,
            ErrorCategory::RangeValidation,
        ] {
            assert_eq!(
                classify(&plain(category)),
                RetryVerdict::Permanent,
                "an unmarked {category:?} failure is never extrapolated to transient"
            );
        }
    }

    #[test]
    fn shutdown_is_never_classified_as_retryable() {
        let cancelled = AppError::new(
            ErrorCategory::Cancelled,
            "shutdown was requested during the transfer of /x; the persisted \
             download is paused and resumable"
                .to_owned(),
        );
        assert_eq!(classify(&cancelled), RetryVerdict::Permanent);
    }

    /// Deep chains keep the marker findable: any intermediary that keeps
    /// the hidden source (as `with_transfer_context` does with its wrapped
    /// original) must not hide eligibility from classification.
    #[test]
    fn classification_reaches_the_marker_through_wrappers() {
        let wrapped = AppError::new(
            ErrorCategory::Http,
            "the transfer of /x stopped: 512 bytes were written to the partial \
             file; the remote server responded HTTP 503 for /x"
                .to_owned(),
        )
        .with_source(status_error(503));
        assert_eq!(classify(&wrapped), RetryVerdict::Transient);

        let permanent_wrapped = AppError::new(
            ErrorCategory::RangeValidation,
            "the transfer of /x stopped: 512 bytes were written to the partial \
             file; a plain categorized failure"
                .to_owned(),
        )
        .with_source(plain(ErrorCategory::RangeValidation));
        assert_eq!(classify(&permanent_wrapped), RetryVerdict::Permanent);
    }

    /// A bare `reqwest::Error` (no `HttpFailure` marker, e.g. a probe error
    /// that surfaced before any marker-wrapping path) is recognized by its
    /// own transport flags. Constructing its shape honestly: a connection
    /// error through a real failed connect against an unused local port —
    /// no external HTTP exchange ever happens.
    #[tokio::test]
    async fn unmarked_reqwest_transport_errors_classify_by_their_flags() {
        let client = reqwest::Client::builder().build().expect("plain client");
        // Port 1 on loopback: connection refused locally, no server at all.
        let result = client
            .get(reqwest::Url::parse("http://127.0.0.1:1/no-server").expect("test url"))
            .send()
            .await;
        let reqwest_err = match result {
            Err(err) => err,
            Ok(_) => panic!("nothing is listening on port 1 of loopback"),
        };
        let plain_http = AppError::new(
            ErrorCategory::Http,
            "the connection to the remote server failed: 127.0.0.1:1".to_owned(),
        )
        .with_source(reqwest_err);
        assert_eq!(classify(&plain_http), RetryVerdict::Transient);
    }

    #[test]
    fn the_attempt_budget_is_retries_plus_one() {
        assert_eq!(RetryPolicy::from_retries(0).attempt_budget(), 1);
        assert_eq!(RetryPolicy::from_retries(5).attempt_budget(), 6);
        // Defaults carry the architecture's retry count.
        assert_eq!(RetryPolicy::default(), RetryPolicy::from_retries(5));
    }

    #[test]
    fn nominal_backoff_grows_exponentially_and_saturates_at_the_cap() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.nominal_delay(0), Duration::from_millis(500));
        assert_eq!(policy.nominal_delay(1), Duration::from_millis(1_000));
        assert_eq!(policy.nominal_delay(2), Duration::from_millis(2_000));
        assert_eq!(policy.nominal_delay(3), Duration::from_millis(4_000));
        assert_eq!(policy.nominal_delay(4), Duration::from_secs(8));
        assert_eq!(
            policy.nominal_delay(100),
            Duration::from_secs(8),
            "the exponent saturates at the cap instead of growing without bound"
        );
    }

    #[test]
    fn effective_jitter_never_exceeds_the_nominal_delay() {
        let policy = RetryPolicy::default();
        for attempt in 0..=4 {
            let nominal = policy.nominal_delay(attempt);
            for _ in 0..64 {
                assert!(
                    policy.delay_for(attempt) <= nominal,
                    "jitter stays within [0, nominal] for attempt {attempt}"
                );
            }
        }
    }

    #[test]
    fn jitter_varies_between_calls() {
        let policy = RetryPolicy::default();
        let mut distinct = std::collections::BTreeSet::new();
        for _ in 0..64 {
            distinct.insert(
                policy
                    .delay_for(3)
                    .as_nanos()
                    .min(u64::from(u32::MAX) as u128) as u64,
            );
        }
        assert!(
            distinct.len() > 2,
            "the per-process salt must actually vary between calls ({} distinct)",
            distinct.len()
        );
    }

    /// A two-wrapper chain still finds a marker underneath: any
    /// intermediary that keeps the hidden source (as another wrapper might)
    /// must not hide eligibility from classification.
    #[test]
    fn wrapped_chains_still_find_the_underlying_marker() {
        let inner = AppError::new(
            ErrorCategory::Http,
            "the remote server responded HTTP 500 for /x".to_owned(),
        )
        .with_source(status_error(500));
        let outer =
            AppError::new(ErrorCategory::Http, "outer accounting".to_owned()).with_source(inner);
        let marker = chained::<HttpFailure>(&outer);
        assert_eq!(marker.and_then(|failure| failure.status()), Some(500));
        assert_eq!(classify(&outer), RetryVerdict::Transient);
    }

    #[test]
    fn markers_keep_their_reason_walkable() {
        let marker =
            HttpFailure::transport().with_reason(std::io::Error::other("connection reset by peer"));
        let reason = (marker.source()).and_then(|source| source.downcast_ref::<std::io::Error>());
        assert!(reason.is_some(), "the reason hop survives in the chain");
    }

    // -- 429 with Retry-After ------------------------------------------------------------

    /// A genuine 429 as the HTTP layer emits it, with the given raw value.
    fn rate_limited_error(retry_after: Option<&str>) -> AppError {
        let headers = retry_after.map(|value| {
            let mut map = reqwest::header::HeaderMap::new();
            map.insert(
                reqwest::header::RETRY_AFTER,
                reqwest::header::HeaderValue::from_str(value).expect("test header value"),
            );
            map
        });
        let code = reqwest::StatusCode::TOO_MANY_REQUESTS;
        crate::download::http::http_status_error(
            code,
            format!("the remote server responded HTTP {code} for /x"),
            headers.as_ref(),
        )
    }

    #[test]
    fn a_429_with_a_bounded_retry_after_is_transient_with_that_exact_delay() {
        let (verdict, delay) = classify_with_delay(&rate_limited_error(Some("12")), RETRY_AFTER_MAX);
        assert_eq!(verdict, RetryVerdict::Transient);
        assert_eq!(
            delay,
            Some(Duration::from_secs(12)),
            "the server's floor is honored verbatim, not jittered"
        );

        // The ceiling itself stays acceptable.
        let at_ceiling = RETRY_AFTER_MAX.as_secs().to_string();
        let (verdict, delay) =
            classify_with_delay(&rate_limited_error(Some(&at_ceiling)), RETRY_AFTER_MAX);
        assert_eq!(verdict, RetryVerdict::Transient);
        assert_eq!(delay, Some(RETRY_AFTER_MAX));
    }

    #[test]
    fn a_retry_after_above_the_bound_is_a_permanent_failure() {
        for bound in [RETRY_AFTER_MAX, Duration::from_secs(5)] {
            let over = bound.as_secs() + 1;
            let (verdict, delay) =
                classify_with_delay(&rate_limited_error(Some(&over.to_string())), bound);
            assert_eq!(
                verdict, RetryVerdict::Permanent,
                "a {over}s server wait must not freeze a worker slot automatically"
            );
            assert_eq!(delay, None, "no delay is spent on a permanent verdict");
        }
    }

    #[test]
    fn a_429_without_a_usable_retry_after_stays_permanent() {
        // No header at all: the Phase 6 contract holds.
        assert_eq!(
            classify_with_delay(&rate_limited_error(None), RETRY_AFTER_MAX),
            (RetryVerdict::Permanent, None)
        );
        // HTTP-date form: no canonical wait, no automatic retry.
        assert_eq!(
            classify_with_delay(&rate_limited_error(Some("Wed, 21 Oct 2026 07:28:00 GMT")), RETRY_AFTER_MAX),
            (RetryVerdict::Permanent, None)
        );
    }
}
