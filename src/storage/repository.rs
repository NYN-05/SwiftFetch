//! SQLite repository for queue and recovery metadata (Phase 5, storage layer).
//!
//! Architecture §9: SQLite is the source of truth for queue and recovery
//! metadata, never for downloaded file contents — the record tables carry
//! metadata columns only (the no-BLOB rule is locked by
//! [`crate::storage::schema`]'s tests). Operating rules for this module:
//!
//! - every public operation opens and commits its own database work and
//!   returns; there is no API that keeps a transaction open across a
//!   callback, a network transfer, or a long file write;
//! - lifecycle transitions are validated against the ADR-0018 graph before
//!   any row is written, and a rejected request writes nothing;
//! - all database failures surface as typed [`ErrorCategory::Database`]
//!   errors with the original error preserved as the hidden source;
//! - `created_at`/`updated_at` are caller-supplied unix timestamps (seconds)
//!   so later recovery phases can stamp consistent, testable values.
//!
//! The database lives in the user's local application-data directory,
//! separate from any download directory (ADR-0017). The repository's
//! connection is serialized behind a mutex: one controlled database-writer
//! path is sufficient for this single-user application.

#![allow(dead_code)] // wired into production paths from Phase 6 onward (ADR-0017)

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::error::{AppError, ErrorCategory};
use crate::storage::schema;

/// The SQLite database's reserved file name in the app-data directory.
const DATABASE_FILE_NAME: &str = "swiftfetch.sqlite3";

/// How long a statement waits for a competing writer before reporting the
/// database busy. Relevant only for cross-process contention (two
/// SwiftFetch processes starting alongside each other).
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Persistence state of a download record (`DOCS/architecture.md` §7).
///
/// Transitions are validated by [`DownloadState::allows`]; the graph is a
/// recorded contract (ADR-0018) and later phases may only add edges by an
/// explicit, tested decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadState {
    Queued,
    Probing,
    Downloading,
    Paused,
    Retrying,
    Finalizing,
    Extracting,
    Completed,
    Failed,
    Cancelled,
}

impl DownloadState {
    /// Every state, declaration order kept stable by tests.
    pub(crate) const ALL: [DownloadState; 10] = [
        DownloadState::Queued,
        DownloadState::Probing,
        DownloadState::Downloading,
        DownloadState::Paused,
        DownloadState::Retrying,
        DownloadState::Finalizing,
        DownloadState::Extracting,
        DownloadState::Completed,
        DownloadState::Failed,
        DownloadState::Cancelled,
    ];

    /// Stable, kebab-case identifier stored in the `status` column.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            DownloadState::Queued => "queued",
            DownloadState::Probing => "probing",
            DownloadState::Downloading => "downloading",
            DownloadState::Paused => "paused",
            DownloadState::Retrying => "retrying",
            DownloadState::Finalizing => "finalizing",
            DownloadState::Extracting => "extracting",
            DownloadState::Completed => "completed",
            DownloadState::Failed => "failed",
            DownloadState::Cancelled => "cancelled",
        }
    }

    /// Whether the lifecycle permits moving `from` to `to`.
    ///
    /// Edges, by phase-applicable flow (ADR-0018):
    /// - `queued` starts work (`probing`) or is abandoned (`cancelled`);
    /// - `probing` yields `downloading`, a permanent `failed`, or `cancelled`;
    /// - `downloading` proceeds to `finalizing`, hands off transient errors to
    ///   `retrying`, is user-controlled via `paused`, or ends as
    ///   `failed`/`cancelled`;
    /// - `paused` resumes into `downloading` or is abandoned;
    /// - `retrying` re-enters `downloading`, may be paused or abandoned, or
    ///   reports exhaustion as `failed`;
    /// - `finalizing` lands as `completed` (rename succeeded) or `failed`;
    /// - `completed` may start extraction, and `extracting` always returns to
    ///   `completed` — extraction success/failure is recorded in the
    ///   `extractions` table, never in the download's own state, because a
    ///   successfully downloaded file stays successfully downloaded;
    /// - `failed` re-enters the queue only through the Phase 11 `retry`
    ///   command (`failed → queued`, the explicit, tested requeue edge the
    ///   graph reserved for that phase; `retry_count` is kept as history);
    /// - `cancelled` remains terminal — the user gave it up, and no command
    ///   resurrects a cancelled record.
    ///
    /// Not permitted: skipping states, re-entering an earlier state from a
    /// later one (except the `completed` ⇄ `extracting` round trip), closing
    /// with `cancelled` after finalization began, and a state moving to
    /// itself (a no-transition write changes nothing). Later phases add
    /// edges explicitly when their flows need them.
    pub(crate) const fn allows(from: Self, to: Self) -> bool {
        matches!(
            (from, to),
            (DownloadState::Queued, DownloadState::Probing)
                | (DownloadState::Queued, DownloadState::Cancelled)
                | (DownloadState::Probing, DownloadState::Downloading)
                | (DownloadState::Probing, DownloadState::Failed)
                | (DownloadState::Probing, DownloadState::Cancelled)
                | (DownloadState::Downloading, DownloadState::Finalizing)
                | (DownloadState::Downloading, DownloadState::Retrying)
                | (DownloadState::Downloading, DownloadState::Paused)
                | (DownloadState::Downloading, DownloadState::Failed)
                | (DownloadState::Downloading, DownloadState::Cancelled)
                | (DownloadState::Paused, DownloadState::Downloading)
                | (DownloadState::Paused, DownloadState::Cancelled)
                | (DownloadState::Retrying, DownloadState::Downloading)
                | (DownloadState::Retrying, DownloadState::Paused)
                | (DownloadState::Retrying, DownloadState::Failed)
                | (DownloadState::Retrying, DownloadState::Cancelled)
                | (DownloadState::Finalizing, DownloadState::Completed)
                | (DownloadState::Finalizing, DownloadState::Failed)
                | (DownloadState::Failed, DownloadState::Queued)
                | (DownloadState::Completed, DownloadState::Extracting)
                | (DownloadState::Extracting, DownloadState::Completed)
        )
    }

    /// Parses the stored `status` text.
    fn parse(text: &str) -> Result<Self, AppError> {
        match text {
            "queued" => Ok(Self::Queued),
            "probing" => Ok(Self::Probing),
            "downloading" => Ok(Self::Downloading),
            "paused" => Ok(Self::Paused),
            "retrying" => Ok(Self::Retrying),
            "finalizing" => Ok(Self::Finalizing),
            "extracting" => Ok(Self::Extracting),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(AppError::new(
                ErrorCategory::Database,
                format!(
                    "the download database contains the unrecognized download state \
                     ({other:?}); the file may be corrupt or belong to another program"
                ),
            )),
        }
    }
}

/// State of one segment record. Phase 8 adds the tested transition graph
/// ([`SegmentState::can_transition_to`]); Phase 5 stored the state
/// round-trippably.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentState {
    Pending,
    Downloading,
    Completed,
    Failed,
}

impl SegmentState {
    /// Stable identifier stored in the `state` column.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            SegmentState::Pending => "pending",
            SegmentState::Downloading => "downloading",
            SegmentState::Completed => "completed",
            SegmentState::Failed => "failed",
        }
    }

    /// Whether one segment state may follow another (Phase 8's graph).
    ///
    /// - a pending span is either claimed by a worker or retired;
    /// - a downloading span advances progress in place, completes, or is
    ///   retired (a failed sibling / fallback / restart abandons the plan);
    /// - a completed span is terminal except for retirement as `failed` when
    ///   a later attempt supersedes the plan (rows are never deleted);
    /// - a failed span is terminal.
    ///
    /// Same-state updates are always permitted: they are progress claims,
    /// not transitions.
    pub(crate) const fn can_transition_to(self, to: SegmentState) -> bool {
        if matches!(
            (self, to),
            (Self::Pending, Self::Pending)
                | (Self::Downloading, Self::Downloading)
                | (Self::Completed, Self::Completed)
                | (Self::Failed, Self::Failed)
        ) {
            return true;
        }
        matches!(
            (self, to),
            (Self::Pending, Self::Downloading | Self::Failed)
                | (Self::Downloading, Self::Completed | Self::Failed)
                | (Self::Completed, Self::Failed)
        )
    }

    fn parse(text: &str) -> Result<Self, AppError> {
        match text {
            "pending" => Ok(Self::Pending),
            "downloading" => Ok(Self::Downloading),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            other => Err(AppError::new(
                ErrorCategory::Database,
                format!(
                    "the download database contains the unrecognized segment state \
                     ({other:?}); the file may be corrupt or belong to another program"
                ),
            )),
        }
    }
}

