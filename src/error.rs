//! Typed application errors.
//!
//! Phase 1 establishes the shared error contract used by every later module:
//! a stable set of error categories (from `DOCS/architecture.md` §12), a
//! user-facing actionable message, and an optional internal diagnostic
//! source that is kept out of the default output.

use std::error::Error as StdError;
use thiserror::Error;

/// Categories of application errors.
///
/// Each category maps to a stable, documented string and a stable process
/// exit code. The mapping is part of the CLI contract and is covered by
/// tests; new categories may only be appended when a phase requires them.
///
/// `dead_code` is waived for the whole taxonomy on purpose: categories earn
/// their constructors in later phases (filesystem, database, ZIP, ...),
/// while the Phase 1 contract already locks strings, order, and exit maps
/// via the stability tests in this module.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Invalid arguments, configuration, or URL input (exit code 2).
    Input,
    /// HTTP and TLS failures.
    Http,
    /// Server capabilities that the transfer strategy cannot use.
    UnsupportedServer,
    /// Byte-range and content validation failures.
    RangeValidation,
    /// Bounded retries for transient errors ran out.
    RetryExhausted,
    /// Filesystem and disk-space failures.
    Filesystem,
    /// Database and recovery failures.
    Database,
    /// ZIP validation and extraction failures.
    ZipExtraction,
    /// Cancellation and shutdown outcomes.
    Cancelled,
    /// Command surface defined in Phase 1 but wired in a later phase.
    NotYetImplemented,
}

impl ErrorCategory {
    /// Every category in declaration order; the order is a contract.
    #[allow(dead_code)] // wired into production paths by later phases
    pub const ALL: [ErrorCategory; 10] = [
        ErrorCategory::Input,
        ErrorCategory::Http,
        ErrorCategory::UnsupportedServer,
        ErrorCategory::RangeValidation,
        ErrorCategory::RetryExhausted,
        ErrorCategory::Filesystem,
        ErrorCategory::Database,
        ErrorCategory::ZipExtraction,
        ErrorCategory::Cancelled,
        ErrorCategory::NotYetImplemented,
    ];

    /// Stable, kebab-case category identifier used in messages and tests.
    #[allow(dead_code)] // wired into production paths by later phases
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCategory::Input => "input",
            ErrorCategory::Http => "http",
            ErrorCategory::UnsupportedServer => "unsupported-server",
            ErrorCategory::RangeValidation => "range-validation",
            ErrorCategory::RetryExhausted => "retry-exhausted",
            ErrorCategory::Filesystem => "filesystem",
            ErrorCategory::Database => "database",
            ErrorCategory::ZipExtraction => "zip-extraction",
            ErrorCategory::Cancelled => "cancelled",
            ErrorCategory::NotYetImplemented => "not-implemented",
        }
    }

    /// Process exit code for this category.
    ///
    /// Exit code `2` marks invalid invocations; all other application
    /// outcomes exit with `1`. The full exit-code table is finalized in
    /// Phase 11 (`DOCS/phases.md`).
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            ErrorCategory::Input => 2,
            _ => 1,
        }
    }
}

/// Application error carrying a stable category, a user-facing message, and
/// an optional internal diagnostic source.
///
/// The `Display` implementation intentionally renders only the user-facing
/// message; diagnostic context must be accessed explicitly (for example in
/// verbose logs) so default terminal output stays concise and safe.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct AppError {
    category: ErrorCategory,
    message: String,
    #[source]
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl AppError {
    /// Creates an error for the given category with a user-facing message.
    #[must_use]
    pub fn new(category: ErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            source: None,
        }
    }

    /// Creates an input/configuration error with an actionable message.
    #[must_use]
    pub fn input(message: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Input, message)
    }

    /// Creates the Phase 1 guard error for a defined but unwired command.
    #[must_use]
    pub fn not_yet_implemented(command: &'static str) -> Self {
        Self::new(
            ErrorCategory::NotYetImplemented,
            format!(
                "the {command} command is not implemented yet; it will be \
                 available when its phase arrives"
            ),
        )
    }

    /// Attaches an internal diagnostic source that never appears in the
    /// user-facing `Display` output.
    #[allow(dead_code)] // transferred modules supply sources from Phase 3 on
    #[must_use]
    pub fn with_source(mut self, source: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// The stable error category.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.category
    }

    /// The user-facing message.
    #[allow(dead_code)] // message kept stable for tests; surfacing grows later
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[cfg(test)]
mod tests {
    use super::{AppError, ErrorCategory};
    use std::error::Error as StdError;

    #[test]
    fn categories_have_stable_identifiers() {
        let expected = [
            ("input", ErrorCategory::Input),
            ("http", ErrorCategory::Http),
            ("unsupported-server", ErrorCategory::UnsupportedServer),
            ("range-validation", ErrorCategory::RangeValidation),
            ("retry-exhausted", ErrorCategory::RetryExhausted),
            ("filesystem", ErrorCategory::Filesystem),
            ("database", ErrorCategory::Database),
            ("zip-extraction", ErrorCategory::ZipExtraction),
            ("cancelled", ErrorCategory::Cancelled),
            ("not-implemented", ErrorCategory::NotYetImplemented),
        ];
        assert_eq!(
            expected.len(),
            ErrorCategory::ALL.len(),
            "a category was added without extending this test"
        );
        for (ident, category) in expected {
            assert!(!ErrorCategory::ALL.is_empty());
            assert_eq!(category.as_str(), ident, "category string changed");
            assert!(
                ErrorCategory::ALL.contains(&category),
                "category {ident} missing from ALL list"
            );
        }
    }

    #[test]
    fn only_invalid_invocations_exit_with_code_2() {
        assert_eq!(ErrorCategory::Input.exit_code(), 2);
        for category in ErrorCategory::ALL {
            if category != ErrorCategory::Input {
                assert_eq!(category.exit_code(), 1, "{category:?} exited with 2");
            }
        }
    }

    #[test]
    fn display_shows_only_the_user_facing_message() {
        let err = AppError::input("output directory must not be empty");
        assert_eq!(err.to_string(), "output directory must not be empty");
        assert!(err.source().is_none());
    }

    #[test]
    fn diagnostic_source_is_kept_out_of_display() {
        let source = std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "simulated internal diagnostic detail",
        );
        let err = AppError::input("could not open the URL list file").with_source(source);

        assert!(err.source().is_some(), "diagnostic source must be retained");
        assert!(
            !err.to_string()
                .contains("simulated internal diagnostic detail"),
            "user-facing display leaked internal context: {}",
            err
        );
    }

    #[test]
    fn not_yet_implemented_names_the_command() {
        let err = AppError::not_yet_implemented("download");
        assert_eq!(err.category(), ErrorCategory::NotYetImplemented);
        assert_eq!(err.category().exit_code(), 1);
        assert!(
            err.message().contains("download"),
            "message must name the command: {}",
            err.message()
        );
    }
}
