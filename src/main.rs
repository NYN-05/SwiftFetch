//! Windows-first CLI download manager.
//!
//! The binary is a thin layer: it parses the CLI contract, installs
//! diagnostics, builds the async runtime the command workflows run on
//! (explicitly, current-thread — no bootstrap macro), and maps the typed
//! outcomes to the documented exit codes. All transfer, storage, and
//! archive behavior lives in the engine and storage modules
//! (`DOCS/phases.md`).
//!
//! Exit codes (help text and README carry the same table):
//! - `0` — every requested download of the invocation completed;
//! - `1` — at least one did not (failed or interrupted), or the command
//!   could not continue (unrecoverable application failure);
//! - `2` — invalid invocation or configuration.

use std::time::Duration;

use clap::Parser;

mod app;
mod archive;
mod cli;
mod config;
mod download;
mod error;
mod logging;
mod storage;

/// Phase 3 test-support include: the binary's own unit tests (e.g. the
/// network-bound tests in `src/download/probe.rs`) reuse the controlled
/// Phase 2 mock server by including this shared module under `cfg(test)`.
/// Test-support code never reaches a non-test compilation of the product.
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
pub(crate) mod common;

/// Phase 13 performance benchmarks: long-running, ignored measurements that
/// drive the real engine and storage layers in-process. Excluded from
/// non-test builds like every other `#[cfg(test)]` module.
#[cfg(test)]
mod benchmarks;

fn main() {
    let cli = cli::Cli::parse();
    logging::init(cli.verbose);

    // The workflows are async (the engine's shutdown and progress wiring
    // races on tokio primitives); the runtime itself is deliberately
    // current-thread: the engine is a bounded pool of tasks over one
    // serialized database connection, not a many-core server.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the tokio runtime must build");

    let exit_code = match runtime.block_on(app::run(&cli)) {
        Ok(end) => end.exit_code(),
        Err(err) => {
            eprintln!("error: {err}");
            err.category().exit_code()
        }
    };
    // Drop the runtime first so spawned tasks are joined/torn down before
    // the process ends; then exit with the documented code.
    runtime.shutdown_timeout(Duration::from_secs(1));
    std::process::exit(exit_code);
}
