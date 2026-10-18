//! Clap-based command definitions.
//!
//! This is the typed CLI contract. Every command listed here is *defined* in
//! Phase 1; only later phases wire them to implementations. Help text must
//! therefore stay accurate about what is actually supported: unwired
//! commands say so.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Root of the command-line interface.
///
/// The help text must not promise behavior that does not exist yet; the
/// trailing status line states this explicitly.
#[derive(Debug, Parser)]
#[command(
    name = "download-manager",
    version,
    about = "Windows-first command-line download manager.",
    long_about = "Windows-first command-line download manager.",
    after_help = "Development status: all commands are defined here, but none \
                 are wired to a transfer engine yet; they exit with an honest \
                 not-implemented error."
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
}

/// The exact command surface required by the project.
///
/// `download` carries the download inputs and configuration options;
/// `status`, `resume`, and `retry` inspect or alter persistent queue state
/// once persistence exists (Phase 5+).
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Download one URL, several URLs, or URLs from a list file (not wired yet).
    Download(DownloadArgs),
    /// Inspect queued, active, completed, and failed downloads (not wired yet).
    Status,
    /// Resume unfinished downloads (not wired yet).
    Resume,
    /// Retry failed downloads (not wired yet).
    Retry,
}

/// Inputs and options of the `download` command.
///
/// The architecture requires Honesty: options printed here must map to real
/// configuration the engine will consume; the engine arrives in later
/// phases, until then `download` dispatches an honest not-implemented error.
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
        help = "Plain-text file with one URL per line (content handling \
                arrives with the transfer engine)"
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
                default; it arrives in a later phase)"
    )]
    pub no_extract: bool,
}
