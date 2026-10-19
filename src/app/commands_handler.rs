//! Command dispatch handlers (Phase 11).
//!
//! These functions wire the parsed CLI contract to the completed engine and
//! storage services:
//!
//! - `download` → the Phase 7 batch scheduler ([`run_batch`]) under the
//!   Phase 6 graceful-shutdown wiring, with live progress sweeps drained
//!   from the transfer registry between refresh intervals;
//! - `status` → read-only persisted-queue rendering (`src/cli/status.rs`);
//! - `resume` → eligible unfinished records (ADR-0019's `RECOVERABLE`
//!   states) driven through the persisted engine ([`transfer_persisted`])
//!   within the configured file-concurrency limit;
//! - `retry` → failed records requeued through the explicitly tested
//!   `failed → queued` lifecycle edge (ADR-0018 reserved it for the retry
//!   command's phase; see ADR-0023) and then driven the same way.
//!
//! The handlers never implement HTTP or transfer logic (that lives in the
//! engine), never touch SQLite outside the storage layer, and never decide
//! what resumes safely — `plan_for` owns that verdict. They orchestrate
//! services, render results, and map outcomes to the documented exit
//! codes: `0` every requested download completed, `1` some did not (or the
//! command could not run), `2` invalid invocation/configuration.
//!
//! Wiring notes (ADR-0023):
//! - the `SWIFT_FETCH_DATABASE` location override is honored by the
//!   repository's default path resolution (hermetic end-to-end tests and
//!   deliberate profile separation);
//! - Ctrl+C (`tokio::signal::ctrl_c`) trips the engine's shutdown handle;
//!   in-flight transfers park through the Phase 6 path and the command
//!   completes with the honest interrupted report (exit 1);
//! - stdout stays reserved for results and controlled progress renders;
//!   every URL that reaches a display line is redacted first.

use std::collections::{hash_map::Entry, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::app::batch::{run_batch, BatchRequest, DownloadResult};
use crate::cli::progress::{
    estimate_eta, format_bytes, sweep_lines, ThroughputWindow, REFRESH_INTERVAL, ROLLING_WINDOW,
};
use crate::cli::status::{status_lines, StatusRow};
use crate::cli::DownloadArgs;
use crate::config::AppConfig;
use crate::download::http::HttpClient;
use crate::download::progress::{Progress, TransferKey, TransferRegistry};
use crate::download::resume::{transfer_persisted, ShutdownHandle, ShutdownSignal, RECOVERABLE};
use crate::download::retry::RetryPolicy;
use crate::download::segmented::SegmentationPolicy;
use crate::error::{AppError, ErrorCategory};
use crate::logging::redact_url;
use crate::storage::repository::{
    default_database_path, DownloadRecord, DownloadState, ExtractionState, Repository, SegmentState,
};

/// How a command itself ended, and the exit code that maps to it.
///
/// Documented table (help text and README): `0` every requested download
/// of the invocation completed; `1` at least one did not (failed or
/// interrupted) — or the tool could not continue at all; a hard
/// [`AppError`] maps through its category (`2` for invalid
/// invocation/configuration) inside `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandEnd {
    /// Every requested download completed.
    Complete,
    /// The command ran, but not all requested downloads succeeded (or it
    /// was interrupted mid-flight).
    Incomplete,
}

impl CommandEnd {
    /// The documented exit code for this end state.
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            CommandEnd::Complete => 0,
            CommandEnd::Incomplete => 1,
        }
    }
}

/// The per-command plumbing: configured services shared by every handler,
/// opened once per invocation and owned by the handler.
struct Plumbing {
    repo: Repository,
    client: HttpClient,
    config: AppConfig,
    policy: RetryPolicy,
    segmentation: SegmentationPolicy,
}

impl Plumbing {
    /// Opens the services with the given (already validated) run
    /// configuration. The database lives in the user's local
    /// application-data directory unless `SWIFT_FETCH_DATABASE` names
    /// somewhere deliberate instead (ADR-0017 location, ADR-0023
    /// override).
    fn open(config: AppConfig) -> Result<Self, AppError> {
        config.validate()?;
        let database = default_database_path()?;
        let repo = Repository::open(&database)?;
        Ok(Self {
            repo,
            client: HttpClient::default_client()?,
            policy: RetryPolicy::from_retries(config.retry_attempts),
            segmentation: SegmentationPolicy::from_config(&config),
            config,
        })
    }

