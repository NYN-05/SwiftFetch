//! Storage layer.
//!
//! Phase 4 contributes the partial-file writer and safe finalization
//! ([`file_writer`]): partial transfers live under an identifiable
//! engine-owned `.part` name, and the final name is claimed through an
//! exclusive reservation plus a same-volume rename, so an existing user
//! file can never be replaced. The SQLite repository, schema, and recovery
//! metadata arrive in Phase 5 per `DOCS/phases.md`.

pub mod file_writer;
