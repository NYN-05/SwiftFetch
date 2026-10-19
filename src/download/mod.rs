//! HTTP download engine.
//!
//! Phase 3 contributes the shared, configured HTTP client ([`http`]) and
//! resource probing ([`probe`]). Phase 4 adds the sequential transfer
//! engine ([`sequential`]) — one resource streamed to a Windows-safe file
//! name and finalized by no-overwrite rename — with wait-free internal
//! progress counters ([`progress`]). Phase 6 makes the same engine
//! durable across process restarts: [`resume`] reconciles unfinished
//! records against the partial files on disk, revalidates remote identity,
//! appends verified ranges, and returns honest exceptions to new callers;
//! [`retry`] supplies the bounded transient-failure classification and
//! backoff those flows share; [`segmented`] (Phase 8) plans and runs
//! parallel byte-range transfers with validated `206` answers, bounded
//! per-file workers, and a safe sequential fallback. Commands still
//! dispatch honest not-implemented errors until Phase 11 wires them.
//!
//! Transfer-law recap for this module: error statuses never write bytes,
//! incomplete data never gains a final name, existing files are never
//! overwritten, and every logged URL goes through
//! [`crate::logging::redact_url`].
//!
//! Test-support note: the network behavior of this module is verified
//! against the shared Phase 2 mock server, which is re-used from the
//! binary's unit tests via the `common` module declared in `main.rs`
//! (`#[cfg(test)] #[path = "../tests/common/mod.rs"]`). No `src` module
//! shadows it at runtime.

pub mod http;
pub mod probe;
pub mod progress;
pub mod resume;
pub mod retry;
pub mod segmented;
pub mod sequential;