    /// Opens the services with the architecture's starting configuration
    /// (`status`, `resume`, and `retry` take no configuration options; the
    /// documented starting values apply).
    fn open_with_defaults() -> Result<Self, AppError> {
        Self::open(AppConfig::default())
    }
}

/// Unix seconds, as the repository's timestamp contract expects.
fn production_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Handles the `download` command: validate the arguments, open the
/// services, drive the batch scheduler, render controlled live progress,
/// and finish with the per-download summary and the overall line.
pub(super) async fn handle_download(args: &DownloadArgs) -> Result<CommandEnd, AppError> {
    let config = crate::cli::config_from_download(args)?;
    crate::cli::validate_download_input(args)?;
    let plumbing = Plumbing::open(config)?;
    let request = BatchRequest {
        urls: args.urls.clone(),
        url_file: args.url_file.clone(),
    };
    let output_dir = plumbing.config.output_dir.clone();

    let registry = TransferRegistry::shared();
    let (stop_display, display_rx) = watch::channel(false);
    let display = tokio::spawn(render_transfers(
        Arc::clone(&registry),
        plumbing.repo.clone(),
        display_rx,
    ));
    // Phase 6's shutdown contract reaches the command layer here: the first
    // Ctrl+C trips the handle the engine races at every read.
    let (shutdown_signal, shutdown) = ShutdownSignal::channel();
    let interrupt = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shutdown_signal.trip();
        }
    });

    let attempted = run_batch(
        plumbing.client.clone(),
        Arc::new(plumbing.repo.clone()),
        request,
        &output_dir,
        plumbing.config.max_concurrent_files,
        &plumbing.policy,
        &plumbing.segmentation,
        &registry,
        plumbing.config.zip_auto_extract,
        &shutdown,
        production_now,
    )
    .await;

    // Orderly teardown of the spawned wiring, whatever the batch outcome.
    interrupt.abort();
    let _ = stop_display.send(true);
    let _ = display.await;

    let report = attempted?;
    let completed = report.completed_count();
    let failed = report.failed_count();
    let interrupted = report.interrupted_count();

    let mut outcomes = report.outcomes;
    outcomes.sort_by(|left, right| left.url.cmp(&right.url));
    for outcome in &outcomes {
        let url = redact_url(&outcome.url);
        match &outcome.result {
            DownloadResult::Completed {
                destination,
                bytes_written,
            } => println!(
                "completed {url} -> {} ({bytes})",
                destination.display(),
                bytes = format_bytes(*bytes_written),
            ),
            DownloadResult::Failed { category, message } => {
                println!("failed {url} ({}): {message}", category.as_str());
            }
            DownloadResult::Interrupted => println!(
                "interrupted {url} (the download remains queued or paused; \
                 run swift-fetch resume to continue it)",
            ),
        }
    }
    println!(
        "SwiftFetch: {completed} completed, {failed} failed, {interrupted} interrupted \
         of {accepted}; peak active downloads {} of the configured {}",
        report.peak_active_files,
        plumbing.config.max_concurrent_files,
        accepted = outcomes.len(),
    );

    Ok(if failed + interrupted == 0 {
        CommandEnd::Complete
    } else {
        CommandEnd::Incomplete
    })
}

/// Handles the `status` command: a read-only view of the persisted queue.
///
/// The listing never mutates state; it renders only stored facts.
pub(super) fn handle_status() -> Result<CommandEnd, AppError> {
    let plumbing = Plumbing::open_with_defaults()?;
    for line in status_lines(&collect_status_rows(&plumbing.repo)?) {
        println!("{line}");
    }
    Ok(CommandEnd::Complete)
}

/// Assembles the rendered facts for every record from stored state.
fn collect_status_rows(repo: &Repository) -> Result<Vec<StatusRow>, AppError> {
    let mut rows = Vec::new();
    for record in repo.list_downloads()? {
        let confirmed = if record.status == DownloadState::Completed {
            None
        } else {
            confirmed_progress(repo, record.id)?
        };
        let extraction_failed = if record.status == DownloadState::Completed {
            newest_failed_extraction(repo, record.id)?
        } else {
            None
        };
        rows.push(StatusRow {
            id: record.id,
            state: record.status.as_str(),
            file_name: record.file_name,
            url: record.url,
            expected_size: record.expected_size,
            confirmed,
            error_category: record.error_category.map(ErrorCategory::as_str),
            extraction_failed,
        });
    }
    Ok(rows)
}