/// State of one extraction record. Extraction semantics (including its
/// transition rules) arrive with the extraction engine (Phase 10); Phase 5
/// stores the state round-trippably, independent of the download status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtractionState {
    Queued,
    Extracting,
    Completed,
    Failed,
}

impl ExtractionState {
    /// Stable identifier stored in the `state` column.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            ExtractionState::Queued => "queued",
            ExtractionState::Extracting => "extracting",
            ExtractionState::Completed => "completed",
            ExtractionState::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Result<Self, AppError> {
        match text {
            "queued" => Ok(Self::Queued),
            "extracting" => Ok(Self::Extracting),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            other => Err(AppError::new(
                ErrorCategory::Database,
                format!(
                    "the download database contains the unrecognized extraction state \
                     ({other:?}); the file may be corrupt or belong to another program"
                ),
            )),
        }
    }
}

/// Parses a stored error-category identifier back to its category.
fn parse_error_category(text: &str) -> Result<ErrorCategory, AppError> {
    ErrorCategory::ALL
        .iter()
        .copied()
        .find(|category| category.as_str() == text)
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::Database,
                format!(
                    "the download database contains the unrecognized error category \
                     ({text:?}); the file may be corrupt or belong to another program"
                ),
            )
        })
}

/// A persisted download record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownloadRecord {
    pub(crate) id: i64,
    pub(crate) url: String,
    pub(crate) effective_url: Option<String>,
    pub(crate) destination: PathBuf,
    pub(crate) file_name: String,
    pub(crate) status: DownloadState,
    pub(crate) expected_size: Option<u64>,
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
    pub(crate) retry_count: u64,
    pub(crate) error_category: Option<ErrorCategory>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

/// A persisted segment record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentRecord {
    pub(crate) id: i64,
    pub(crate) download_id: i64,
    pub(crate) min_byte: u64,
    pub(crate) max_byte: u64,
    pub(crate) state: SegmentState,
    pub(crate) received_bytes: u64,
}

/// A persisted extraction record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtractionRecord {
    pub(crate) id: i64,
    pub(crate) download_id: i64,
    pub(crate) archive_path: PathBuf,
    pub(crate) destination: PathBuf,
    pub(crate) state: ExtractionState,
    pub(crate) error_category: Option<ErrorCategory>,
    pub(crate) error_message: Option<String>,
}

/// The data needed to queue a download. New records always start `queued`:
/// nothing has happened to the transfer yet, and the lifecycle is truthful
/// from the first statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewDownload {
    pub(crate) url: String,
    pub(crate) destination: PathBuf,
    pub(crate) file_name: String,
}

/// Facts a probe learns about a remote object. `None` fields are left
/// untouched as facts are only learned, not unlearned; a later probe may
/// replace individual known facts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TransferFacts {
    pub(crate) effective_url: Option<String>,
    pub(crate) expected_size: Option<u64>,
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
}

/// The data needed to plan one segment of a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewSegment {
    pub(crate) min_byte: u64,
    pub(crate) max_byte: u64,
    pub(crate) state: SegmentState,
    pub(crate) received_bytes: u64,
}

/// The data needed to plan one extraction job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewExtraction {
    pub(crate) archive_path: PathBuf,
    pub(crate) destination: PathBuf,
}

/// Resolves the database path from a local application-data root.
///
/// `None`/empty means the environment could not name the directory; the
/// failure is typed and actionable rather than silently writing somewhere
/// arbitrary. Kept pure so tests exercise the resolution without touching
/// process environment variables.
pub(crate) fn database_path_in(local_root: Option<&OsStr>) -> Result<PathBuf, AppError> {
    let Some(root) = local_root.filter(|value| !value.is_empty()) else {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "could not locate the user's local application-data directory: the \
             LOCALAPPDATA environment variable is missing or empty, so SwiftFetch \
             has nowhere well-defined to keep its download database",
        ));
    };
    Ok(Path::new(root).join("SwiftFetch").join(DATABASE_FILE_NAME))
}

/// Resolves the database path when an explicit location is requested
/// (`SWIFT_FETCH_DATABASE`, the ADR-0011 env-var form of the crate name).
///
/// A deliberate explicit location wins over the platform default: this is
/// how automated end-to-end tests keep the persistence layer hermetic and
/// how a second profile on one machine can be separated deliberately. An
/// empty value is not a location and falls through to the platform
/// resolution (which still refuses to guess). Kept pure so tests exercise
/// the precedence without racing process environment variables.
pub(crate) fn database_path(
    explicit: Option<&OsStr>,
    local_root: Option<&OsStr>,
) -> Result<PathBuf, AppError> {
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    database_path_in(local_root)
}

/// Resolves the database path from the platform's local application-data
/// directory (`%LOCALAPPDATA%` on Windows, the platform SwiftFetch treats
/// as first-class), honoring an explicit `SWIFT_FETCH_DATABASE` override.
pub(crate) fn default_database_path() -> Result<PathBuf, AppError> {
    let explicit = std::env::var_os("SWIFT_FETCH_DATABASE");
    let local = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty());
    database_path(explicit.as_deref(), local.as_deref())
}

/// The metadata repository: one connection to one SQLite file, serialized
/// behind a mutex so every write is short, ordered, and observable.
///
/// Cloning shares the same connection handle (the mutex is behind an `Arc`),
/// which is what lets Phase 8's spawned segment workers coordinate their
/// per-segment rows with the transfer engine without opening a second
/// connection or passing borrowed state into `'static` tasks.
#[derive(Debug, Clone)]
pub(crate) struct Repository {
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl Repository {
    /// Opens (creating when absent) the database at `path` and brings its
    /// schema to the current generation.
    ///
    /// The parent directory is created when missing; an inaccessible
    /// directory is a typed [`ErrorCategory::Filesystem`] failure, while
    /// SQLite- and schema-level problems are
    /// [`ErrorCategory::Database`] ones. WAL journaling and foreign-key
    /// enforcement are enabled for the connection; schema migrations run
    /// per [`crate::storage::schema`]'s protocol.
    pub(crate) fn open(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    AppError::new(
                        ErrorCategory::Filesystem,
                        format!(
                            "the download database's directory {} could not be created \
                             or is not a directory: {err}",
                            parent.display()
                        ),
                    )
                    .with_source(err)
                })?;
            }
        }
        let mut conn = Connection::open(path).map_err(|err| {
            AppError::new(
                ErrorCategory::Database,
                format!(
                    "the download database at {} could not be opened: {err}",
                    path.display()
                ),
            )
            .with_source(err)
        })?;
        configure(&conn)?;
        schema::migrate_connection(&mut conn, schema::MIGRATIONS)?;
        Ok(Self {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// The file this repository writes (`open_default` callers surface it in
    /// diagnostics and the future `status` command).
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Locks the single database writer; a panic inside any operation
    /// poisons it and every later operation fails honestly instead of
    /// wrapping parallel work around corrupted state.
    fn lock(&self) -> Result<MutexGuard<'_, Connection>, AppError> {
        self.conn.lock().map_err(|_| {
            AppError::new(
                ErrorCategory::Database,
                "the download database writer panicked during an earlier operation; \
                 restart SwiftFetch so startup recovery can inspect the database",
            )
        })
    }

    /// Runs one multi-statement write inside a fresh immediate transaction,
    /// mapped per repository contract. The transaction commits before the
    /// method returns and never wraps network or file work (no public API
    /// takes a closure spanning one).
    fn write_transaction<T, F>(&self, operation: &'static str, body: F) -> Result<T, AppError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T, WriteFailure>,
    {
        let mut conn = self.lock()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| schema::db_failure(operation, err))?;
        match body(&transaction) {
            Ok(value) => {
                transaction
                    .commit()
                    .map_err(|err| schema::db_failure(operation, err))?;
                Ok(value)
            }
            Err(WriteFailure::Sql(err)) => Err(schema::db_failure(operation, err)),
            Err(WriteFailure::Rejected(err)) => Err(err),
        }
    }
}

/// Failure kind for repository write bodies: a rusqlite error, or a domain
/// rejection (validation, missing record) whose typed message must roll the
/// transaction back unchanged.
enum WriteFailure {
    Sql(rusqlite::Error),
    Rejected(AppError),
}

impl From<rusqlite::Error> for WriteFailure {
    fn from(err: rusqlite::Error) -> Self {
        WriteFailure::Sql(err)
    }
}

