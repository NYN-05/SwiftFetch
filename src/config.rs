//! Central configuration types.
//!
//! Phase 1 defines the configuration model for the whole application:
//! output directory, concurrency settings, retry settings, and the ZIP
//! extraction preference. Defaults come from `DOCS/architecture.md` §6 and
//! are documented as provisional values; later phases only tune them with
//! benchmark evidence.

use crate::error::AppError;
use std::path::{Path, PathBuf};

/// Immutable snapshot of user-facing run configuration.
///
/// Defaults explicitly follow `DOCS/architecture.md` §6 (concurrent files 3,
/// segments per file 4, retry attempts 5, automatic ZIP extraction enabled,
/// existing files never overwritten). No `User-Agent` is sent by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    /// Directory that receives downloaded and finalized files.
    pub output_dir: PathBuf,
    /// Maximum number of files downloading at the same time.
    pub max_concurrent_files: u32,
    /// Maximum number of byte-range segments per file.
    pub max_segments_per_file: u32,
    /// Number of retry attempts for eligible transient failures.
    pub retry_attempts: u32,
    /// Whether completed ZIP archives are extracted automatically.
    pub zip_auto_extract: bool,
    /// Optional `User-Agent` header value sent with every HTTP request.
    /// `None` means the HTTP library's default (or none).
    pub user_agent: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("."),
            max_concurrent_files: 3,
            max_segments_per_file: 4,
            retry_attempts: 5,
            zip_auto_extract: true,
            user_agent: None,
        }
    }
}

impl AppConfig {
    /// Validates the configuration against the Phase 1 contract.
    ///
    /// Concurrency limits must be positive; the output directory must be a
    /// usable, non-empty path without embedded NUL characters. Deeper
    /// filesystem checks (existence, writability) belong to the phases that
    /// touch the filesystem.
    pub fn validate(&self) -> Result<(), AppError> {
        if self.max_concurrent_files == 0 {
            return Err(AppError::input(format!(
                "the maximum number of concurrent file downloads must be at \
                 least 1 (got {})",
                self.max_concurrent_files
            )));
        }
        if self.max_segments_per_file == 0 {
            return Err(AppError::input(format!(
                "the maximum number of segments per file must be at least 1 \
                 (got {})",
                self.max_segments_per_file
            )));
        }
        validate_output_dir(&self.output_dir)?;
        if let Some(agent) = &self.user_agent {
            // The same rule the clap value_parser applies to `--user-agent`:
            // keeping the check here makes one validation source for both
            // the CLI surface and programmatic configs.
            valid_user_agent(agent)
                .map_err(|reason| AppError::input(format!("invalid --user-agent: {reason}")))?;
        }
        Ok(())
    }
}

/// Validates a configured `User-Agent` header value.
///
/// The value must be non-empty, contain no leading or trailing whitespace,
/// and consist only of visible ASCII characters plus interior spaces (the
/// HTTP header-value characters a `User-Agent` can legally carry). No
/// browser impersonation is built in: absent the flag, no `User-Agent`
/// header is sent at all.
///
/// The same function is the clap `value_parser` for `--user-agent`
/// (violations are invalid invocations, exit code 2) and a check inside
/// [`AppConfig::validate`](AppConfig::validate) — one validation source
/// for the CLI surface and programmatic configs alike.
///
/// # Errors
/// A human-readable rejection reason when the value is not a header-safe
/// `User-Agent`.
pub fn valid_user_agent(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err("the value cannot be empty".to_owned());
    }
    if value.trim().len() != value.len() {
        return Err("the value cannot have leading or trailing whitespace".to_owned());
    }
    if let Some(index) = value
        .as_bytes()
        .iter()
        .position(|byte| !byte.is_ascii_graphic() && *byte != b' ')
    {
        return Err(format!(
            "the value contains a character a header value cannot carry (byte 0x{:02X})",
            value.as_bytes()[index]
        ));
    }
    Ok(value.to_owned())
}