/// The confirmed bytes of a record's transfer so far, from stored rows.
///
/// The newest `min_byte = 0` non-`failed` segment row's confirmed claim is
/// the same value the resume trust rule would trust — `status` cannot show
/// more than the engine itself would resume from.
fn confirmed_progress(repo: &Repository, id: i64) -> Result<Option<u64>, AppError> {
    Ok(repo
        .list_segments(id)?
        .into_iter()
        .rfind(|segment| segment.min_byte == 0 && segment.state != SegmentState::Failed)
        .map(|segment| segment.received_bytes))
}

/// The stored error category of a completed download's newest extraction
/// row, when that extraction failed (completion and extraction are
/// separate outcomes; nothing here rewrites the download's state).
fn newest_failed_extraction(repo: &Repository, id: i64) -> Result<Option<&'static str>, AppError> {
    Ok(repo
        .list_extractions(id)?
        .last()
        .filter(|extraction| extraction.state == ExtractionState::Failed)
        .and_then(|extraction| extraction.error_category)
        .map(ErrorCategory::as_str))
}

/// Handles the `resume` command: eligible unfinished records (queued,
/// probing, downloading, paused, retrying, finalizing) are collected from
/// persistent state and driven through the engine within the configured
/// file-concurrency limit. Records the engine cannot resume safely surface
/// as that record's failed outcome — never a fake completion.
pub(super) async fn handle_resume() -> Result<CommandEnd, AppError> {
    let plumbing = Plumbing::open_with_defaults()?;
    handle_queue_work(plumbing, "resume", select_resume_candidates).await
}

/// Handles the `retry` command: failed records are requeued through the
/// tested requeue edge, then driven like resumable work — completed records
/// and every other unrelated record are untouched.
pub(super) async fn handle_retry() -> Result<CommandEnd, AppError> {
    let plumbing = Plumbing::open_with_defaults()?;
    handle_queue_work(plumbing, "retry", select_retry_candidates).await
}

/// The shared workflow of `resume` and `retry`: select records, drive them
/// through a bounded worker pool, print the summary, and decide the exit.
/// `label` names the command in the user-facing lines. An empty selection
/// is a successful, honestly-worded no-op.
async fn handle_queue_work(
    plumbing: Plumbing,
    label: &'static str,
    select: fn(&Repository) -> Result<Vec<RecordSelection>, AppError>,
) -> Result<CommandEnd, AppError> {
    let selections = select(&plumbing.repo)?;
    if selections.is_empty() {
        println!("SwiftFetch: nothing to {label}; no eligible downloads on record.");
        return Ok(CommandEnd::Complete);
    }

    let registry = TransferRegistry::shared();
    let (stop_display, display_rx) = watch::channel(false);
    let display = tokio::spawn(render_transfers(
        Arc::clone(&registry),
        plumbing.repo.clone(),
        display_rx,
    ));
    let (shutdown_signal, shutdown) = ShutdownSignal::channel();
    let interrupt = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shutdown_signal.trip();
        }
    });

    let attempts = drive_selections(&plumbing, selections, &registry, &shutdown).await?;

    // Orderly teardown of the spawned wiring, whatever the outcomes were.
    interrupt.abort();
    let _ = stop_display.send(true);
    let _ = display.await;

    let completed = attempts
        .iter()
        .filter(|attempt| matches!(attempt.result, RecordResult::Completed { .. }))
        .count();
    let failed = attempts
        .iter()
        .filter(|attempt| matches!(attempt.result, RecordResult::Failed { .. }))
        .count();
    let interrupted = attempts
        .iter()
        .filter(|attempt| matches!(attempt.result, RecordResult::Interrupted))
        .count();

    let mut attempts = attempts;
    attempts.sort_by_key(|attempt| attempt.record_id);
    for attempt in &attempts {
        let url = redact_url(&attempt.url);
        match &attempt.result {
            RecordResult::Completed {
                destination,
                bytes_written,
            } => println!(
                "completed #{id} {url} -> {} ({bytes})",
                destination.display(),
                id = attempt.record_id,
                bytes = format_bytes(*bytes_written),
            ),
            RecordResult::Failed { category, message } => {
                println!(
                    "failed #{id} ({}): {message}",
                    category.as_str(),
                    id = attempt.record_id,
                );
            }
            RecordResult::Interrupted => println!(
                "interrupted #{id} {url} (the download remains resumable; \
                 rerun swift-fetch resume or retry)",
                id = attempt.record_id,
            ),
        }
    }
    println!(
        "SwiftFetch: {completed} completed, {failed} failed, {interrupted} interrupted \
         of {selected}",
        selected = attempts.len(),
    );

    Ok(if failed + interrupted == 0 {
        CommandEnd::Complete
    } else {
        CommandEnd::Incomplete
    })
}

