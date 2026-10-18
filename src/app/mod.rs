//! Application orchestration: argument validation and command dispatch.
//!
//! Phase 1 defines the command surface; no command is wired to an
//! implementation yet. Dispatch therefore reports a typed, honest
//! not-implemented error instead of fabricating success. Wiring happens
//! phase by phase (`DOCS/phases.md`).

mod commands_handler;

use crate::cli::{Cli, Commands};
use crate::error::AppError;
use commands_handler::{handle_download, handle_queue_command, QueueCommand};

/// Runs the already-parsed CLI contract.
///
/// This is the application entry point between the CLI layer and, in later
/// phases, the download engine, storage, and archive layers.
pub fn run(cli: &Cli) -> Result<(), AppError> {
    match &cli.command {
        Commands::Download(args) => handle_download(args),
        Commands::Status => handle_queue_command(QueueCommand::Status),
        Commands::Resume => handle_queue_command(QueueCommand::Resume),
        Commands::Retry => handle_queue_command(QueueCommand::Retry),
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::cli::Cli;
    use crate::error::ErrorCategory;
    use clap::Parser;

    #[test]
    fn every_queue_command_reports_not_implemented_in_phase_1() {
        for command in ["status", "resume", "retry"] {
            let cli =
                Cli::try_parse_from(["swift-fetch", command]).expect("valid subcommand must parse");
            let error = run(&cli).expect_err("phase 1 must not claim success for any command");
            assert_eq!(error.category(), ErrorCategory::NotYetImplemented);
            assert!(
                error.message().contains(command),
                "message must name the executed command: {}",
                error.message()
            );
        }
    }

    #[test]
    fn download_with_valid_arguments_reports_not_implemented() {
        let cli = Cli::try_parse_from([
            "swift-fetch",
            "download",
            "https://localhost:9000/fixture.bin",
        ])
        .expect("valid download arguments must parse");
        let error = run(&cli).expect_err("phase 1 must not claim download success");
        assert_eq!(error.category(), ErrorCategory::NotYetImplemented);
        assert!(error.message().contains("download"));
    }

    #[test]
    fn download_validation_errors_precede_the_not_implemented_guard() {
        let cli = Cli::try_parse_from(["swift-fetch", "download", "ftp://host/f"])
            .expect("arguments must parse");
        let error = run(&cli).expect_err("invalid URL must be rejected early");
        assert_eq!(error.category(), ErrorCategory::Input);

        let cli = Cli::try_parse_from(["swift-fetch", "download", "-j", "0", "https://host/f"])
            .expect("arguments must parse");
        let error = run(&cli).expect_err("non-positive concurrency must be rejected early");
        assert_eq!(error.category(), ErrorCategory::Input);
    }
}
