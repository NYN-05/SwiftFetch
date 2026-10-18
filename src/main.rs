//! Windows-first CLI download manager.
//!
//! The binary is a thin layer: it parses the CLI contract, installs
//! diagnostics, and dispatches to the application entry point, mapping
//! typed errors to documented exit codes. All transfer, storage, and
//! archive behavior lives in modules introduced by later phases
//! (`DOCS/phases.md`).

use clap::Parser;

mod app;
mod cli;
mod config;
mod error;
mod logging;

fn main() {
    let cli = cli::Cli::parse();
    logging::init(cli.verbose);
    let exit_code = match app::run(&cli) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("error: {err}");
            err.category().exit_code()
        }
    };
    std::process::exit(exit_code);
}