/// One queued record the pool will drive. `output_dir` is the scope the
/// engine plans against; `None` marks a record whose stored destination
/// carries no directory at all — reported as that record's failure,
/// never networked.
struct RecordSelection {
    record_id: i64,
    url: String,
    output_dir: Option<PathBuf>,
}

/// The `resume` selection: every record a later engine run could still
/// finish (the same [`RECOVERABLE`] set the batch scheduler's adoption
/// law uses — the two definitions must stay one), ordered by insertion.
fn select_resume_candidates(repo: &Repository) -> Result<Vec<RecordSelection>, AppError> {
    let mut selections = Vec::new();
    for record in repo.list_downloads()? {
        if RECOVERABLE.contains(&record.status) {
            selections.push(selection_of(record));
        }
    }
    Ok(selections)
}

/// The `retry` selection: requeue every failed record through the tested
/// lifecycle edge, then hand the same records to the engine as queued work.
///
/// The requeue is deliberate per-record database work before any transfer
/// begins; a failing transition is a typed error that stops the command
/// honestly. Failed records keep their `retry_count` as history — the
/// engine's retry policy budgets per invocation, not per record lifetime.
fn select_retry_candidates(repo: &Repository) -> Result<Vec<RecordSelection>, AppError> {
    let mut selections = Vec::new();
    for record in repo.list_downloads()? {
        if record.status == DownloadState::Failed {
            repo.update_status(record.id, DownloadState::Queued, production_now(), None)?;
            selections.push(selection_of(record));
        }
    }
    Ok(selections)
}

/// Builds one selection from its record; the scope directory is the
/// record's own destination parent — scope truth, exactly as `plan_for`
/// cuts it, even when the user typed a different spelling of the directory
/// in today's invocation. A destination whose parent is missing or *empty*
/// (a bare root-less file name) defines no usable directory scope; `None`
/// marks it so the pool reports the record instead of inventing a scope.
fn selection_of(record: DownloadRecord) -> RecordSelection {
    let output_dir = record
        .destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf);
    RecordSelection {
        record_id: record.id,
        url: record.url,
        output_dir,
    }
}

/// The per-record result of the pool.
#[derive(Debug)]
enum RecordResult {
    /// The file is finalized at `destination`.
    Completed {
        destination: PathBuf,
        bytes_written: u64,
    },
    /// The record's own failure; its persistent state carries it.
    Failed {
        category: ErrorCategory,
        message: String,
    },
    /// Shutdown reached this record before completion.
    Interrupted,
}

#[derive(Debug)]
struct RecordAttempt {
    record_id: i64,
    url: String,
    result: RecordResult,
}

/// Drives the selected records through a bounded pool of worker tasks
/// (`min(max_concurrent_files, selections)` workers over a bounded
/// channel — the same shape the batch scheduler uses, never one task per
/// record). Every record's error is its own outcome; the pool keeps going.
/// A worker task that *fails to reach an outcome* (joined with an error)
/// is a programming defect surfaced as a typed error — never swallowed,
/// never a faked completion.
async fn drive_selections(
    plumbing: &Plumbing,
    selections: Vec<RecordSelection>,
    registry: &Arc<TransferRegistry>,
    shutdown: &ShutdownHandle,
) -> Result<Vec<RecordAttempt>, AppError> {
    let worker_count = plumbing.config.max_concurrent_files.max(1) as usize;
    let worker_count = worker_count.min(selections.len().max(1));

    let (work_tx, work_rx) = mpsc::channel::<RecordSelection>(worker_count * 2);
    let (results_tx, mut results_rx) = mpsc::channel::<RecordAttempt>(worker_count * 2);

    let work_rx = Arc::new(tokio::sync::Mutex::new(work_rx));
    let mut workers = JoinSet::new();
    for _ in 0..worker_count {
        let worker = QueueWorker {
            client: plumbing.client.clone(),
            repo: plumbing.repo.clone(),
            policy: plumbing.policy,
            segmentation: plumbing.segmentation,
            shutdown: shutdown.clone(),
            registry: Arc::clone(registry),
        };
        workers.spawn(worker.run(Arc::clone(&work_rx), results_tx.clone()));
    }
    drop(results_tx);
    for selection in selections {
        if work_tx.send(selection).await.is_err() {
            break; // every worker stopped; the joins surface the cause
        }
    }
    drop(work_tx);

    let mut attempts = Vec::new();
    while let Some(attempt) = results_rx.recv().await {
        attempts.push(attempt);
    }
    while let Some(joined) = workers.join_next().await {
        joined.map_err(|err| {
            AppError::new(
                ErrorCategory::Database,
                format!("a queue worker task failed unexpectedly (this is a bug): {err}"),
            )
        })?;
    }
    Ok(attempts)
}

