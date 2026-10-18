//! HTTP download engine.
//!
//! Phase 3 contributes the shared, configured HTTP client ([`http`]) and
//! resource probing ([`probe`]); sequential and segmented transfers, the
//! retry policy, and progress reporting arrive with their own phases per
//! `DOCS/phases.md`. Commands still dispatch honest not-implemented errors.
//!
//! Test-support note: the network behavior of this module is verified
//! against the shared Phase 2 mock server, which is re-used from the
//! binary's unit tests via the `common` module declared in `main.rs`
//! (`#[cfg(test)] #[path = "tests/common/mod.rs"]`). No `src` module
//! shadows it at runtime.

pub mod http;
pub mod probe;
