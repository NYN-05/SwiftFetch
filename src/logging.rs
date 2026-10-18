//! Structured logging setup and log-safety helpers.
//!
//! Diagnostics are written by `tracing` to **stderr** so that stdout stays
//! reserved for user-facing CLI output (results, progress in later phases).
//! Default terminal output is concise; `--verbose` enables detailed
//! diagnostics.
//!
//! Log safety: URLs and other remote-derived data must never be logged raw.
//! [`redact_url`] strips the query and fragment entirely (the application
//! cannot reliably classify which query parameters carry tokens) and masks
//! userinfo passwords. Every later module that logs a URL must route it
//! through this helper.

use tracing_subscriber::filter::LevelFilter;

/// Installs the global `tracing` subscriber.
///
/// Concise by default; `--verbose` switches to detailed diagnostics.
pub fn init(verbose: bool) {
    let level = level_filter(verbose);
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(level)
        .init();
}

/// The diagnostic verbosity level for a given `--verbose` setting.
///
/// Exposed as a pure function so the contract is testable without installing
/// a global subscriber.
#[must_use]
pub fn level_filter(verbose: bool) -> LevelFilter {
    if verbose {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    }
}

/// Produces a log-safe representation of a URL.
///
/// - the query string and fragment are removed entirely, because the
///   application cannot know which parameters carry authentication tokens;
/// - the password inside `user:password@host` userinfo is replaced with
///   `[REDACTED]`.
///
/// Malformed input never panics: anything that cannot be recognized is
/// passed through without scheme/userinfo handling. Crucially the caller
/// logs the returned string, never the original URL.
#[allow(dead_code)] // becomes load-bearing when download logging lands (Phase 3+)
#[must_use]
pub fn redact_url(url: &str) -> String {
    const MASK: &str = "[REDACTED]";

    // 1. Drop the query string and fragment: stop at the first '?' or '#'.
    let separator = url
        .char_indices()
        .find(|(_, c)| *c == '?' || *c == '#')
        .map_or(url.len(), |(idx, _)| idx);
    let without_query = &url[..separator];

    // 2. Mask the password inside the userinfo part, if any.
    let after_scheme = match without_query.find("://") {
        Some(pos) => pos + 3,
        None => {
            return without_query.to_owned();
        }
    };
    let authority_end = without_query[after_scheme..]
        .find('/')
        .map_or(without_query.len(), |idx| after_scheme + idx);
    let authority = &without_query[after_scheme..authority_end];

    let masked = match authority.rfind('@') {
        Some(at) => {
            let userinfo = &authority[..at];
            let host_part = &authority[at..];
            let user = match userinfo.split_once(':') {
                // user:password@ — mask the password part.
                Some((user, _password)) => format!("{user}:{MASK}"),
                // no colon: a bare username carries no secret.
                None => userinfo.to_owned(),
            };
            format!("{user}{host_part}")
        }
        None => authority.to_owned(),
    };

    format!(
        "{}{}{}",
        &without_query[..after_scheme],
        masked,
        &without_query[authority_end..]
    )
}

#[cfg(test)]
mod tests {
    use super::{level_filter, redact_url};
    use tracing_subscriber::filter::LevelFilter;

    #[test]
    fn default_level_is_concise_and_verbose_enables_debug() {
        assert_eq!(level_filter(false), LevelFilter::INFO);
        assert_eq!(level_filter(true), LevelFilter::DEBUG);
    }

    #[test]
    fn query_and_fragment_are_removed() {
        assert_eq!(
            redact_url("https://example.com/path?token=abc&x=1#page2"),
            "https://example.com/path"
        );
        assert_eq!(
            redact_url("https://example.com/path#section"),
            "https://example.com/path"
        );
    }

    #[test]
    fn userinfo_password_is_masked() {
        assert_eq!(
            redact_url("https://user:secret@example.com/file.zip?auth=1"),
            "https://user:[REDACTED]@example.com/file.zip"
        );
        assert_eq!(
            redact_url("https://user:secret@example.com:8443/file.zip"),
            "https://user:[REDACTED]@example.com:8443/file.zip"
        );
    }

    #[test]
    fn clean_urls_pass_through_unchanged() {
        assert_eq!(
            redact_url("https://example.com/file.zip"),
            "https://example.com/file.zip"
        );
        assert_eq!(
            redact_url("http://localhost:9000/fixture.bin"),
            "http://localhost:9000/fixture.bin"
        );
        assert_eq!(
            redact_url("https://user@example.com/file.zip"),
            "https://user@example.com/file.zip",
            "a bare username carries no password, nothing to mask"
        );
    }

    #[test]
    fn malformed_input_never_panics() {
        assert_eq!(redact_url(""), "");
        assert_eq!(redact_url("?$#"), "");
        assert_eq!(redact_url("some text with a ? in it"), "some text with a ");
        assert_eq!(
            redact_url("://user:pass@host/p"),
            "://user:[REDACTED]@host/p",
            "redaction is best-effort for malformed scheme parts"
        );
        assert_eq!(
            redact_url("https://:pass@host/p"),
            "https://:[REDACTED]@host/p"
        );
    }
}
