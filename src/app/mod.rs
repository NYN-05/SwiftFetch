//! Application orchestration: command dispatch.
//!
//! Phase 11 wires every command to the implemented services: `download`
//! drives the batch scheduler under graceful shutdown with live progress;
//! `status`, `resume`, and `retry` operate on the persisted queue through
//! the command handlers ([`commands_handler`]). Dispatch owns no transfer
//! or storage logic of its own — it routes the parsed CLI contract to the
//! handler that owns the plumbing and maps nothing about success or
//! failure beyond what the handlers report ([`CommandEnd`], [`AppError`]).

pub(crate) mod batch;
mod commands_handler;

use crate::cli::{Cli, Commands};
use crate::error::AppError;
use commands_handler::{handle_download, handle_resume, handle_retry, handle_status, CommandEnd};

/// Runs the already-parsed CLI contract.
///
/// This is the application entry point between the CLI layer and the
/// engine, storage, and archive layers. It is async because the download
/// workflows are; `main` builds the runtime around it (current-thread,
/// explicitly — no bootstrap macro).
pub async fn run(cli: &Cli) -> Result<CommandEnd, AppError> {
    // The global `--user-agent` value is intentionally threaded to every
    // command that performs transfers (download, resume, retry), so the
    // remote sees one consistent identity; `status` performs no HTTP.
    let user_agent = cli.user_agent.clone();
    match &cli.command {
        Commands::Download(args) => handle_download(args, user_agent).await,
        Commands::Status => handle_status(),
        Commands::Resume => handle_resume(user_agent).await,
        Commands::Retry => handle_retry(user_agent).await,
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::cli::Cli;
    use crate::error::ErrorCategory;
    use clap::Parser;

    /// Phase 11 wiring keeps the Phase 1 order guarantee: obviously invalid
    /// `download` input is rejected before any service is opened (no
    /// database is created, nothing is queued, nothing is fetched), and the
    /// error lands in the invalid-invocation category (exit 2 in `main`).
    ///
    /// Valid-input dispatch is exercised end to end by the integration
    /// suite (`tests/cli.rs`) against the controlled local mock server,
    /// with the database redirected via `SWIFT_FETCH_DATABASE` — unit tests
    /// must never touch the user's real application-data directory.
    #[tokio::test]
    async fn download_validation_errors_are_invalid_invocations() {
        for args in [
            vec!["download"],                          // no input at all
            vec!["download", "ftp://host/f"],          // wrong scheme
            vec!["download", "-j", "0", "http://h/f"], // non-positive concurrency
            vec!["download", "-s", "0", "http://h/f"], // non-positive segmentation
            vec!["download", "-o", "", "http://h/f"],  // empty output directory
        ] {
            let mut invocation = vec!["swift-fetch"];
            invocation.extend_from_slice(&args);
            let cli = Cli::try_parse_from(invocation)
                .unwrap_or_else(|error| panic!("inputs {args:?} must parse: {error}"));
            let error = run(&cli)
                .await
                .expect_err("invalid input must never run a command");
            assert_eq!(
                error.category(),
                ErrorCategory::Input,
                "invalid input is an invalid invocation: {}",
                error.message()
            );
        }
    }
}
