//! Versioned SQLite schema and the migration protocol (Phase 5, storage layer).
//!
//! Rules for this module, from `DOCS/architecture.md` §9 and phase 5:
//!
//! - the database schema is created by an ordered list of migrations; each
//!   migration applies inside its own immediate transaction together with
//!   the `schema_version` bookkeeping row, so a database can never stop
//!   half-migrated or versionless (SQLite DDL is transactional);
//! - a database written by a *newer* SwiftFetch is refused outright, never
//!   downgraded or guessed at;
//! - the record tables (`downloads`, `segments`, `extractions`) hold
//!   metadata only: no column is BLOB-typed, so downloaded file contents
//!   cannot enter the database — file bytes live in the filesystem;
//! - every database failure maps to [`ErrorCategory::Database`] with the
//!   original error preserved as the hidden diagnostic source.

#![allow(dead_code)] // wired into production paths from Phase 6 onward (ADR-0017)

use std::error::Error as StdError;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::error::{AppError, ErrorCategory};

/// Maps a rusqlite failure into a typed, user-facing [`AppError`]; the raw
/// error stays available diagnostics. `operation` is a running-text fragment
/// (for example `"migrating the schema"`), composed as
/// `the download database failed while {operation}: {err}`.
pub(crate) fn db_failure(operation: &str, err: impl StdError + Send + Sync + 'static) -> AppError {
    AppError::new(
        ErrorCategory::Database,
        format!("the download database failed while {operation}: {err}"),
    )
    .with_source(err)
}

/// One ordered schema migration. `version` is the generation the database
/// reaches once `sql` has been applied successfully.
pub(crate) struct Migration {
    pub(crate) version: i64,
    pub(crate) sql: &'static str,
}

/// The schema generation this binary produces and fully understands.
pub(crate) const SCHEMA_VERSION: i64 = 1;

/// Name of the bookkeeping table that records the applied schema version.
const VERSION_TABLE: &str = "schema_version";

const MIGRATION_1: &str = "
CREATE TABLE downloads (
    id              INTEGER PRIMARY KEY,
    url             TEXT    NOT NULL,
    effective_url   TEXT,
    destination     TEXT    NOT NULL,
    file_name       TEXT    NOT NULL,
    status          TEXT    NOT NULL,
    expected_size   INTEGER,
    etag            TEXT,
    last_modified   TEXT,
    retry_count     INTEGER NOT NULL DEFAULT 0,
    error_category  TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE TABLE segments (
    id              INTEGER PRIMARY KEY,
    download_id     INTEGER NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    min_byte        INTEGER NOT NULL,
    max_byte        INTEGER NOT NULL,
    state           TEXT    NOT NULL,
    received_bytes  INTEGER NOT NULL
);
CREATE TABLE extractions (
    id              INTEGER PRIMARY KEY,
    download_id     INTEGER NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    archive_path    TEXT    NOT NULL,
    destination     TEXT    NOT NULL,
    state           TEXT    NOT NULL,
    error_category  TEXT,
    error_message   TEXT
);
";

/// Ordered migrations starting from generation 0 (an empty database file).
/// New schema generations are appended here; existing entries are never
/// rewritten. Every record column is metadata: no BLOB columns, per the
/// architecture's "the database is not a replacement for file contents".
pub(crate) const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: MIGRATION_1,
}];

/// Explicit projection for download rows; index order is fixed by the row
/// mapping in `repository.rs` (`download_from_row`).
pub(crate) const DOWNLOAD_COLUMNS: &str = "id, url, effective_url, destination, file_name, \
     status, expected_size, etag, last_modified, retry_count, error_category, created_at, \
     updated_at";

/// Explicit projection for segment rows (mapped positionally in
/// `repository.rs`).
pub(crate) const SEGMENT_COLUMNS: &str =
    "id, download_id, min_byte, max_byte, state, received_bytes";

/// Explicit projection for extraction rows (mapped positionally in
/// `repository.rs`).
pub(crate) const EXTRACTION_COLUMNS: &str =
    "id, download_id, archive_path, destination, state, error_category, error_message";

/// Reads the schema generation of a database connection.
///
/// Generation 0 means "no version table exists yet" (a fresh or wholly
/// failed initialization); an existing table stores exactly one row, and
/// its value is the generation. Older generations stay applied until their
/// migrations run; anything greater than this build understands is reported
/// by [`migrate_connection`].
pub(crate) fn version_of(conn: &Connection) -> Result<i64, AppError> {
    let has_table: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [VERSION_TABLE],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| db_failure("checking the schema version table", err))?;
    if has_table.is_none() {
        return Ok(0);
    }
    let version: Option<i64> = conn
        .query_row(
            &format!("SELECT MAX(version) FROM {VERSION_TABLE}"),
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| db_failure("reading the schema version", err))?;
    Ok(version.unwrap_or(0))
}