/// A record pool worker's private clone of the plumbing it needs; cloning
/// `HttpClient` shares the one connection pool, and `Repository` clones
/// share the one serialized connection.
struct QueueWorker {
    client: HttpClient,
    repo: Repository,
    policy: RetryPolicy,
    segmentation: SegmentationPolicy,
    shutdown: ShutdownHandle,
    registry: Arc<TransferRegistry>,
}

impl QueueWorker {
    async fn run(
        self,
        work: Arc<tokio::sync::Mutex<mpsc::Receiver<RecordSelection>>>,
        results: mpsc::Sender<RecordAttempt>,
    ) {
        loop {
            let item = {
                let mut work = work.lock().await;
                work.recv().await
            };
            let Some(item) = item else {
                break;
            };
            let record_id = item.record_id;
            let url = item.url.clone();
            let result = if self.shutdown.tripped() {
                RecordResult::Interrupted
            } else {
                self.attempt(item).await
            };
            let attempt = RecordAttempt {
                record_id,
                url,
                result,
            };
            if results.send(attempt).await.is_err() {
                break;
            }
        }
    }

    /// Runs one record through the persisted engine. The never-touch-the-
    /// network lane is for records whose stored destination carries no
    /// directory at all — the same refusal the engine's planner produces,
    /// reported here without any write.
    async fn attempt(&self, item: RecordSelection) -> RecordResult {
        let Some(output_dir) = item.output_dir else {
            return RecordResult::Failed {
                category: ErrorCategory::Input,
                message: format!(
                    "the download record {} has no directory in its destination and \
                     cannot be resumed",
                    item.record_id
                ),
            };
        };
        let key = TransferKey::new(item.url.clone(), output_dir.clone());
        let progress = Progress::shared();
        self.registry.publish(key.clone(), Arc::clone(&progress));
        let attempt = transfer_persisted(
            &self.client,
            &self.repo,
            &item.url,
            &output_dir,
            progress,
            &self.policy,
            &self.segmentation,
            production_now,
            &self.shutdown,
            true,
        )
        .await;
        self.registry.remove(&key);
        match attempt {
            Ok(download) => RecordResult::Completed {
                destination: download.destination,
                bytes_written: download.bytes_written,
            },
            Err(err) if err.category() == ErrorCategory::Cancelled => RecordResult::Interrupted,
            Err(err) => RecordResult::Failed {
                category: err.category(),
                message: err.message().to_owned(),
            },
        }
    }
}

/// The live progress renderer: samples the transfer registry every
/// [`REFRESH_INTERVAL`], pairs each live transfer's *observed* byte count
/// with recorded probe facts (expected size) from the database, and prints
/// one controlled line per transfer whose bytes changed since the last
/// sweep (4 updates per second, unstalled transfers only).
///
/// Database reads are best-effort and display-only: a hiccup degrades the
/// sweep to "size unknown" wording rather than interrupting any transfer.
async fn render_transfers(
    registry: Arc<TransferRegistry>,
    repo: Repository,
    mut stop: watch::Receiver<bool>,
) {
    let mut windows: HashMap<TransferKey, ThroughputWindow> = HashMap::new();
    let mut shown: HashMap<TransferKey, u64> = HashMap::new();
    loop {
        tokio::select! {
            () = tokio::time::sleep(REFRESH_INTERVAL) => {}
            _ = stop.changed() => break,
        }
        let snapshot = registry.snapshot();
        if snapshot.is_empty() {
            continue;
        }
        let facts = registry_facts(&repo, &snapshot);
        let now = Instant::now();
        let mut entries = Vec::with_capacity(snapshot.len());
        for (key, progress) in snapshot {
            let bytes = progress.received();
            let expected = facts
                .get(&(key.url.clone(), key.directory.clone()))
                .copied()
                .flatten();
            let window = windows
                .entry(key.clone())
                .or_insert_with(|| ThroughputWindow::new(ROLLING_WINDOW));
            let speed = window.sample(now, bytes);
            let eta = expected.and_then(|expected| estimate_eta(expected, bytes, speed));
            entries.push((key.clone(), bytes, expected, speed, eta));
        }
        // The live set is the snapshot's own keys (one entry per live
        // transfer), so the sweep can hand `entries` to the renderer.
        let live: HashSet<TransferKey> = entries
            .iter()
            .map(|(key, _, _, _, _)| key.clone())
            .collect();
        for line in sweep_lines(&mut shown, entries) {
            println!("{line}");
        }
        windows.retain(|key, _| live.contains(key));
        shown.retain(|key, _| live.contains(key));
    }
}