impl From<AppError> for WriteFailure {
    fn from(err: AppError) -> Self {
        WriteFailure::Rejected(err)
    }
}

/// Runs one single-statement write, mapped per repository contract.
fn write<T, F>(repo: &Repository, operation: &'static str, body: F) -> Result<T, AppError>
where
    F: FnOnce(&mut Connection) -> Result<T, WriteFailure>,
{
    let mut conn = repo.lock()?;
    body(&mut conn).map_err(|failure| match failure {
        WriteFailure::Sql(err) => schema::db_failure(operation, err),
        WriteFailure::Rejected(err) => err,
    })
}

/// Runs one read, mapped per repository contract.
fn read<T, F>(repo: &Repository, operation: &'static str, body: F) -> Result<T, AppError>
where
    F: FnOnce(&Connection) -> rusqlite::Result<T>,
{
    let conn = repo.lock()?;
    body(&conn).map_err(|err| schema::db_failure(operation, err))
}

/// Enables foreign keys, switching to WAL journaling, and installs the
/// lock-wait budget before any statements (including migrations) run.
fn configure(conn: &Connection) -> Result<(), AppError> {
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|err| schema::db_failure("enabling foreign key enforcement", err))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|err| schema::db_failure("selecting write-ahead logging", err))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|err| schema::db_failure("configuring lock waits", err))?;
    Ok(())
}