/// Applies every pending migration to `conn`, each inside its own
/// immediate transaction.
///
/// For every migration newer than the database's current generation, the
/// migration SQL and the schema-version update commit or roll back
/// together: a failure at any point leaves the previous generation
/// completely intact, and a fresh database that fails to initialize stays
/// empty (so the next attempt re-runs cleanly). A database whose recorded
/// generation exceeds what this build understands is refused with an
/// actionable [`ErrorCategory::Database`] error — downgrading would risk
/// interpreting records written by a later version.
pub(crate) fn migrate_connection(
    conn: &mut Connection,
    migrations: &[Migration],
) -> Result<(), AppError> {
    let current = version_of(conn)?;
    let supported = migrations.last().map_or(0, |migration| migration.version);
    if current > supported {
        return Err(AppError::new(
            ErrorCategory::Database,
            format!(
                "the download database was created by a newer version of SwiftFetch: \
                 its schema version {current} is beyond what this build supports \
                 (version {supported}); update SwiftFetch or restore a compatible \
                 database file instead of resuming with it"
            ),
        ));
    }
    for migration in migrations.iter().filter(|m| m.version > current) {
        apply_one(conn, migration).map_err(|err| {
            db_failure(
                &format!("migrating the schema to version {}", migration.version),
                err,
            )
        })?;
    }
    Ok(())
}

/// Applies a single migration atomically: DDL, the (re)created version
/// table, and the new version row share one transaction. `conn` owns the
/// transaction because `Connection::transaction` requires `&mut self`.
fn apply_one(conn: &mut Connection, migration: &Migration) -> Result<(), rusqlite::Error> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(migration.sql)?;
    transaction.execute_batch(&m_version_reset(migration.version))?;
    transaction.commit()
}

