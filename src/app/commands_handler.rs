//! Command dispatch handlers.
//!
//! One function family per command shape so later phases insert their
//! implementations here without touching the application entry point.

use crate::cli::DownloadArgs;
use crate::error::AppError;

/// Handles the `download` command.
///
/// Phase 1: arguments and configuration are validated; dispatch stops at the
/// not-implemented guard. Phases 4+ replace the guard with the real engine.
pub(super) fn handle_download(args: &DownloadArgs) -> Result<(), AppError> {
    let _config = crate::cli::config_from_download(args)?;
    crate::cli::validate_download_input(args)?;
    Err(AppError::not_yet_implemented("download"))
}

/// Handles commands that only inspect or alter persisted queue state
/// (`status`, `resume`, `retry`); persistence arrives in Phase 5.
pub(super) fn handle_queue_command(command: QueueCommand) -> Result<(), AppError> {
    Err(AppError::not_yet_implemented(command.name()))
}

/// Marker types for the queue-inspection commands, so dispatch stays
/// exhaustive without a stringly-typed path.
#[derive(Debug, Clone, Copy)]
pub(super) enum QueueCommand {
    Status,
    Resume,
    Retry,
}

impl QueueCommand {
    /// The CLI-facing name of the command.
    const fn name(self) -> &'static str {
        match self {
            QueueCommand::Status => "status",
            QueueCommand::Resume => "resume",
            QueueCommand::Retry => "retry",
        }
    }
}
