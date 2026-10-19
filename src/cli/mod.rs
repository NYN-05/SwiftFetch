//! CLI layer: argument mapping, validation, and honest dispatch support.
//!
//! The CLI layer never implements transfer logic; it maps parsed arguments
//! onto the shared configuration types, validates obvious input errors
//! early, and delegates command dispatch to the application entry point.
//! Since Phase 11 it also owns user-facing rendering: live progress
//! (`progress`) and the persisted-queue status listing (`status`).

mod commands;
pub(crate) mod progress;
pub(crate) mod status;

pub use commands::{Cli, Commands, DownloadArgs};
use std::path::PathBuf;

use crate::config::AppConfig;
use crate::error::AppError;

/// Maps parsed `download` arguments onto [`AppConfig`], then validates.
///
/// Every field of the CLI surface maps to exactly one configuration field;
/// defaults live in [`AppConfig::default`] so there is a single source of
/// truth for values (CLI help text documents the same numbers).
pub fn config_from_download(args: &DownloadArgs) -> Result<AppConfig, AppError> {
    let defaults = AppConfig::default();
    let config = AppConfig {
        output_dir: args
            .output
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| defaults.output_dir.clone()),
        max_concurrent_files: args.max_files.unwrap_or(defaults.max_concurrent_files),
        max_segments_per_file: args.max_segments.unwrap_or(defaults.max_segments_per_file),
        retry_attempts: args.retries.unwrap_or(defaults.retry_attempts),
        zip_auto_extract: !args.no_extract,
    };
    config.validate()?;
    Ok(config)
}

/// Validates `download` inputs that can be judged before dispatch.
///
/// - at least one URL or a URL-list file must be present;
/// - inline URLs must start with the `http://` or `https://` scheme
///   (case-insensitive). Deeper URL parsing and list-file content handling
///   arrive with the transfer engine phases.
pub fn validate_download_input(args: &DownloadArgs) -> Result<(), AppError> {
    if args.urls.is_empty() && args.url_file.is_none() {
        return Err(AppError::input(
            "provide one or more HTTP or HTTPS URLs, or a URL-list file \
             with --url-file",
        ));
    }
    for url in &args.urls {
        validate_url_scheme(url)?;
    }
    Ok(())
}

