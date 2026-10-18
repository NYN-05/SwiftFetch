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
mod download;
mod error;
mod logging;

/// Phase 3 test-support include: the binary's own unit tests (e.g. the
/// network-bound tests in `src/download/probe.rs`) reuse the controlled
/// Phase 2 mock server by including this shared module under `cfg(test)`.
/// Test-support code never reaches a non-test compilation of the product.
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
pub(crate) mod common;

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
