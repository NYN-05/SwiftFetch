//! Clap-based command definitions.
//!
//! This is the typed CLI contract. Since Phase 11 every command is wired
//! to its implementation (download through the engine, the queue commands
//! through the persisted state); help text documents what the commands do
//! and the exit-code table they honor.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Root of the command-line interface.
///
/// The trailing status text carries the documented exit-code table: the
/// same table `main` maps outcomes through and the README documents.
#[derive(Debug, Parser)]
#[command(
    name = "SwiftFetch",
    version,
    about = "SwiftFetch — Windows-first command-line download manager.",
    long_about = "SwiftFetch — Windows-first command-line download manager.",
    after_help = "Exit codes: 0 every requested download completed; \
                  1 at least one did not (failed or interrupted), or the \
                  command could not run; 2 invalid invocation or \
                  configuration."
)]
pub struct Cli {
    /// The parsed command.
    #[command(subcommand)]
    pub command: Commands,

    /// Enable detailed diagnostic logging on stderr.
    #[arg(
        short = 'v',
        long,
        global = true,
        help = "Enable detailed diagnostic logging on stderr"
    )]
    pub verbose: bool,

    /// Custom `User-Agent` header value for every HTTP request the sends
    /// through, whichever command performs them (`download`, `resume`,
    /// `retry`). The value cannot be empty and must contain only printable
    /// ASCII characters (a valid HTTP header value); no browser-impersonation
    /// defaults are invented — absent the flag no such header is sent.
    #[arg(
        long = "user-agent",
        global = true,
        value_name = "STRING",
        value_parser = crate::config::valid_user_agent,
        help = "Custom User-Agent header value sent with every HTTP request \
                [default: none]"
    )]
    pub user_agent: Option<String>,
}

/// The exact command surface required by the project.
///
/// `download` carries the download inputs and configuration options;
/// `status`, `resume`, and `retry` inspect or alter the persistent queue
/// state.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Download one URL, several URLs, or URLs from a list file.
    Download(DownloadArgs),
    /// Inspect queued, active, paused, completed, and failed downloads.
    Status,
    /// Resume unfinished downloads from persistent state.
    Resume,
    /// Retry failed downloads from persistent state.
    Retry,
}

/// Inputs and options of the `download` command.
///
/// The architecture requires honesty: options printed here map to real
/// configuration the engine consumes ([`crate::config::AppConfig`]; the
/// defaults live there as the single source of truth).
#[derive(Debug, Default, Args)]
pub struct DownloadArgs {
    /// One or more HTTP/HTTPS URLs.
    #[arg(
        value_name = "URL",
        help = "One or more HTTP or HTTPS URLs; use --url-file for a list"
    )]
    pub urls: Vec<String>,

    /// Plain-text file with one URL per line.
    #[arg(
        short = 'f',
        long = "url-file",
        value_name = "PATH",
        help = "Plain-text file with one URL per line (blank lines and \
                #-comments are ignored)"
    )]
    pub url_file: Option<PathBuf>,

    /// Output directory for downloaded files.
    #[arg(
        short = 'o',
        long = "output",
        value_name = "DIR",
        help = "Output directory [default: current directory]"
    )]
    pub output: Option<String>,

    /// Maximum number of files downloading at the same time.
    #[arg(
        short = 'j',
        long = "files",
        value_name = "N",
        help = "Maximum concurrent file downloads [default: 3]"
    )]
    pub max_files: Option<u32>,

    /// Maximum number of byte-range segments per file.
    #[arg(
        short = 's',
        long = "segments",
        value_name = "N",
        help = "Maximum segments per file [default: 4]"
    )]
    pub max_segments: Option<u32>,

    /// Number of retry attempts for transient failures.
    #[arg(
        short = 'r',
        long = "retries",
        value_name = "N",
        help = "Retry attempts for transient failures [default: 5]"
    )]
    pub retries: Option<u32>,

    /// Disable automatic ZIP extraction.
    #[arg(
        long = "no-extract",
        help = "Disable automatic ZIP extraction (extraction is enabled by \
                default; the downloaded archive is always preserved)"
    )]
    pub no_extract: bool,
}
