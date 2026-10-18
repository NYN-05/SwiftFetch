//! Shared Phase 2 test support: deterministic fixtures and the controlled
//! local HTTP mock server consumed by the integration test binaries in
//! `tests/`. Nothing here is compiled into the product binary.

pub mod fixture;
pub mod server;