/// The version-bookkeeping batch executed inside the same transaction as
/// the migration DDL: create the table when fresh, then overwrite the
/// recorded version with the generation just applied.
fn m_version_reset(version: i64) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {VERSION_TABLE}(version INTEGER NOT NULL);\n\
         DELETE FROM {VERSION_TABLE};\n\
         INSERT INTO {VERSION_TABLE}(version) VALUES({version});"
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Migration, SCHEMA_VERSION};
    use crate::error::ErrorCategory;
    use crate::storage::repository::{NewDownload, Repository};

    /// An isolated, auto-removed scratch directory for one database file.
    struct ScratchDb(PathBuf);

    impl ScratchDb {
        fn new(tag: &str) -> Self {
            // Uniqueness without a dependency (mirrors the Phase 4
            // `ScratchDir`): pid plus a nanosecond stamp.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "swiftfetch-p5-{tag}-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir created");
            Self(dir)
        }

        /// The path of the database file inside this scratch directory.
        fn file(&self) -> PathBuf {
            self.0.join("queue.sqlite3")
        }

        /// The recorded schema version, read through a fresh connection.
        fn inspect(&self) -> i64 {
            let conn = rusqlite::Connection::open(self.file()).expect("inspect connection");
            super::version_of(&conn).expect("inspect version")
        }

        /// Whether a table exists in this scratch database.
        fn table_exists(&self, table: &str) -> bool {
            let conn = rusqlite::Connection::open(self.file()).expect("inspect connection");
            conn.table_exists(None::<&str>, table)
                .expect("inspect tables")
        }

        /// BLOB-typed columns across the record tables, for the metadata-only
        /// law (none may exist).
        fn blob_columns(&self) -> Vec<String> {
            let conn = rusqlite::Connection::open(self.file()).expect("inspect connection");
            let mut found = Vec::new();
            for table in ["downloads", "segments", "extractions", "schema_version"] {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info({table})"))
                    .expect("table info prepared");
                let types = stmt
                    .query_map([], |row| row.get::<_, String>(2))
                    .expect("column types mapped");
                for ty in types {
                    let ty = ty.expect("column type read");
                    if ty.eq_ignore_ascii_case("blob") {
                        found.push(format!("{table}.{ty}"));
                    }
                }
            }
            found
        }
    }

    impl Drop for ScratchDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample_destination(tag: &str) -> PathBuf {
        let scratch = std::env::temp_dir().join("swiftfetch-p5-sample-destinations");
        scratch.join(format!("{tag}.bin"))
    }

    #[test]
    fn shipped_migrations_are_well_formed() {
        use super::MIGRATIONS;
        assert!(
            !MIGRATIONS.is_empty(),
            "a release without migrations cannot build a schema"
        );
        let mut expected_version = 0;
        for migration in MIGRATIONS {
            expected_version += 1;
            assert_eq!(
                migration.version, expected_version,
                "migration versions must ascend strictly from 1"
            );
            assert!(
                !migration.sql.trim().is_empty(),
                "migration {} carries no SQL",
                migration.version
            );
        }
        assert_eq!(
            SCHEMA_VERSION, expected_version,
            "SCHEMA_VERSION must name the newest shipped migration"
        );
    }

    #[test]
    fn a_fresh_database_reaches_the_current_schema_version() {
        let scratch = ScratchDb::new("fresh-schema");
        Repository::open(&scratch.file()).expect("database opened");
        assert_eq!(scratch.inspect(), SCHEMA_VERSION, "stamped as current");
        for table in ["downloads", "segments", "extractions", "schema_version"] {
            assert!(scratch.table_exists(table), "table {table} must exist");
        }
    }

    #[test]
    fn opening_an_existing_database_is_idempotent() {
        let scratch = ScratchDb::new("reopen");
        {
            let repo = Repository::open(&scratch.file()).expect("first open");
            let id = repo
                .insert_download(
                    NewDownload {
                        url: "https://db.test/reopen".to_owned(),
                        destination: sample_destination("reopen"),
                        file_name: "reopen.bin".to_owned(),
                    },
                    1_700_000_000,
                )
                .expect("record inserted");
            assert_eq!(id, 1, "first record earns id 1");
        }
        let repo = Repository::open(&scratch.file()).expect("second open");
        assert_eq!(scratch.inspect(), SCHEMA_VERSION, "version stays intact");
        assert!(scratch.table_exists("downloads"), "tables stay intact");
        let record = repo.get_download(1).expect("read after reopen");
        assert_eq!(
            record.map(|record| record.url),
            Some("https://db.test/reopen".to_owned()),
            "records survive reopening: queue metadata persists between program runs"
        );
    }

    #[test]
    fn a_newer_schema_version_refuses_to_open() {
        let scratch = ScratchDb::new("newer-schema");
        let conn = rusqlite::Connection::open(scratch.file()).expect("raw connection");
        conn.execute_batch(
            "CREATE TABLE schema_version(version INTEGER NOT NULL);
             INSERT INTO schema_version(version) VALUES(99);",
        )
        .expect("foreign version staged");
        drop(conn);

        let err = Repository::open(&scratch.file()).expect_err("a future schema must not open");
        assert_eq!(err.category(), ErrorCategory::Database);
        assert!(
            err.message().contains("newer version"),
            "the message must be actionable: {}",
            err.message()
        );
    }

    #[test]
    fn a_failed_migration_rolls_back_completely() {
        let migrations = [
            Migration {
                version: 1,
                sql: "CREATE TABLE first(x INTEGER);",
            },
            Migration {
                version: 2,
                sql: "CREATE TABLE second(x INTEGER); INSERT INTO missing_table VALUES(1);",
            },
        ];
        let mut conn = rusqlite::Connection::open_in_memory().expect("memory database");
        let err = super::migrate_connection(&mut conn, &migrations)
            .expect_err("the broken migration must fail");
        assert_eq!(err.category(), ErrorCategory::Database);
        // The failed migration rolled back fully: its table is gone, the
        // version stays at the previously applied generation, and the
        // database is immediately usable again.
        assert!(
            !conn
                .table_exists(None::<&str>, "second")
                .expect("table check"),
            "no half-migration debris"
        );
        assert_eq!(
            super::version_of(&conn).expect("version after rollback"),
            1,
            "the version must stay at the last fully applied generation"
        );
        assert!(
            conn.table_exists(None::<&str>, "first")
                .expect("table check"),
            "applied migrations persist"
        );

        // Re-running after rollback works: recovery is idempotent.
        super::migrate_connection(&mut conn, &migrations[..1])
            .expect("reapplication after rollback");
        assert_eq!(super::version_of(&conn).expect("version after reapply"), 1);
    }

    #[test]
    fn a_fresh_database_whose_initialization_fails_stays_empty() {
        let broken = [Migration {
            version: 1,
            sql: "CREATE TABLE first(x INTEGER); INSERT INTO missing_table VALUES(1);",
        }];
        let mut conn = rusqlite::Connection::open_in_memory().expect("memory database");
        super::migrate_connection(&mut conn, &broken)
            .expect_err("broken fresh initialization fails");
        // Nothing leaked: both the record table and the version table are
        // part of the same transaction, whose rollback leaves generation 0
        // — so the next startup re-runs migration 1 from a clean slate.
        assert!(!conn
            .table_exists(None::<&str>, "first")
            .expect("table check"));
        assert!(!conn
            .table_exists(None::<&str>, "schema_version")
            .expect("table check"));
        assert_eq!(super::version_of(&conn).expect("version of failed init"), 0);

        let clean = [Migration {
            version: 1,
            sql: "CREATE TABLE first(x INTEGER);",
        }];
        super::migrate_connection(&mut conn, &clean).expect("re-run from the clean slate");
        assert_eq!(super::version_of(&conn).expect("version after re-run"), 1);
    }

    #[test]
    fn downloadable_metadata_stays_blob_free() {
        let scratch = ScratchDb::new("no-blobs");
        Repository::open(&scratch.file()).expect("database opened");
        let blobs = scratch.blob_columns();
        assert!(
            blobs.is_empty(),
            "BLOB columns would invite storing file contents: {blobs:?}"
        );
    }

    /// Guard for the shared wrapper used by row mappers: a typed row
    /// rejection must stay visible when it rides inside a rusqlite
    /// conversion failure.
    #[test]
    fn conversion_failures_keep_the_typed_message_visible() {
        let reject = crate::error::AppError::new(
            ErrorCategory::Database,
            "unreadable data in the download database",
        );
        let wrapped = rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(reject),
        );
        let surfaced = format!("{wrapped}");
        assert!(
            surfaced.contains("unreadable data"),
            "the inner rejection must not vanish: {surfaced}"
        );
    }
}