/// Recorded probe facts for the live transfers, by engine scope.
///
/// A scope's *first* record wins when several claim the same text (only
/// one unfinished record per scope can exist; a stale duplicate row is
/// terminal history and does not own the scope).
fn registry_facts(
    repo: &Repository,
    snapshot: &[(TransferKey, Arc<Progress>)],
) -> HashMap<(String, PathBuf), Option<u64>> {
    let mut facts = HashMap::new();
    let Ok(records) = repo.list_downloads() else {
        return facts; // display-only downgrade: expected sizes stay unknown
    };
    for record in records {
        let Some(directory) = record.destination.parent().map(Path::to_path_buf) else {
            continue;
        };
        let key = (record.url, directory);
        match facts.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(record.expected_size);
            }
            Entry::Occupied(_) => {}
        }
    }
    let live_keys: HashSet<(String, PathBuf)> = snapshot
        .iter()
        .map(|(key, _)| (key.url.clone(), key.directory.clone()))
        .collect();
    facts.retain(|scope, _| live_keys.contains(scope));
    facts
}

#[cfg(test)]
mod tests {
    use super::{collect_status_rows, confirmed_progress, production_now};
    use super::{select_resume_candidates, select_retry_candidates, CommandEnd};
    use crate::error::ErrorCategory;
    use crate::storage::repository::{DownloadState, ExtractionState};
    use crate::storage::repository::{
        NewDownload, NewExtraction, NewSegment, Repository, SegmentState,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    /// An isolated, auto-removed scratch directory for one test (the same
    /// pattern the batch scheduler's tests use).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "swiftfetch-p11-{tag}-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir created");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A strictly increasing clock so `updated_at` writes always advance.
    fn test_clock() -> u64 {
        static TICK: AtomicU64 = AtomicU64::new(1_900_000_000);
        TICK.fetch_add(1, AtomicOrdering::Relaxed)
    }

    fn scratch_repo(tag: &str) -> (Scratch, Repository) {
        let scratch = Scratch::new(tag);
        let repo = Repository::open(&scratch.path().join("queue.sqlite3"))
            .expect("scratch repository opened");
        (scratch, repo)
    }

    fn stage(repo: &Repository, name: &str) -> i64 {
        repo.insert_download(
            NewDownload {
                url: format!("http://127.0.0.1:9/{name}"),
                destination: scratch_download_dir().join(name),
                file_name: name.to_owned(),
            },
            test_clock(),
        )
        .expect("record staged")
    }

    /// A stable scope directory for staged destinations.
    fn scratch_download_dir() -> PathBuf {
        PathBuf::from("downloads")
    }

    /// Walks a record along the lifecycle to the requested state with a
    /// strictly increasing clock (each hop is an ADR-0018-permitted edge).
    fn drive(repo: &Repository, id: i64, target: DownloadState) {
        let hops = match target {
            DownloadState::Cancelled => vec![DownloadState::Cancelled],
            DownloadState::Paused => vec![
                DownloadState::Probing,
                DownloadState::Downloading,
                DownloadState::Paused,
            ],
            DownloadState::Failed => vec![
                DownloadState::Probing,
                DownloadState::Downloading,
                DownloadState::Failed,
            ],
            DownloadState::Completed => vec![
                DownloadState::Probing,
                DownloadState::Downloading,
                DownloadState::Finalizing,
                DownloadState::Completed,
            ],
            other => panic!("test helper drives to paused/failed/completed, not {other:?}"),
        };
        for hop in hops {
            let error = if hop == DownloadState::Failed {
                Some(ErrorCategory::Http)
            } else {
                None
            };
            repo.update_status(id, hop, test_clock(), error)
                .expect("the lifecycle permits this hop");
        }
    }

    // ---------------------------------------------------------------------
    // CommandEnd: the documented exit-code table.
    // ---------------------------------------------------------------------

    #[test]
    fn command_end_maps_to_the_documented_exit_codes() {
        assert_eq!(CommandEnd::Complete.exit_code(), 0);
        assert_eq!(CommandEnd::Incomplete.exit_code(), 1);
    }

    // ---------------------------------------------------------------------
    // Selection: resume and retry pick exactly their eligible records.
    // ---------------------------------------------------------------------

    #[test]
    fn resume_selects_exactly_the_recoverable_records_in_insertion_order() {
        let (_scratch, repo) = scratch_repo("resume-selection");
        let queued = stage(&repo, "queued.bin");
        let cancelled = stage(&repo, "cancelled.bin");
        let paused = stage(&repo, "paused.bin");
        let failed = stage(&repo, "failed.bin");
        let completed = stage(&repo, "completed.bin");
        drive(&repo, cancelled, DownloadState::Cancelled);
        drive(&repo, paused, DownloadState::Paused);
        drive(&repo, failed, DownloadState::Failed);
        drive(&repo, completed, DownloadState::Completed);

        let selections = select_resume_candidates(&repo).expect("selection succeeds");
        let selected: Vec<i64> = selections.iter().map(|item| item.record_id).collect();
        assert_eq!(
            selected,
            vec![queued, paused],
            "only queued/paused are recoverable (failed and completed are not, \
             cancelled is terminal), in insertion order: {selected:?}"
        );
        for selection in &selections {
            assert!(
                selection.output_dir.is_some(),
                "staged destinations always carry a directory"
            );
        }
    }

    #[test]
    fn retry_requeues_only_failed_records_and_leaves_the_rest_untouched() {
        let (_scratch, repo) = scratch_repo("retry-selection");
        let failed = stage(&repo, "failed.bin");
        let completed = stage(&repo, "completed.bin");
        let queued = stage(&repo, "queued.bin");
        drive(&repo, failed, DownloadState::Failed);
        drive(&repo, completed, DownloadState::Completed);

        let selections = select_retry_candidates(&repo).expect("selection succeeds");
        let selected: Vec<i64> = selections.iter().map(|item| item.record_id).collect();
        assert_eq!(selected, vec![failed], "only the failed record requeues");

        let records = repo.list_downloads().expect("records listed");
        let requeued = records.iter().find(|record| record.id == failed).unwrap();
        assert_eq!(requeued.status, DownloadState::Queued);
        assert_eq!(
            requeued.error_category, None,
            "a requeued record is not failed; the stored reason is cleared"
        );
        let untouched_completed = records
            .iter()
            .find(|record| record.id == completed)
            .unwrap();
        assert_eq!(untouched_completed.status, DownloadState::Completed);
        let untouched_queued = records.iter().find(|record| record.id == queued).unwrap();
        assert_eq!(untouched_queued.status, DownloadState::Queued);

        // A second selection pass over a requeued-but-not-yet-run record
        // finds nothing failed: the requeue happened exactly once.
        let again = select_retry_candidates(&repo).expect("second selection succeeds");
        assert!(
            again.is_empty(),
            "the record is queued now, not failed; retry cannot double-select"
        );
    }

    #[test]
    fn a_selection_for_a_directory_less_destination_is_reported_not_networked() {
        // The engine scopes by destination directory; a record whose
        // destination has no directory at all cannot be planned. The
        // selection carries `None` so the pool reports it as that record's
        // failure instead of inventing a scope.
        let (_scratch, repo) = scratch_repo("directory-less");
        let id = repo
            .insert_download(
                NewDownload {
                    url: "http://127.0.0.1:9/root.bin".to_owned(),
                    destination: PathBuf::from("root.bin"),
                    file_name: "root.bin".to_owned(),
                },
                test_clock(),
            )
            .expect("record staged");

        let selections = select_resume_candidates(&repo).expect("selection succeeds");
        assert_eq!(selections.len(), 1);
        assert_eq!(selections[0].record_id, id);
        assert_eq!(
            selections[0].output_dir, None,
            "a destination without a parent directory cannot be resumed safely"
        );
    }

    // ---------------------------------------------------------------------
    // Status row facts: confirmed progress and extraction failures.
    // ---------------------------------------------------------------------

    #[test]
    fn confirmed_progress_trusts_the_last_non_failed_root_segment() {
        let (_scratch, repo) = scratch_repo("confirmed-progress");
        let id = stage(&repo, "segments.bin");
        // Segment 1: a trusted root-segment claim of 512 bytes.
        repo.insert_segment(
            id,
            NewSegment {
                min_byte: 0,
                max_byte: 4_095,
                state: SegmentState::Downloading,
                received_bytes: 512,
            },
        )
        .expect("segment staged");
        // Segment 2: a non-root span never counts as whole-file progress.
        repo.insert_segment(
            id,
            NewSegment {
                min_byte: 2_048,
                max_byte: 8_191,
                state: SegmentState::Downloading,
                received_bytes: 256,
            },
        )
        .expect("segment staged");
        // Segment 3: a failed root claim is not trusted.
        let retired = repo
            .insert_segment(
                id,
                NewSegment {
                    min_byte: 0,
                    max_byte: 4_095,
                    state: SegmentState::Downloading,
                    received_bytes: 999,
                },
            )
            .expect("segment staged");
        repo.update_segment(retired, SegmentState::Failed, 999)
            .expect("segment retired");
        // Segment 4: the newest trusted root claim wins.
        repo.insert_segment(
            id,
            NewSegment {
                min_byte: 0,
                max_byte: 4_095,
                state: SegmentState::Downloading,
                received_bytes: 768,
            },
        )
        .expect("segment staged");

        let confirmed = confirmed_progress(&repo, id).expect("confirmed progress read");
        assert_eq!(
            confirmed,
            Some(768),
            "the newest min_byte = 0 non-failed segment row's claim is the whole-\
             file progress, the same value the resume trust rule would adopt"
        );
    }

    #[test]
    fn status_rows_report_extraction_failure_separately_from_completion() {
        let (_scratch, repo) = scratch_repo("status-extraction");
        let id = stage(&repo, "archive.zip");
        drive(&repo, id, DownloadState::Completed);
        let extraction = repo
            .insert_extraction(
                id,
                NewExtraction {
                    archive_path: scratch_download_dir().join("archive.zip"),
                    destination: scratch_download_dir().join("archive"),
                },
            )
            .expect("extraction staged");
        repo.update_extraction(
            extraction,
            ExtractionState::Failed,
            Some((
                ErrorCategory::ZipExtraction,
                "the archive is corrupt".to_owned(),
            )),
        )
        .expect("extraction failed");

        let rows = collect_status_rows(&repo).expect("rows collected");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "completed");
        assert_eq!(
            rows[0].extraction_failed,
            Some("zip-extraction"),
            "the extraction failure is a separate stored fact"
        );
        assert_eq!(
            rows[0].error_category, None,
            "the download itself never flips to failed"
        );

        // The newest failed extraction is the one shown: a fresh successful
        // retry of the extraction clears the report.
        let second = repo
            .insert_extraction(
                id,
                NewExtraction {
                    archive_path: scratch_download_dir().join("archive.zip"),
                    destination: scratch_download_dir().join("archive"),
                },
            )
            .expect("second extraction staged");
        repo.update_extraction(second, ExtractionState::Completed, None)
            .expect("extraction completed");
        let rows = collect_status_rows(&repo).expect("rows collected");
        assert_eq!(
            rows[0].extraction_failed, None,
            "only the newest extraction row's outcome is reported"
        );
    }

    #[test]
    fn status_rows_carry_stored_facts_for_a_failed_record() {
        let (_scratch, repo) = scratch_repo("status-failed");
        let id = stage(&repo, "broken.bin");
        drive(&repo, id, DownloadState::Failed);

        let rows = collect_status_rows(&repo).expect("rows collected");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "failed");
        assert_eq!(rows[0].error_category, Some("http"));
        assert_eq!(
            rows[0].confirmed, None,
            "a failed probe never wrote segment rows, so there is no confirmed \
             progress to show"
        );
    }

    // ---------------------------------------------------------------------
    // The engine-side redaction law reaches the command layer through
    // `production_now`'s neighbors; the timestamp helper itself is trivial
    // but its contract (unix seconds) is what the repository stamps.
    // ---------------------------------------------------------------------

    #[test]
    fn production_now_is_unix_seconds() {
        let now = production_now();
        // Not a clock mock: just the honest sanity bound (after 2020-01-01,
        // before 2100-01-01) so a broken clock fails loudly here.
        assert!(
            (1_577_836_800..4_102_444_800).contains(&now),
            "production_now must be plausible unix seconds: {now}"
        );
    }
}
