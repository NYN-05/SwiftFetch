//! Shared Phase 2 test support: deterministic fixtures and the controlled
//! local HTTP mock server consumed by the integration test binaries in
//! `tests/` and — since Phase 3 — by the binary's own unit tests through the
//! `common` module declared in `main.rs`. Nothing here is compiled into a
//! non-test build of the product.

pub mod fixture;
pub mod server;