/// Validates an output directory path for obvious early failures.
///
/// - empty paths are rejected with an actionable message;
/// - paths containing NUL bytes are rejected (invalid on every platform);
/// - non-Unicode paths are rejected so the CLI can address them reliably.
fn validate_output_dir(path: &Path) -> Result<(), AppError> {
    let text = match path.as_os_str().to_str() {
        Some(text) => text,
        None => {
            return Err(AppError::input(
                "the output directory must be valid (Unicode) text",
            ));
        }
    };
    if text.is_empty() {
        return Err(AppError::input(
            "the output directory must not be empty (remove --output to use \
             the current directory, or provide a path)",
        ));
    }
    if text.contains('\0') {
        return Err(AppError::input(
            "the output directory must not contain a null character",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_output_dir, AppConfig};
    use crate::error::{AppError, ErrorCategory};
    use std::path::{Path, PathBuf};

    fn category_of(result: Result<(), AppError>) -> ErrorCategory {
        result
            .expect_err("validation must reject an invalid configuration")
            .category()
    }

    #[test]
    fn defaults_follow_architecture_starting_values() {
        let defaults = AppConfig::default();
        assert_eq!(defaults.output_dir, PathBuf::from("."));
        assert_eq!(defaults.max_concurrent_files, 3);
        assert_eq!(defaults.max_segments_per_file, 4);
        assert_eq!(defaults.retry_attempts, 5);
        assert!(defaults.zip_auto_extract);
        assert_eq!(
            defaults.user_agent, None,
            "no User-Agent header is sent by default (no impersonation)"
        );
        assert!(defaults.validate().is_ok());
    }

    #[test]
    fn user_agent_values_are_header_safe() {
        for accepted in [
            "swiftfetch-test/1.0",
            "SwiftFetch/0.15 (Windows; custom build) +tests",
        ] {
            assert_eq!(
                super::valid_user_agent(accepted).as_deref(),
                Ok(accepted),
                "a printable-ASCII value is a valid User-Agent: {accepted}"
            );
        }
        for rejected in ["", "   ", " leading", "trailing ", "a\tb", "a\u{FFFD}b"] {
            assert!(
                super::valid_user_agent(rejected).is_err(),
                "an empty, whitespace-padded, control, or non-ASCII value is rejected: {rejected:?}"
            );
        }
    }

    #[test]
    fn an_invalid_user_agent_is_an_invalid_configuration() {
        let config = AppConfig {
            user_agent: Some("bad\tvalue".to_owned()),
            ..Default::default()
        };
        assert_eq!(category_of(config.validate()), ErrorCategory::Input);
    }

    #[test]
    fn zero_concurrent_files_is_rejected() {
        let config = AppConfig {
            max_concurrent_files: 0,
            ..Default::default()
        };
        assert_eq!(category_of(config.validate()), ErrorCategory::Input);
    }

    #[test]
    fn zero_segments_per_file_is_rejected() {
        let config = AppConfig {
            max_segments_per_file: 0,
            ..Default::default()
        };
        assert_eq!(category_of(config.validate()), ErrorCategory::Input);
    }

    #[test]
    fn zero_retries_is_a_valid_configuration() {
        let config = AppConfig {
            retry_attempts: 0,
            ..Default::default()
        };
        assert!(config.validate().is_ok(), "retries = 0 means no retries");
    }

    #[test]
    fn empty_output_dir_is_rejected() {
        let err =
            validate_output_dir(Path::new("")).expect_err("empty output path must be rejected");
        assert_eq!(err.category(), ErrorCategory::Input);
        assert!(
            err.message().contains("--output"),
            "message must tell the user how to fix it: {}",
            err.message()
        );
    }

    #[test]
    fn nul_byte_in_output_dir_is_rejected() {
        assert_eq!(
            category_of(validate_output_dir(Path::new("out\0dir"))),
            ErrorCategory::Input
        );
    }

    #[test]
    fn plain_output_dir_is_accepted() {
        assert!(validate_output_dir(Path::new(".")).is_ok());
        assert!(validate_output_dir(Path::new("downloads")).is_ok());
        assert!(
            validate_output_dir(Path::new("C:/Temp/downloads")).is_ok(),
            "windows-style absolute paths are valid input at this stage"
        );
    }
}
