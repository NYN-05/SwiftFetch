//! Storage layer.
//!
//! Phase 4 contributes the partial-file writer and safe finalization
//! ([`file_writer`]): partial transfers live under an identifiable
//! engine-owned `.part` name, and the final name is claimed through an
//! exclusive reservation plus a same-volume rename, so an existing user
//! file can never be replaced. Phase 5 adds the SQLite queue and recovery
//! metadata store: [`schema`] owns the versioned schema and migration
//! protocol, [`repository`] owns the records, lifecycle states, and every
//! database read/write (ADR-0017). The database stores metadata only —
//! downloaded file contents never enter it — and lives in the user's local
//! application-data directory, separate from any download directory
//! (architecture §9).

pub mod file_writer;
pub mod repository;
pub mod schema;
