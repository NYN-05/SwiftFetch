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
/// existing files never overwritten).
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("."),
            max_concurrent_files: 3,
            max_segments_per_file: 4,
            retry_attempts: 5,
            zip_auto_extract: true,
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
        validate_output_dir(&self.output_dir)
    }
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
        assert!(defaults.validate().is_ok());
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