/// Inline URLs must be direct HTTP or HTTPS downloads.
fn validate_url_scheme(url: &str) -> Result<(), AppError> {
    let lowered = url.to_ascii_lowercase();
    if lowered.starts_with("http://") || lowered.starts_with("https://") {
        Ok(())
    } else {
        Err(AppError::input(format!(
            "URLs must start with http:// or https:// (got {url})"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::{config_from_download, validate_download_input};
    use crate::cli::DownloadArgs;
    use crate::error::{AppError, ErrorCategory};
    use std::path::PathBuf;

    fn category_of(result: Result<(), AppError>) -> ErrorCategory {
        result
            .expect_err("validation must reject an invalid argument")
            .category()
    }

    #[test]
    fn default_arguments_map_to_architecture_defaults() {
        let config = config_from_download(&DownloadArgs::default())
            .expect("default arguments must be valid");
        assert_eq!(config.output_dir, PathBuf::from("."));
        assert_eq!(config.max_concurrent_files, 3);
        assert_eq!(config.max_segments_per_file, 4);
        assert_eq!(config.retry_attempts, 5);
        assert!(config.zip_auto_extract);
    }

    #[test]
    fn options_override_defaults() {
        let args = DownloadArgs {
            output: Some("downloads".to_owned()),
            max_files: Some(2),
            max_segments: Some(8),
            retries: Some(0),
            no_extract: true,
            ..DownloadArgs::default()
        };
        let config = config_from_download(&args).expect("values within range must be valid");
        assert_eq!(config.output_dir, PathBuf::from("downloads"));
        assert_eq!(config.max_concurrent_files, 2);
        assert_eq!(config.max_segments_per_file, 8);
        assert_eq!(config.retry_attempts, 0, "0 retries is a valid choice");
        assert!(!config.zip_auto_extract);
    }

    #[test]
    fn non_positive_concurrency_is_rejected_with_actionable_errors() {
        for args in [
            DownloadArgs {
                max_files: Some(0),
                urls: vec!["https://example.com/f".to_owned()],
                ..DownloadArgs::default()
            },
            DownloadArgs {
                max_segments: Some(0),
                urls: vec!["https://example.com/f".to_owned()],
                ..DownloadArgs::default()
            },
        ] {
            let error = config_from_download(&args).expect_err("zero limits must be rejected");
            assert_eq!(error.category(), ErrorCategory::Input);
            assert!(
                error.message().contains("at least 1"),
                "message must be actionable: {}",
                error.message()
            );
        }
    }

    #[test]
    fn empty_and_nul_output_are_rejected() {
        let empty = config_from_download(&DownloadArgs {
            output: Some(String::new()),
            ..DownloadArgs::default()
        });
        assert!(empty.is_err(), "empty --output must be rejected");

        let nul = config_from_download(&DownloadArgs {
            output: Some("bad\0dir".to_owned()),
            ..DownloadArgs::default()
        });
        assert!(nul.is_err(), "NUL byte must be rejected");
    }

    #[test]
    fn plausible_output_path_is_accepted() {
        let config = config_from_download(&DownloadArgs {
            output: Some("C:/Temp/dl".to_owned()),
            ..DownloadArgs::default()
        })
        .expect("path without forbidden characters is valid");
        assert_eq!(config.output_dir, PathBuf::from("C:/Temp/dl"));
    }

    #[test]
    fn at_least_one_url_or_list_file_is_required() {
        assert_eq!(
            category_of(validate_download_input(&DownloadArgs::default())),
            ErrorCategory::Input,
            "download with no input is useless and rejected"
        );
        assert!(
            validate_download_input(&DownloadArgs {
                url_file: Some(PathBuf::from("urls.txt")),
                ..DownloadArgs::default()
            })
            .is_ok(),
            "a URL-list file alone satisfies the input requirement"
        );
    }

    #[test]
    fn inline_urls_must_be_http_or_https() {
        assert!(validate_download_input(&DownloadArgs {
            urls: vec![
                "http://localhost:9000/a.bin".to_owned(),
                "HTTPS://EXAMPLE.COM/b.zip".to_owned(),
            ],
            ..DownloadArgs::default()
        })
        .is_ok());

        for invalid in ["ftp://host/f", "example.com/f", "", "://x"] {
            let error = validate_download_input(&DownloadArgs {
                urls: vec![invalid.to_owned()],
                ..DownloadArgs::default()
            })
            .expect_err("non-http(s) input must be rejected");
            assert_eq!(error.category(), ErrorCategory::Input);
            assert!(
                error.message().contains("http"),
                "message must state the accepted schemes: {}",
                error.message()
            );
        }
    }

    #[test]
    fn provided_url_and_list_file_are_both_accepted() {
        assert!(
            validate_download_input(&DownloadArgs {
                urls: vec!["https://example.com/x".to_owned()],
                url_file: Some(PathBuf::from("urls.txt")),
                ..DownloadArgs::default()
            })
            .is_ok(),
            "both input kinds may be provided together"
        );
    }

    #[test]
    fn cli_parse_accepts_the_documented_surface() {
        use crate::cli::{Cli, Commands};
        use clap::Parser;

        let parsed = Cli::try_parse_from([
            "swift-fetch",
            "download",
            "--url-file",
            "urls.txt",
            "-o",
            "out",
            "-j",
            "3",
            "-s",
            "2",
            "-r",
            "1",
        ])
        .expect("documented options must parse");
        let Commands::Download(args) = &parsed.command else {
            unreachable!("checked directly above");
        };
        assert!(validate_download_input(args).is_ok());
        assert!(
            config_from_download(args).is_ok(),
            "documented options must also produce a valid configuration"
        );

        let verbose =
            Cli::try_parse_from(["swift-fetch", "download", "-v", "https://example.com/f"])
                .expect("--verbose must parse from subcommand level");
        assert!(verbose.verbose);
        // Dispatch itself is exercised end to end by tests/cli.rs against
        // the local mock server (unit tests must never touch the real
        // application-data directory or the network).
    }
}