impl Repository {
    /// Inserts a queued download record and returns its id. Created and
    /// updated stamps both carry `ts` (unix seconds, caller-supplied).
    pub(crate) fn insert_download(&self, spec: NewDownload, ts: u64) -> Result<i64, AppError> {
        let destination = path_text("destination", &spec.destination)?;
        let stamp = seconds_stamp(ts)?;
        write(self, "inserting a download record", |conn| {
            conn.execute(
                "INSERT INTO downloads \
                 (url, destination, file_name, status, retry_count, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
                params![
                    spec.url,
                    destination,
                    spec.file_name,
                    DownloadState::Queued.as_str(),
                    stamp
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Reads one download record by id; missing ids read as `None`.
    pub(crate) fn get_download(&self, id: i64) -> Result<Option<DownloadRecord>, AppError> {
        read(self, "reading a download record", |conn| {
            conn.query_row(
                &format!(
                    "SELECT {} FROM downloads WHERE id = ?1",
                    schema::DOWNLOAD_COLUMNS
                ),
                [id],
                download_from_row,
            )
            .optional()
        })
    }

    /// Reads all download records ordered by insertion.
    pub(crate) fn list_downloads(&self) -> Result<Vec<DownloadRecord>, AppError> {
        read(self, "listing download records", |conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {} FROM downloads ORDER BY id",
                schema::DOWNLOAD_COLUMNS
            ))?;
            let rows = statement.query_map([], download_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Reads download records whose status matches, ordered by insertion.
    pub(crate) fn list_downloads_by_status(
        &self,
        status: DownloadState,
    ) -> Result<Vec<DownloadRecord>, AppError> {
        read(self, "listing download records", |conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {} FROM downloads WHERE status = ?1 ORDER BY id",
                schema::DOWNLOAD_COLUMNS
            ))?;
            let rows = statement.query_map([status.as_str()], download_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Moves a download record through one validated lifecycle transition.
    ///
    /// The current status is read and validated inside the same immediate
    /// transaction that writes the new one (`DOCS/phases.md` phase 5:
    /// "Use transactions for database-level changes that must be
    /// consistent"), so an invalid request changes nothing rather than
    /// leaving a partially updated record. The record must exist, must not
    /// already be in `to`, and the edge `current → to` must be permitted by
    /// [`DownloadState::allows`]. Entering `failed` requires an error
    /// category; other states reject one, keeping the column's meaning
    /// sharp ("why this record failed"). `updated_at` becomes `ts`.
    pub(crate) fn update_status(
        &self,
        id: i64,
        to: DownloadState,
        ts: u64,
        error: Option<ErrorCategory>,
    ) -> Result<(), AppError> {
        let stamp = seconds_stamp(ts)?;
        self.write_transaction("updating a download record", |transaction| {
            let current_text: Option<String> = transaction
                .query_row("SELECT status FROM downloads WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            let Some(current_text) = current_text else {
                return Err(not_found("download record", id).into());
            };
            let current = DownloadState::parse(&current_text).map_err(WriteFailure::Rejected)?;
            if current == to {
                return Err(rejection(format!(
                    "the download record {id} is already in the {} state; nothing \
                     transitions from a state to itself",
                    current.as_str()
                )));
            }
            if !DownloadState::allows(current, to) {
                return Err(rejection(format!(
                    "the download record {id} cannot move from {} to {}: the \
                     download lifecycle does not allow this transition",
                    current.as_str(),
                    to.as_str()
                )));
            }
            if to == DownloadState::Failed && error.is_none() {
                return Err(rejection(
                    "entering the failed state requires an error category: a failed \
                     download must record why",
                ));
            }
            if to != DownloadState::Failed && error.is_some() {
                return Err(rejection(
                    "an error category can only be recorded when a download enters \
                     the failed state",
                ));
            }
            let changed = transaction.execute(
                "UPDATE downloads \
                 SET status = ?2, error_category = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, to.as_str(), error.map(ErrorCategory::as_str), stamp],
            )?;
            if changed == 0 {
                return Err(not_found("download record", id).into());
            }
            Ok(())
        })
    }

    /// Records facts learned from a probe. `None` fields keep their current
    /// value; `updated_at` becomes `ts`.
    pub(crate) fn update_transfer_facts(
        &self,
        id: i64,
        facts: &TransferFacts,
        ts: u64,
    ) -> Result<(), AppError> {
        let expected_size = facts
            .expected_size
            .map(|size| numeric_value("expected size", size))
            .transpose()?;
        let stamp = seconds_stamp(ts)?;
        self.write_transaction("updating a download record", |transaction| {
            let changed = transaction.execute(
                "UPDATE downloads \
                 SET effective_url = COALESCE(?2, effective_url), \
                     expected_size = COALESCE(?3, expected_size), \
                     etag          = COALESCE(?4, etag), \
                     last_modified = COALESCE(?5, last_modified), \
                     updated_at    = ?6 \
                 WHERE id = ?1",
                params![
                    id,
                    facts.effective_url,
                    expected_size,
                    facts.etag,
                    facts.last_modified,
                    stamp
                ],
            )?;
            if changed == 0 {
                return Err(not_found("download record", id).into());
            }
            Ok(())
        })
    }

    /// Overwrites the record's retry count (`DOCS/phases.md` phase 5: retry
    /// counts persist with the other transfer fields; the retry policy that
    /// drives it belongs to Phase 6). `updated_at` becomes `ts`.
    pub(crate) fn set_retry_count(&self, id: i64, count: u64, ts: u64) -> Result<(), AppError> {
        let retry_count = numeric_value("retry count", count)?;
        let stamp = seconds_stamp(ts)?;
        self.write_transaction("updating a download record", |transaction| {
            let changed = transaction.execute(
                "UPDATE downloads SET retry_count = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, retry_count, stamp],
            )?;
            if changed == 0 {
                return Err(not_found("download record", id).into());
            }
            Ok(())
        })
    }

    /// Corrects the record's destination when finalization picked a
    /// deterministic `(n)`-suffixed sibling instead of the planned name
    /// (architecture §9: the record's destination points at the file that
    /// actually exists, so recovery trusts it blindly). `updated_at`
    /// becomes `ts`.
    pub(crate) fn set_destination(
        &self,
        id: i64,
        destination: &Path,
        ts: u64,
    ) -> Result<(), AppError> {
        let destination = path_text("destination", destination)?;
        let stamp = seconds_stamp(ts)?;
        self.write_transaction("updating a download record", |transaction| {
            let changed = transaction.execute(
                "UPDATE downloads SET destination = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, destination, stamp],
            )?;
            if changed == 0 {
                return Err(not_found("download record", id).into());
            }
            Ok(())
        })
    }

    /// Corrects the planned file name *and* destination of a record whose
    /// first probe learned a different effective URL than the one that
    /// queued it (Phase 7 pristine refresh, ADR-0020). Only a record with
    /// no confirmed facts and no engine-owned partial may be corrected this
    /// way — the engine owns that policy; the column write is one
    /// transaction so a crash can never leave the name and the path
    /// disagreeing. `updated_at` becomes `ts`.
    pub(crate) fn set_planned_name(
        &self,
        id: i64,
        file_name: &str,
        destination: &Path,
        ts: u64,
    ) -> Result<(), AppError> {
        let destination = path_text("destination", destination)?;
        let stamp = seconds_stamp(ts)?;
        self.write_transaction("updating a download record", |transaction| {
            let changed = transaction.execute(
                "UPDATE downloads SET file_name = ?2, destination = ?3, updated_at = ?4 \
                 WHERE id = ?1",
                params![id, file_name, destination, stamp],
            )?;
            if changed == 0 {
                return Err(not_found("download record", id).into());
            }
            Ok(())
        })
    }

    /// Inserts a segment plan and returns its id.
    ///
    /// Ranges are inclusive byte spans; an inverted range, or a progress
    /// claim larger than the range, is rejected before anything is written
    /// (a metadata claim that overshoots a byte range would start recovery
    /// at corrupt trust values later).
    pub(crate) fn insert_segment(
        &self,
        download_id: i64,
        spec: NewSegment,
    ) -> Result<i64, AppError> {
        if spec.max_byte < spec.min_byte {
            return Err(AppError::new(
                ErrorCategory::Input,
                format!(
                    "the segment byte range is inverted: start {} exceeds end {}",
                    spec.min_byte, spec.max_byte
                ),
            ));
        }
        let Some(span) = spec
            .max_byte
            .checked_sub(spec.min_byte)
            .and_then(|difference| difference.checked_add(1))
        else {
            return Err(AppError::new(
                ErrorCategory::Input,
                "the segment byte range spans more than the addressable byte space",
            ));
        };
        if spec.received_bytes > span {
            return Err(AppError::new(
                ErrorCategory::Input,
                format!(
                    "confirmed segment progress ({}) exceeds the segment's byte range \
                     ({} bytes): records must not claim bytes that were never transferred",
                    spec.received_bytes, span
                ),
            ));
        }
        let min_byte = numeric_value("segment start", spec.min_byte)?;
        let max_byte = numeric_value("segment end", spec.max_byte)?;
        let received = numeric_value("confirmed segment progress", spec.received_bytes)?;
        write(self, "inserting a segment record", |conn| {
            conn.execute(
                "INSERT INTO segments \
                 (download_id, min_byte, max_byte, state, received_bytes) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    download_id,
                    min_byte,
                    max_byte,
                    spec.state.as_str(),
                    received
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Updates a segment's state and its *confirmed* progress: progress may
    /// not exceed the segment's planned byte range (inclusively counting
    /// bytes validated at write time, not merely received from the network).
    ///
    /// The transition must follow [`SegmentState::can_transition_to`]; a
    /// rejected transition writes nothing and reports the current and
    /// requested states, so a caller can never silently rewrite a finished
    /// or retired span into another state.
    pub(crate) fn update_segment(
        &self,
        segment_id: i64,
        to: SegmentState,
        received: u64,
    ) -> Result<(), AppError> {
        let received_raw = numeric_value("confirmed segment progress", received)?;
        self.write_transaction("updating a segment record", |transaction| {
            let row: Option<(u64, u64, String)> = transaction
                .query_row(
                    "SELECT min_byte, max_byte, state FROM segments WHERE id = ?1",
                    [segment_id],
                    |row| {
                        let min: i64 = row.get(0)?;
                        let max: i64 = row.get(1)?;
                        let min = nonneg_required(0, "segment start", Ok(min))?;
                        let max = nonneg_required(1, "segment end", Ok(max))?;
                        Ok((min, max, row.get(2)?))
                    },
                )
                .optional()?;
            let Some((min, max, current)) = row else {
                return Err(not_found("segment record", segment_id).into());
            };
            let current = SegmentState::parse(&current)?;
            if !current.can_transition_to(to) {
                return Err(AppError::new(
                    ErrorCategory::Database,
                    format!(
                        "the segment record {segment_id} cannot move from {} to {}; \
                         the segment lifecycle graph has no such edge",
                        current.as_str(),
                        to.as_str()
                    ),
                )
                .into());
            }
            let span = (max - min) + 1;
            if received > span {
                return Err(AppError::new(
                    ErrorCategory::Input,
                    format!(
                        "confirmed segment progress ({received}) exceeds the segment's \
                         byte range ({span} bytes): records must not claim bytes that \
                         were never transferred"
                    ),
                )
                .into());
            }
            let changed = transaction.execute(
                "UPDATE segments SET state = ?2, received_bytes = ?3 WHERE id = ?1",
                params![segment_id, to.as_str(), received_raw],
            )?;
            if changed == 0 {
                return Err(not_found("segment record", segment_id).into());
            }
            Ok(())
        })
    }

    /// Lists the segments planned for one download, ordered by insertion.
    pub(crate) fn list_segments(&self, download_id: i64) -> Result<Vec<SegmentRecord>, AppError> {
        read(self, "listing segment records", |conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {} FROM segments WHERE download_id = ?1 ORDER BY id",
                schema::SEGMENT_COLUMNS
            ))?;
            let rows = statement.query_map([download_id], segment_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Inserts an extraction job for one download and returns its id.
    pub(crate) fn insert_extraction(
        &self,
        download_id: i64,
        spec: NewExtraction,
    ) -> Result<i64, AppError> {
        let archive = path_text("archive", &spec.archive_path)?;
        let destination = path_text("destination", &spec.destination)?;
        write(self, "inserting an extraction record", |conn| {
            conn.execute(
                "INSERT INTO extractions (download_id, archive_path, destination, state) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    download_id,
                    archive,
                    destination,
                    ExtractionState::Queued.as_str()
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Updates an extraction record's state and error details. Entering
    /// `failed` requires details (category + message); other states carry
    /// none. Extraction state semantics formalize in Phase 10; until then
    /// any non-failed contract is honored as written.
    pub(crate) fn update_extraction(
        &self,
        id: i64,
        to: ExtractionState,
        error: Option<(ErrorCategory, String)>,
    ) -> Result<(), AppError> {
        if to == ExtractionState::Failed && error.is_none() {
            return Err(AppError::new(
                ErrorCategory::Database,
                "entering the failed state requires extraction error details \
                 (category and message)",
            ));
        }
        let (category, message) = error.map_or((None, None), |(category, message)| {
            (Some(category.as_str()), Some(message))
        });
        write(self, "updating an extraction record", |conn| {
            let changed = conn.execute(
                "UPDATE extractions SET state = ?2, error_category = ?3, error_message = ?4 \
                 WHERE id = ?1",
                params![id, to.as_str(), category, message],
            )?;
            if changed == 0 {
                return Err(not_found("extraction record", id).into());
            }
            Ok(())
        })
    }

    /// Reads one extraction record by id; missing ids read as `None`.
    pub(crate) fn get_extraction(&self, id: i64) -> Result<Option<ExtractionRecord>, AppError> {
        read(self, "reading an extraction record", |conn| {
            conn.query_row(
                &format!(
                    "SELECT {} FROM extractions WHERE id = ?1",
                    schema::EXTRACTION_COLUMNS
                ),
                [id],
                extraction_from_row,
            )
            .optional()
        })
    }

    /// Lists the extraction records of one download, ordered by insertion.
    pub(crate) fn list_extractions(
        &self,
        download_id: i64,
    ) -> Result<Vec<ExtractionRecord>, AppError> {
        read(self, "listing extraction records", |conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {} FROM extractions WHERE download_id = ?1 ORDER BY id",
                schema::EXTRACTION_COLUMNS
            ))?;
            let rows = statement.query_map([download_id], extraction_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }
}

/// `AppError` rejections routed through a transaction body.
fn rejection(message: impl Into<String>) -> WriteFailure {
    WriteFailure::Rejected(AppError::new(ErrorCategory::Database, message))
}

/// The typed "record missing" rejection.
fn not_found(what: &str, id: i64) -> AppError {
    AppError::new(
        ErrorCategory::Database,
        format!("the {what} {id} was not found in the download database"),
    )
}

/// Converts a byte magnitude (or retry count/timestamp) into SQLite's
/// INTEGER range.
fn numeric_value(input: &str, bytes: u64) -> Result<i64, AppError> {
    i64::try_from(bytes).map_err(|_| {
        AppError::new(
            ErrorCategory::Input,
            format!(
                "{input} ({bytes}) exceeds what the metadata database can store \
                 (up to 9223372036854775807)"
            ),
        )
    })
}

/// Converts the caller-supplied unix-seconds stamp.
fn seconds_stamp(ts: u64) -> Result<i64, AppError> {
    numeric_value("timestamp", ts)
}

/// Converts a path into storable TEXT metadata.
fn path_text(input: &str, path: &Path) -> Result<String, AppError> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        AppError::new(
            ErrorCategory::Input,
            format!(
                "the {input} path {} is not valid UTF-8 and cannot be stored as \
                 database metadata",
                path.display()
            ),
        )
    })
}

/// Wraps a typed row-read rejection into a rusqlite conversion failure so
/// it carries through `query_map`/`query_row` and keeps its message.
fn corrupt_column(column: usize, column_type: Type, err: AppError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, column_type, Box::new(err))
}

/// Reads a required non-negative stored integer.
fn nonneg_required(
    column: usize,
    kind: &'static str,
    fetched: rusqlite::Result<i64>,
) -> rusqlite::Result<u64> {
    let value = fetched?;
    if value < 0 {
        return Err(corrupt_column(
            column,
            Type::Integer,
            AppError::new(
                ErrorCategory::Database,
                format!(
                    "the download database contains a negative {kind} value ({value}); \
                     the file may be corrupt or belong to another program"
                ),
            ),
        ));
    }
    Ok(value as u64)
}

/// Reads an optional non-negative stored integer.
fn nonneg_optional(
    column: usize,
    kind: &'static str,
    fetched: rusqlite::Result<Option<i64>>,
) -> rusqlite::Result<Option<u64>> {
    match fetched? {
        None => Ok(None),
        Some(value) => Ok(Some(nonneg_required(column, kind, Ok(value))?)),
    }
}

/// Parses a required textual column through a domain parser.
fn parsed_required<T, P>(
    column: usize,
    parse: P,
    fetched: rusqlite::Result<String>,
) -> rusqlite::Result<T>
where
    P: Fn(&str) -> Result<T, AppError>,
{
    let text = fetched?;
    parse(&text).map_err(|err| corrupt_column(column, Type::Text, err))
}

/// Parses an optional textual column through a domain parser.
fn parsed_optional<T, P>(
    column: usize,
    parse: P,
    fetched: rusqlite::Result<Option<String>>,
) -> rusqlite::Result<Option<T>>
where
    P: Fn(&str) -> Result<T, AppError>,
{
    match fetched? {
        None => Ok(None),
        Some(text) => Ok(Some(parsed_required(column, parse, Ok(text))?)),
    }
}

/// Maps one `downloads` row positionally over [`schema::DOWNLOAD_COLUMNS`].
fn download_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DownloadRecord> {
    Ok(DownloadRecord {
        id: row.get(0)?,
        url: row.get(1)?,
        effective_url: row.get(2)?,
        destination: PathBuf::from(row.get::<_, String>(3)?),
        file_name: row.get(4)?,
        status: parsed_required(5, DownloadState::parse, row.get(5))?,
        expected_size: nonneg_optional(6, "expected size", row.get(6))?,
        etag: row.get(7)?,
        last_modified: row.get(8)?,
        retry_count: nonneg_required(9, "retry count", row.get(9))?,
        error_category: parsed_optional(10, parse_error_category, row.get(10))?,
        created_at: nonneg_required(11, "created-at timestamp", row.get(11))?,
        updated_at: nonneg_required(12, "updated-at timestamp", row.get(12))?,
    })
}

/// Maps one `segments` row positionally over [`schema::SEGMENT_COLUMNS`].
fn segment_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SegmentRecord> {
    Ok(SegmentRecord {
        id: row.get(0)?,
        download_id: row.get(1)?,
        min_byte: nonneg_required(2, "segment start", row.get(2))?,
        max_byte: nonneg_required(3, "segment end", row.get(3))?,
        state: parsed_required(4, SegmentState::parse, row.get(4))?,
        received_bytes: nonneg_required(5, "confirmed segment progress", row.get(5))?,
    })
}

/// Maps one `extractions` row positionally over
/// [`schema::EXTRACTION_COLUMNS`].
fn extraction_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExtractionRecord> {
    Ok(ExtractionRecord {
        id: row.get(0)?,
        download_id: row.get(1)?,
        archive_path: PathBuf::from(row.get::<_, String>(2)?),
        destination: PathBuf::from(row.get::<_, String>(3)?),
        state: parsed_required(4, ExtractionState::parse, row.get(4))?,
        error_category: parsed_optional(5, parse_error_category, row.get(5))?,
        error_message: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use super::{
        database_path_in, DownloadState, ErrorCategory, NewDownload, NewExtraction, NewSegment,
        Repository, SegmentState, TransferFacts,
    };

    /// An isolated, auto-removed scratch directory for one database file.
    struct ScratchDb(PathBuf);

    impl ScratchDb {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "swiftfetch-p5-repo-{tag}-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir created");
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("queue.sqlite3")
        }

        fn open(&self) -> Repository {
            Repository::open(&self.file()).expect("scratch repository opened")
        }
    }

    impl Drop for ScratchDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn queued(url: &str, tag: &str) -> NewDownload {
        NewDownload {
            url: url.to_owned(),
            destination: std::env::temp_dir().join(format!("swiftfetch-p5-dest-{tag}.bin")),
            file_name: format!("{tag}.bin"),
        }
    }

    fn insert(repo: &Repository, url: &str, tag: &str) -> i64 {
        repo.insert_download(queued(url, tag), 1_700_000_000)
            .expect("download inserted")
    }

    #[test]
    fn queue_metadata_persists_between_program_runs() {
        let scratch = ScratchDb::new("persist");
        let first;
        {
            let repo = scratch.open();
            let id = insert(&repo, "https://db.test/persist", "persist");
            repo.update_status(id, DownloadState::Probing, 1_700_000_001, None)
                .expect("transition before the restart");
            first = repo.get_download(id).expect("read before close");
        }
        let repo = scratch.open();
        let second = repo.get_download(1).expect("read after reopen");
        assert_eq!(
            first, second,
            "records must not lose state when the process ends"
        );
        assert_eq!(
            first.map(|record| record.status),
            Some(DownloadState::Probing)
        );
        assert_eq!(
            repo.get_download(999).expect("missing reads None"),
            None,
            "unknown ids read as absent, never as an error"
        );
    }

    #[test]
    fn download_records_round_trip_every_field() {
        let scratch = ScratchDb::new("round-trip");
        let repo = scratch.open();
        let id = insert(&repo, "https://db.test/facts", "facts");
        repo.update_status(id, DownloadState::Probing, 1_700_000_001, None)
            .expect("to probing");
        repo.update_status(id, DownloadState::Downloading, 1_700_000_002, None)
            .expect("to downloading");
        repo.update_transfer_facts(
            id,
            &TransferFacts {
                effective_url: Some("https://db.test/facts-final.bin".to_owned()),
                expected_size: Some(4_096),
                etag: Some("\"etag-1\"".to_owned()),
                last_modified: Some("Sun, 18 Oct 2026 00:00:00 GMT".to_owned()),
            },
            1_700_000_003,
        )
        .expect("facts recorded");
        repo.set_retry_count(id, 2, 1_700_000_004)
            .expect("retry count written");

        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.url, "https://db.test/facts");
        assert_eq!(
            record.effective_url,
            Some("https://db.test/facts-final.bin".to_owned())
        );
        assert_eq!(record.expected_size, Some(4_096));
        assert_eq!(record.etag, Some("\"etag-1\"".to_owned()));
        assert_eq!(
            record.last_modified,
            Some("Sun, 18 Oct 2026 00:00:00 GMT".to_owned())
        );
        assert_eq!(record.retry_count, 2);
        assert_eq!(record.status, DownloadState::Downloading);
        assert_eq!(record.error_category, None);
    }

    /// Finalization sometimes lands on a deterministic `(n)` sibling; the
    /// record must follow the file that actually exists, because recovery
    /// (Phase 6 idempotence) trusts the destination column blindly.
    #[test]
    fn the_records_destination_follows_the_actual_finalized_file() {
        let scratch = ScratchDb::new("destination");
        let repo = scratch.open();
        let id = insert(&repo, "https://db.test/destination", "destination");

        repo.set_destination(
            id,
            &queued(url_fixture(), "sibling").destination,
            1_700_000_002,
        )
        .expect("destination corrected");
        let record = repo.get_download(id).expect("read back").expect("present");
        assert_ne!(
            record.destination,
            queued(url_fixture(), "destination").destination,
            "the planned name and the actual file differ by design here"
        );
        // The stamp moves with the write, exactly like the other mutators.
        assert_eq!(record.updated_at, 1_700_000_002);

        let err = repo
            .set_destination(999, &record.destination, 1_700_000_003)
            .expect_err("unknown ids cannot be updated");
        assert_eq!(
            err.category(),
            ErrorCategory::Database,
            "the missing id surfaces as a typed database error"
        );
        assert!(
            err.message().contains("was not found"),
            "the message names the absent record: {}",
            err.message()
        );
    }

    fn url_fixture() -> &'static str {
        "https://db.test/destination"
    }

    /// Phase 7's pristine refresh moves a queued record's provisional name
    /// and destination together in one write: the engine derives both from
    /// the effective URL after the first probe (ADR-0020).
    #[test]
    fn the_planned_name_and_destination_move_together() {
        let scratch = ScratchDb::new("planned-name");
        let repo = scratch.open();
        let id = insert(&repo, "https://db.test/queued", "provisional");

        let renamed = std::env::temp_dir().join("swiftfetch-p7-effective.bin");
        repo.set_planned_name(id, "effective.bin", &renamed, 1_700_000_002)
            .expect("planned name corrected");
        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.file_name, "effective.bin");
        assert_eq!(record.destination, renamed);
        // The stamp moves with the write, exactly like the other mutators.
        assert_eq!(record.updated_at, 1_700_000_002);

        let err = repo
            .set_planned_name(999, "effective.bin", &renamed, 1_700_000_003)
            .expect_err("unknown ids cannot be updated");
        assert_eq!(err.category(), ErrorCategory::Database);
        assert!(
            err.message().contains("was not found"),
            "the message names the absent record: {}",
            err.message()
        );
    }

    #[test]
    fn the_full_happy_walk_follows_only_valid_edges() {
        let scratch = ScratchDb::new("happy-walk");
        let repo = scratch.open();
        let id = insert(&repo, "https://db.test/walk", "walk");
        let path = [
            DownloadState::Probing,
            DownloadState::Downloading,
            DownloadState::Retrying,
            DownloadState::Downloading,
            DownloadState::Paused,
            DownloadState::Downloading,
            DownloadState::Finalizing,
            DownloadState::Completed,
            DownloadState::Extracting,
            DownloadState::Completed,
        ];
        for (step, state) in path.into_iter().enumerate() {
            repo.update_status(id, state, 1_700_000_000 + step as u64 + 1, None)
                .unwrap_or_else(|err| panic!("edge {state:?}: {err}"));
            let record = repo.get_download(id).expect("read").expect("present");
            assert_eq!(record.status, state);
        }
    }

    #[test]
    fn the_lifecycle_graph_matches_the_documented_truth_table() {
        // The complete transition graph of ADR-0018: every edge listed by
        // `DownloadState::allows` is exactly the edge this table lists, for
        // all 100 (from, to) pairs.
        let truth: &[(DownloadState, &[DownloadState])] = &[
            (
                DownloadState::Queued,
                &[DownloadState::Probing, DownloadState::Cancelled],
            ),
            (
                DownloadState::Probing,
                &[
                    DownloadState::Downloading,
                    DownloadState::Failed,
                    DownloadState::Cancelled,
                ],
            ),
            (
                DownloadState::Downloading,
                &[
                    DownloadState::Finalizing,
                    DownloadState::Retrying,
                    DownloadState::Paused,
                    DownloadState::Failed,
                    DownloadState::Cancelled,
                ],
            ),
            (
                DownloadState::Paused,
                &[DownloadState::Downloading, DownloadState::Cancelled],
            ),
            (
                DownloadState::Retrying,
                &[
                    DownloadState::Downloading,
                    DownloadState::Paused,
                    DownloadState::Failed,
                    DownloadState::Cancelled,
                ],
            ),
            (
                DownloadState::Finalizing,
                &[DownloadState::Completed, DownloadState::Failed],
            ),
            (DownloadState::Completed, &[DownloadState::Extracting]),
            (DownloadState::Extracting, &[DownloadState::Completed]),
            // Phase 11: the retry command's requeue edge, added by explicit
            // tested decision (ADR-0023). `cancelled` stays terminal.
            (DownloadState::Failed, &[DownloadState::Queued]),
            (DownloadState::Cancelled, &[]),
        ];
        for (from, allowed) in truth {
            for to in DownloadState::ALL {
                let permitted = DownloadState::allows(*from, to);
                assert_eq!(
                    permitted,
                    allowed.contains(&to),
                    "transition {from:?} → {to:?} must be {}",
                    if permitted { "allowed" } else { "rejected" }
                );
            }
        }
    }

    #[test]
    fn invalid_transitions_are_rejected_atomically() {
        let scratch = ScratchDb::new("invalid");
        let repo = scratch.open();
        let id = insert(&repo, "https://db.test/invalid", "invalid");

        // Queued cannot leap to completed.
        let err = repo
            .update_status(id, DownloadState::Completed, 1_700_000_001, None)
            .expect_err("skip must fail");
        assert!(err.message().contains("cannot move from"), "{err}");
        let after = repo.get_download(id).expect("read").expect("present");
        assert_eq!(after.status, DownloadState::Queued, "nothing was written");
        assert_eq!(after.updated_at, 1_700_000_000, "stamps untouched");

        // A state does not transition to itself.
        let err = repo
            .update_status(id, DownloadState::Queued, 1_700_000_002, None)
            .expect_err("no-op must fail");
        assert!(err.message().contains("is already in the"), "{err}");

        // Extraction is out of reach of the downloading state directly.
        repo.update_status(id, DownloadState::Probing, 1_700_000_003, None)
            .expect("probe starts");
        repo.update_status(id, DownloadState::Downloading, 1_700_000_004, None)
            .expect("transfer starts");
        let err = repo
            .update_status(id, DownloadState::Extracting, 1_700_000_005, None)
            .expect_err("extraction requires completed state");
        assert!(err.message().contains("cannot move from"), "{err}");
        let after = repo.get_download(id).expect("read").expect("present");
        assert_eq!(after.status, DownloadState::Downloading);
    }

    #[test]
    fn entering_failed_requires_and_records_an_error_category() {
        let scratch = ScratchDb::new("failed-category");
        let repo = scratch.open();
        let id = insert(&repo, "https://db.test/failed", "failed");
        repo.update_status(id, DownloadState::Probing, 1_700_000_001, None)
            .expect("probe starts");

        let err = repo
            .update_status(id, DownloadState::Failed, 1_700_000_002, None)
            .expect_err("failed without a category must fail");
        assert!(
            err.message().contains("requires an error category"),
            "{err}"
        );

        repo.update_status(
            id,
            DownloadState::Failed,
            1_700_000_003,
            Some(ErrorCategory::Http),
        )
        .expect("failed with a category");
        let record = repo.get_download(id).expect("read").expect("present");
        assert_eq!(record.status, DownloadState::Failed);
        assert_eq!(record.error_category, Some(ErrorCategory::Http));

        // And healthy states refuse to carry an error category.
        let fresh = insert(&repo, "https://db.test/failed-2", "failed-2");
        let err = repo
            .update_status(
                fresh,
                DownloadState::Probing,
                1_700_004_000,
                Some(ErrorCategory::Input),
            )
            .expect_err("category on a healthy state must fail");
        assert!(err.message().contains("only be recorded"), "{err}");
        let record = repo.get_download(fresh).expect("read").expect("present");
        assert_eq!(record.error_category, None, "nothing was recorded");
        assert_eq!(record.status, DownloadState::Queued);
    }

    #[test]
    fn failed_and_cancelled_download_records_are_terminal() {
        let scratch = ScratchDb::new("terminal");
        let repo = scratch.open();
        let failed = insert(&repo, "https://db.test/terminal-1", "terminal-1");
        repo.update_status(failed, DownloadState::Probing, 1_700_000_001, None)
            .expect("probe starts");
        repo.update_status(
            failed,
            DownloadState::Failed,
            1_700_000_002,
            Some(ErrorCategory::Http),
        )
        .expect("failed");

        // As of Phase 11 a failed record is no longer fully terminal: the
        // `retry` command requeues it through the explicit `failed → queued`
        // edge the graph reserved for that phase (ADR-0023). Every other hop
        // out of `failed` is still rejected at the database level — the same
        // checks the exhaustive truth table locks above.
        for to in [
            DownloadState::Downloading,
            DownloadState::Probing,
            DownloadState::Finalizing,
            DownloadState::Cancelled,
        ] {
            let err = repo
                .update_status(failed, to, 1_700_000_003, None)
                .expect_err("failed only re-enters the queue");
            assert!(err.message().contains("cannot move from"), "{err}");
        }

        repo.update_status(failed, DownloadState::Queued, 1_700_000_004, None)
            .expect("the retry requeue edge is permitted");
        let record = repo.get_download(failed).expect("read").expect("present");
        assert_eq!(record.status, DownloadState::Queued, "requeued for retry");
        assert_eq!(
            record.error_category, None,
            "the requeue clears the stored reason"
        );
        assert_eq!(
            record.retry_count, 0,
            "update_status keeps retry history untouched \
             (`set_retry_count` owns it)"
        );

        // `cancelled` remains fully terminal: the user gave the record up,
        // and no command — including `retry` — resurrects it (ADR-0023).
        let cancelled = insert(&repo, "https://db.test/terminal-2", "terminal-2");
        repo.update_status(cancelled, DownloadState::Cancelled, 1_700_000_005, None)
            .expect("cancel queued work");
        for to in [
            DownloadState::Probing,
            DownloadState::Failed,
            DownloadState::Queued,
        ] {
            let err = repo
                .update_status(cancelled, to, 1_700_000_006, None)
                .expect_err("cancelled is terminal");
            assert!(err.message().contains("cannot move from"), "{err}");
        }
    }

    #[test]
    fn listing_orders_by_insertion_and_filters_by_status() {
        let scratch = ScratchDb::new("list");
        let repo = scratch.open();
        insert(&repo, "https://db.test/a", "list-a");
        insert(&repo, "https://db.test/b", "list-b");
        insert(&repo, "https://db.test/c", "list-c");
        repo.update_status(1, DownloadState::Probing, 1_700_000_001, None)
            .expect("1 probes");
        repo.update_status(
            1,
            DownloadState::Failed,
            1_700_000_002,
            Some(ErrorCategory::Http),
        )
        .expect("1 fails");

        let all = repo.list_downloads().expect("all listed");
        assert_eq!(
            all.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "insertion order is stable"
        );
        let queued = repo
            .list_downloads_by_status(DownloadState::Queued)
            .expect("queued listed");
        assert_eq!(
            queued.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        let failed = repo
            .list_downloads_by_status(DownloadState::Failed)
            .expect("failed listed");
        assert_eq!(
            failed.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![1]
        );
        let none = repo
            .list_downloads_by_status(DownloadState::Cancelled)
            .expect("cancelled listed");
        assert!(none.is_empty());
    }

    #[test]
    fn corrupt_persisted_names_surface_as_typed_database_errors() {
        let scratch = ScratchDb::new("corrupt");
        let repo = scratch.open();
        insert(&repo, "https://db.test/corrupt", "corrupt");

        let watcher = rusqlite::Connection::open(scratch.file()).expect("watcher");
        watcher
            .execute("UPDATE downloads SET status = 'garbage' WHERE id = 1", [])
            .expect("corrupted status");
        let err = repo
            .get_download(1)
            .expect_err("corrupt status must be a typed failure");
        assert_eq!(err.category(), ErrorCategory::Database);
        assert!(
            err.message().contains("unrecognized download state"),
            "{err}"
        );

        watcher
            .execute(
                "UPDATE downloads SET status = 'failed', error_category = 'not-a-category' \
                 WHERE id = 1",
                [],
            )
            .expect("corrupted category");
        let err = repo
            .get_download(1)
            .expect_err("corrupt category must be a typed failure");
        assert_eq!(err.category(), ErrorCategory::Database);
        assert!(
            err.message().contains("unrecognized error category"),
            "{err}"
        );
    }

    #[test]
    fn negative_metadata_values_surface_as_typed_database_errors() {
        let scratch = ScratchDb::new("negative");
        let repo = scratch.open();
        insert(&repo, "https://db.test/negative", "negative");

        let watcher = rusqlite::Connection::open(scratch.file()).expect("watcher");
        for (column, value) in [("expected_size", -5), ("retry_count", -1)] {
            watcher
                .execute(
                    &format!("UPDATE downloads SET {column} = ?1 WHERE id = 1"),
                    [value],
                )
                .expect("corrupted count");
            let err = repo
                .get_download(1)
                .expect_err("negative values must be a typed failure");
            assert_eq!(err.category(), ErrorCategory::Database);
            assert!(err.message().contains("negative"), "{err}");
        }
    }

    #[test]
    fn writes_commit_before_the_repository_returns() {
        let scratch = ScratchDb::new("visible");
        let repo = scratch.open();
        // A second connection sees every write the moment a repository
        // method returns: no operation holds an open transaction across any
        // other work, and callers observe commits immediately.
        let watcher = rusqlite::Connection::open(scratch.file()).expect("watcher");

        insert(&repo, "https://db.test/visible", "visible");
        let rows: i64 = watcher
            .query_row("SELECT count(*) FROM downloads", [], |row| row.get(0))
            .expect("watcher reads committed rows");
        assert_eq!(rows, 1, "the insert was already committed");

        repo.update_status(1, DownloadState::Probing, 1_700_000_001, None)
            .expect("transition");
        let status: String = watcher
            .query_row("SELECT status FROM downloads WHERE id = 1", [], |row| {
                row.get(0)
            })
            .expect("watcher reads the committed status");
        assert_eq!(status, "probing");
    }

    #[test]
    fn records_carry_caller_supplied_timestamps() {
        let scratch = ScratchDb::new("stamps");
        let repo = scratch.open();
        let id = insert(&repo, "https://db.test/stamps", "stamps");
        let record = repo.get_download(id).expect("read").expect("present");
        assert_eq!(record.created_at, 1_700_000_000);
        assert_eq!(record.updated_at, 1_700_000_000);

        repo.update_status(id, DownloadState::Probing, 1_700_000_050, None)
            .expect("probing");
        repo.update_transfer_facts(
            id,
            &TransferFacts {
                expected_size: Some(10),
                ..TransferFacts::default()
            },
            1_700_000_100,
        )
        .expect("facts");
        let record = repo.get_download(id).expect("read").expect("present");
        assert_eq!(record.created_at, 1_700_000_000, "birth is immutable");
        assert_eq!(record.updated_at, 1_700_000_100, "latest write stamps");
    }

    #[test]
    fn segments_round_trip_and_guard_their_confirmed_progress() {
        let scratch = ScratchDb::new("segments");
        let repo = scratch.open();
        let id = insert(&repo, "https://db.test/segments", "segments");

        let first = repo
            .insert_segment(
                id,
                NewSegment {
                    min_byte: 0,
                    max_byte: 1_023,
                    state: SegmentState::Pending,
                    received_bytes: 0,
                },
            )
            .expect("first segment");
        let second = repo
            .insert_segment(
                id,
                NewSegment {
                    min_byte: 1_024,
                    max_byte: 4_095,
                    state: SegmentState::Pending,
                    received_bytes: 0,
                },
            )
            .expect("second segment");

        let listed = repo.list_segments(id).expect("segments listed");
        assert_eq!(listed.len(), 2, "both segments stored");
        assert_eq!(
            listed[0],
            super::SegmentRecord {
                id: first,
                download_id: id,
                min_byte: 0,
                max_byte: 1_023,
                state: SegmentState::Pending,
                received_bytes: 0,
            }
        );
        assert_eq!(listed[1].min_byte, 1_024);
        assert_eq!(listed[1].max_byte, 4_095);

        repo.update_segment(second, SegmentState::Downloading, 0)
            .expect("segment claimed");
        repo.update_segment(second, SegmentState::Completed, 3_072)
            .expect("progress confirmed");
        let listed = repo.list_segments(id).expect("segments listed");
        assert_eq!(listed[1].state, SegmentState::Completed);
        assert_eq!(listed[1].received_bytes, 3_072);

        // Over-claiming, even by one byte, is rejected.
        let err = repo
            .update_segment(second, SegmentState::Completed, 3_073)
            .expect_err("over-claim must fail");
        assert!(err.message().contains("exceeds the segment"), "{err}");

        let err = repo
            .insert_segment(
                id,
                NewSegment {
                    min_byte: 9_999,
                    max_byte: 9_998,
                    state: SegmentState::Pending,
                    received_bytes: 0,
                },
            )
            .expect_err("inverted range must fail");
        assert!(err.message().contains("inverted"), "{err}");
    }

    #[test]
    fn segment_states_follow_the_phase_8_graph() {
        use super::SegmentState::{Completed, Downloading, Failed, Pending};
        let allowed = [
            (Pending, Pending),
            (Pending, Downloading),
            (Pending, Failed),
            (Downloading, Downloading),
            (Downloading, Completed),
            (Downloading, Failed),
            (Completed, Completed),
            (Completed, Failed),
            (Failed, Failed),
        ];
        for (from, to) in allowed {
            assert!(
                from.can_transition_to(to),
                "{} must be allowed to follow {}",
                to.as_str(),
                from.as_str()
            );
        }
        let refused = [
            (Pending, Completed),
            (Downloading, Pending),
            (Completed, Pending),
            (Completed, Downloading),
            (Failed, Pending),
            (Failed, Downloading),
            (Failed, Completed),
        ];
        for (from, to) in refused {
            assert!(
                !from.can_transition_to(to),
                "{} must not follow {}",
                to.as_str(),
                from.as_str()
            );
        }
    }

    #[test]
    fn a_segment_cannot_skip_its_lifecycle_graph() {
        let scratch = ScratchDb::new("segment-graph");
        let repo = scratch.open();
        let id = insert(&repo, "https://db.test/segment-graph", "graph");
        let segment = repo
            .insert_segment(
                id,
                NewSegment {
                    min_byte: 0,
                    max_byte: 9,
                    state: SegmentState::Pending,
                    received_bytes: 0,
                },
            )
            .expect("segment inserted");

        let err = repo
            .update_segment(segment, SegmentState::Completed, 10)
            .expect_err("a pending segment cannot complete without downloading");
        assert_eq!(err.category(), ErrorCategory::Database);
        assert!(
            err.message()
                .contains("cannot move from pending to completed"),
            "the refusal names the refused edge: {err}"
        );
        let stored = repo.list_segments(id).expect("segments listed");
        assert_eq!(stored[0].state, SegmentState::Pending);
        assert_eq!(stored[0].received_bytes, 0, "the refusal writes nothing");
    }

    #[test]
    fn segment_and_extraction_records_hang_off_their_download() {
        let scratch = ScratchDb::new("foreign");
        let repo = scratch.open();

        let err = repo
            .insert_segment(
                777,
                NewSegment {
                    min_byte: 0,
                    max_byte: 0,
                    state: SegmentState::Pending,
                    received_bytes: 0,
                },
            )
            .expect_err("orphan segment must fail");
        assert_eq!(err.category(), ErrorCategory::Database);
        assert!(err.message().contains("FOREIGN KEY"), "{err}");

        let err = repo
            .insert_extraction(
                777,
                NewExtraction {
                    archive_path: std::env::temp_dir().join("x.zip"),
                    destination: std::env::temp_dir().join("x"),
                },
            )
            .expect_err("orphan extraction must fail");
        assert_eq!(err.category(), ErrorCategory::Database);
        assert!(err.message().contains("FOREIGN KEY"), "{err}");
    }

    #[test]
    fn extraction_records_track_state_and_error_details() {
        let scratch = ScratchDb::new("extract");
        let repo = scratch.open();
        let download = insert(&repo, "https://db.test/extract", "extract");
        let extraction = repo
            .insert_extraction(
                download,
                NewExtraction {
                    archive_path: std::env::temp_dir().join("file.zip"),
                    destination: std::env::temp_dir().join("extracted"),
                },
            )
            .expect("extraction inserted");
        let record = repo
            .get_extraction(extraction)
            .expect("read")
            .expect("present");
        assert_eq!(record.state, super::ExtractionState::Queued);
        assert_eq!(record.error_category, None);
        assert_eq!(record.error_message, None);

        repo.update_extraction(extraction, super::ExtractionState::Extracting, None)
            .expect("extracting");
        repo.update_extraction(
            extraction,
            super::ExtractionState::Failed,
            Some((
                ErrorCategory::ZipExtraction,
                "destination disk is full".to_owned(),
            )),
        )
        .expect("extraction failure recorded");
        let record = repo
            .get_extraction(extraction)
            .expect("read")
            .expect("present");
        assert_eq!(record.state, super::ExtractionState::Failed);
        assert_eq!(record.error_category, Some(ErrorCategory::ZipExtraction));
        assert_eq!(
            record.error_message,
            Some("destination disk is full".to_owned())
        );

        let err = repo
            .update_extraction(extraction, super::ExtractionState::Failed, None)
            .expect_err("failed without details must fail");
        assert!(err.message().contains("requires"), "{err}");

        // A later successful pass clears the details again.
        repo.update_extraction(extraction, super::ExtractionState::Completed, None)
            .expect("extraction completes");
        let record = repo
            .get_extraction(extraction)
            .expect("read")
            .expect("present");
        assert_eq!(record.state, super::ExtractionState::Completed);
        assert_eq!(record.error_category, None);
        assert_eq!(record.error_message, None);

        assert_eq!(
            repo.list_extractions(download).expect("listed").len(),
            1,
            "the download owns its extraction record"
        );
        let missing = repo.get_extraction(66).expect("missing reads none");
        assert!(missing.is_none());
    }

    #[test]
    fn update_methods_reject_records_that_do_not_exist() {
        let scratch = ScratchDb::new("not-found");
        let repo = scratch.open();

        let err = repo
            .update_status(999, DownloadState::Probing, 0, None)
            .expect_err("missing status update must fail");
        assert!(err.message().contains("was not found"), "{err}");

        let err = repo
            .update_transfer_facts(999, &TransferFacts::default(), 0)
            .expect_err("missing facts update must fail");
        assert!(err.message().contains("was not found"), "{err}");

        let err = repo
            .set_retry_count(999, 1, 0)
            .expect_err("missing retry update must fail");
        assert!(err.message().contains("was not found"), "{err}");

        let err = repo
            .update_segment(999, SegmentState::Completed, 0)
            .expect_err("missing segment update must fail");
        assert!(err.message().contains("was not found"), "{err}");

        let err = repo
            .update_extraction(999, super::ExtractionState::Completed, None)
            .expect_err("missing extraction update must fail");
        assert!(err.message().contains("was not found"), "{err}");
    }

    #[test]
    fn the_default_database_location_stays_under_local_app_data() {
        let root = OsStr::new(r"C:\Users\demo\AppData\Local");
        let expected = r"C:\Users\demo\AppData\Local\SwiftFetch\swiftfetch.sqlite3";
        let resolved = database_path_in(Some(root)).expect("root resolves");
        assert_eq!(resolved, PathBuf::from(expected));

        for root in [None, Some(OsStr::new(""))] {
            let err = database_path_in(root).expect_err("no place to write");
            assert_eq!(err.category(), ErrorCategory::Filesystem);
            assert!(err.message().contains("LOCALAPPDATA"), "{err}");
        }

        // Real-environment smoke: on Windows the path derives cleanly.
        let default = super::default_database_path().expect("platform location");
        assert_eq!(default.file_name(), Some(OsStr::new("swiftfetch.sqlite3")));
        assert_eq!(
            default.parent().and_then(Path::file_name),
            Some(OsStr::new("SwiftFetch"))
        );
    }

    #[test]
    fn an_explicit_database_location_overrides_the_platform_default() {
        // The Phase 11 command wiring resolves this precedence on every
        // launch; hermetic end-to-end tests depend on it. Pure form: the
        // precedence is derived, never read from the process environment
        // here, so parallel tests cannot race it.
        let explicit = OsStr::new(r"D:\profiles\swiftfetch\queue.sqlite3");
        let resolved = super::database_path(
            Some(explicit),
            Some(OsStr::new(r"C:\Users\demo\AppData\Local")),
        )
        .expect("explicit location wins");
        assert_eq!(
            resolved,
            PathBuf::from(r"D:\profiles\swiftfetch\queue.sqlite3"),
            "an explicit location replaces the platform default"
        );

        // An empty explicit value is not a location; the platform default
        // still applies (and still refuses a missing root).
        let resolved =
            super::database_path(Some(OsStr::new("")), Some(OsStr::new(r"C:\AppDataRoot")))
                .expect("an empty override falls back to the platform location");
        assert_eq!(
            resolved,
            PathBuf::from(r"C:\AppDataRoot\SwiftFetch\swiftfetch.sqlite3"),
            "the empty override is not a location"
        );
        let err = super::database_path(Some(OsStr::new("")), None)
            .expect_err("empty override is not a fallback guess");
        assert_eq!(err.category(), ErrorCategory::Filesystem);
    }

    #[test]
    fn an_inaccessible_database_directory_reports_a_typed_error() {
        let scratch = ScratchDb::new("blocked");
        let blocker = scratch.0.join("blocked");
        std::fs::write(&blocker, b"occupied").expect("blocker written");

        let err = Repository::open(&blocker.join("queue.sqlite3"))
            .expect_err("a file cannot parent a database");
        assert_eq!(err.category(), ErrorCategory::Filesystem);
        assert!(
            err.message().contains("blocked"),
            "message names the blocked location: {err}"
        );
    }
}
