//! URL batches and bounded concurrent file scheduling (Phase 7).
//!
//! One invocation accepts one URL, several URLs, or a plain-text URL list
//! file, queues every accepted URL as a persistent `queued` record, and
//! drives the Phase 6 persisted engine over them with a fixed pool of
//! worker tasks — never one task per URL (`DOCS/phases.md` Phase 7).
//!
//! The batch layer's laws:
//!
//! - **Validation before work.** Inline URLs and every list line are
//!   parsed and validated up front with the engine's own `validate_url`; a
//!   malformed input stops the whole invocation before anything is queued,
//!   and file-line errors name the file and the 1-based line number.
//!   Requested URLs are normalized through the URL parser so record
//!   scoping is textual and stable across invocations (ADR-0020).
//! - **URL-list text rules.** Blank/whitespace-only lines and lines whose
//!   first non-blank character is `#` are skipped; a UTF-8 byte-order mark
//!   on the first line is tolerated (Windows editors). Every other line
//!   must be a URL.
//! - **Duplicates.** An identical normalized URL submitted again in the
//!   same invocation is collapsed onto the first occurrence (logged, never
//!   silently downloaded twice). Across invocations, an unfinished record
//!   for the same URL and output directory is *adopted* instead of
//!   duplicated — this is how a later invocation resumes work a terminated
//!   one left behind, and it honors the engine's refusal of two unfinished
//!   records for one scope (`plan_for`).
//! - **Queue before network.** Acceptance inserts (or adopts) the
//!   persistent record first; no transfer ever runs without a record. A
//!   queued record's provisional name is refreshed to the effective URL's
//!   name after the first probe (ADR-0016 naming, ADR-0020 refresh).
//! - **Bounded scheduling.** At most `min(max_concurrent_files, accepted
//!   URLs)` worker tasks pull work items from a bounded channel; the
//!   producer (input reader + queue writer) is the only other task. A
//!   large list file is streamed line by line and never buffered whole.
//! - **Independent failures.** One download's failure only ever touches
//!   its own record; the run reports every outcome and keeps going.
//! - **Shutdown.** A tripped [`ShutdownHandle`] stops the producer from
//!   accepting more input and stops workers from starting new transfers;
//!   nothing is lost — never-started entries stay `queued`, interrupted
//!   streams park `paused`, and a later invocation resumes them.
//!
//! The returned report is completion-ordered data for the Phase 11 command
//! layer, which owns summaries, exit codes, and all user-facing redaction.

#![allow(dead_code)] // commands arrive in Phase 11; narrow waiver per AGENTS.md

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;

use crate::download::http::HttpClient;
use crate::download::probe::validate_url;
use crate::download::progress::Progress;
use crate::download::resume::{
    transfer_persisted, unresolved_multiplicity_error, ShutdownHandle, RECOVERABLE,
};
use crate::download::retry::RetryPolicy;
use crate::download::sequential::select_filename;
use crate::error::{AppError, ErrorCategory};
use crate::logging::redact_url;
use crate::storage::repository::{DownloadState, NewDownload, Repository};

/// The URLs one invocation submitted: command arguments, a list file, or
/// both (inline URLs are queued first, then the file's lines in order).
#[derive(Debug, Clone, Default)]
pub(crate) struct BatchRequest {
    pub(crate) urls: Vec<String>,
    pub(crate) url_file: Option<PathBuf>,
}

/// What happened to one accepted URL.
#[derive(Debug)]
pub(crate) struct DownloadOutcome {
    /// The normalized URL as submitted (raw here; display sites redact).
    pub(crate) url: String,
    pub(crate) result: DownloadResult,
}

/// The per-download end state of one batch run.
#[derive(Debug)]
pub(crate) enum DownloadResult {
    /// The file is finalized at `destination`.
    Completed {
        destination: PathBuf,
        bytes_written: u64,
    },
    /// The download failed on its own; the persistent record carries the
    /// same error category, and `message` is the engine's diagnostic.
    Failed {
        category: ErrorCategory,
        message: String,
    },
    /// Shutdown reached this download before completion: a never-started
    /// entry stays `queued`, an interrupted stream is parked `paused` —
    /// both are resumable by a later invocation.
    Interrupted,
}

/// The aggregate result of one batch run.
#[derive(Debug)]
pub(crate) struct BatchReport {
    /// One entry per unique accepted URL, in completion order (FIFO in
    /// input order when the concurrency limit is 1).
    pub(crate) outcomes: Vec<DownloadOutcome>,
    /// High-water mark of concurrently active file transfers. It can never
    /// exceed the configured `max_concurrent_files`.
    pub(crate) peak_active_files: u32,
}

impl BatchReport {
    /// Downloads that completed (finalized files).
    pub(crate) fn completed_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| matches!(outcome.result, DownloadResult::Completed { .. }))
            .count()
    }

    /// Downloads that failed on their own (each record carries the state).
    pub(crate) fn failed_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| matches!(outcome.result, DownloadResult::Failed { .. }))
            .count()
    }

    /// Downloads that shutdown stopped before completion.
    pub(crate) fn interrupted_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| matches!(outcome.result, DownloadResult::Interrupted))
            .count()
    }
}

/// Runs one batch: validate and normalize the whole submission, queue a
/// persistent record per unique URL, and drive the persisted engine through
/// a bounded pool of worker tasks.
///
/// Hard failures — malformed input, an unreadable list file, database
/// problems, or a worker bug — return `Err`. Everything a download itself
/// suffers is an outcome in the returned [`BatchReport`].
#[allow(clippy::too_many_arguments)] // the run is parameterized, not accidental
pub(crate) async fn run_batch(
    client: HttpClient,
    repo: Arc<Repository>,
    request: BatchRequest,
    output_dir: &Path,
    max_concurrent_files: u32,
    policy: &RetryPolicy,
    shutdown: &ShutdownHandle,
    now: fn() -> u64,
) -> Result<BatchReport, AppError> {
    let inline = validate_inline_urls(&request.urls)?;
    let file_count = match &request.url_file {
        Some(path) => Some(validate_url_list(path).await?),
        None => None,
    };
    let has_any_url = !inline.is_empty() || file_count.unwrap_or(0) > 0;
    if !has_any_url {
        return Err(no_urls_error(&request));
    }

    let scopes = ScopeIndex::build(&repo, output_dir)?;

    // Never spawn more workers than there are submitted URLs, whatever the
    // configured limit says: the pool is min(limit, inputs), and the limit
    // itself is the ceiling (`DOCS/phases.md` Phase 7: no task per URL).
    let known_urls = inline
        .len()
        .saturating_add(usize::try_from(file_count.unwrap_or(0)).unwrap_or(usize::MAX));
    let worker_count = usize::try_from(max_concurrent_files.max(1))
        .unwrap_or(1)
        .min(known_urls.max(1));

    let gauge = Arc::new(ActiveGauge::default());
    let (work_tx, work_rx) = mpsc::channel::<WorkItem>(worker_count * 2);
    let (results_tx, mut results_rx) = mpsc::channel::<DownloadOutcome>(worker_count * 2);

    let worker_receiver = Arc::new(Mutex::new(work_rx));
    let context = WorkerContext {
        client,
        repo: Arc::clone(&repo),
        output_dir: output_dir.to_owned(),
        policy: *policy,
        shutdown: shutdown.clone(),
        gauge: Arc::clone(&gauge),
        now,
    };
    let mut workers = JoinSet::new();
    for _ in 0..worker_count {
        workers.spawn(worker(
            Arc::clone(&worker_receiver),
            results_tx.clone(),
            context.clone(),
        ));
    }
    let producer = tokio::spawn(producer(
        repo,
        request,
        inline,
        output_dir.to_owned(),
        now,
        shutdown.clone(),
        scopes,
        work_tx,
        results_tx.clone(),
    ));
    drop(results_tx);

    // Drain outcomes as they arrive: this loop ends when the producer and
    // every worker have dropped their senders, which is after their tasks
    // finished — the joins below are immediate.
    let mut outcomes = Vec::new();
    while let Some(outcome) = results_rx.recv().await {
        outcomes.push(outcome);
    }

    let produced = producer.await.map_err(|err| {
        AppError::new(
            ErrorCategory::Database,
            format!("the batch input task failed unexpectedly (this is a bug): {err}"),
        )
    })?;
    produced?;
    while let Some(joined) = workers.join_next().await {
        joined.map_err(|err| {
            AppError::new(
                ErrorCategory::Database,
                format!("a batch worker task failed unexpectedly (this is a bug): {err}"),
            )
        })?;
    }

    let peak_active_files = u32::try_from(gauge.peak()).unwrap_or(u32::MAX);
    Ok(BatchReport {
        outcomes,
        peak_active_files,
    })
}

/// One scheduled download: the normalized URL; the engine resolves the
/// persistent record itself (the batch ensures the record exists first).
#[derive(Debug)]
struct WorkItem {
    url: String,
}

/// Everything a worker task needs, cloned once per worker. Cloning the
/// [`HttpClient`] shares the one configured client (and its connection
/// pool), and cloning the [`ShutdownHandle`] shares the one watch channel.
#[derive(Clone)]
struct WorkerContext {
    client: HttpClient,
    repo: Arc<Repository>,
    output_dir: PathBuf,
    policy: RetryPolicy,
    shutdown: ShutdownHandle,
    gauge: Arc<ActiveGauge>,
    now: fn() -> u64,
}

/// The scheduler's own high-water mark of concurrently active transfers.
///
/// It exists so the concurrency bound is observable (the Phase 7 gate:
/// "active file downloads never exceed the configured maximum"); the guard
/// releases the count even when a transfer returns early.
#[derive(Debug, Default)]
struct ActiveGauge {
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl ActiveGauge {
    /// Counts one active transfer until the returned guard drops.
    fn enter(self: &Arc<Self>) -> ActiveGuard {
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(active, Ordering::Relaxed);
        ActiveGuard {
            gauge: Arc::clone(self),
        }
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
}

/// Decrements the active count when the transfer it guards returns.
struct ActiveGuard {
    gauge: Arc<ActiveGauge>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.gauge.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One worker's loop: claim a work item, run the persisted engine for it,
/// and report its outcome. The receiver mutex is held only while claiming,
/// never across a transfer.
async fn worker(
    receiver: Arc<Mutex<mpsc::Receiver<WorkItem>>>,
    results: mpsc::Sender<DownloadOutcome>,
    context: WorkerContext,
) {
    loop {
        let item = {
            let mut receiver = receiver.lock().await;
            receiver.recv().await
        };
        let Some(item) = item else {
            break;
        };
        let result = if context.shutdown.tripped() {
            // The trip order was unambiguous: this item was never started.
            DownloadResult::Interrupted
        } else {
            let guard = context.gauge.enter();
            let attempt = transfer_persisted(
                &context.client,
                &context.repo,
                &item.url,
                &context.output_dir,
                Progress::shared(),
                &context.policy,
                context.now,
                &context.shutdown,
            )
            .await;
            drop(guard);
            match attempt {
                Ok(download) => DownloadResult::Completed {
                    destination: download.destination,
                    bytes_written: download.bytes_written,
                },
                Err(err) if err.category() == ErrorCategory::Cancelled => {
                    DownloadResult::Interrupted
                }
                Err(err) => {
                    // The message itself stays in the outcome (display
                    // material for Phase 11); the log line redacts and
                    // carries only the category, per the logging law.
                    tracing::warn!(
                        "the download of {} failed ({}); its persistent record carries \
                         the failure state",
                        redact_url(&item.url),
                        err.category().as_str(),
                    );
                    DownloadResult::Failed {
                        category: err.category(),
                        message: err.message().to_owned(),
                    }
                }
            }
        };
        let outcome = DownloadOutcome {
            url: item.url,
            result,
        };
        if results.send(outcome).await.is_err() {
            break;
        }
    }
}

/// The acceptance pipeline: streams the submission, ensures one persistent
/// record per unique URL, and dispatches work items. The bounded channels
/// apply backpressure, so a large list is never buffered whole.
#[allow(clippy::too_many_arguments)] // the pipeline is parameterized, not accidental
async fn producer(
    repo: Arc<Repository>,
    request: BatchRequest,
    inline: Vec<String>,
    output_dir: PathBuf,
    now: fn() -> u64,
    shutdown: ShutdownHandle,
    scopes: ScopeIndex,
    work_tx: mpsc::Sender<WorkItem>,
    results_tx: mpsc::Sender<DownloadOutcome>,
) -> Result<(), AppError> {
    let mut queuer = Queuer {
        repo,
        output_dir,
        now,
        scopes,
        seen: HashSet::new(),
        work_tx,
        results_tx,
        shutdown,
    };
    for url in &inline {
        if !queuer.accept(url).await? {
            return Ok(());
        }
    }
    let Some(path) = request.url_file.as_deref() else {
        return Ok(());
    };
    let file = open_list(path).await?;
    let mut lines = BufReader::new(file).lines();
    let mut line_number = 0u64;
    while let Some(raw) = next_line(path, &mut lines, line_number + 1).await? {
        line_number += 1;
        let Some(url) = url_from_line(path, line_number, &raw)? else {
            continue;
        };
        if !queuer.accept(&url).await? {
            return Ok(());
        }
    }
    Ok(())
}

/// The producer's queueing state: one record per unique URL, inserted or
/// adopted exactly once, dispatched to the bounded work channel.
struct Queuer {
    repo: Arc<Repository>,
    output_dir: PathBuf,
    now: fn() -> u64,
    scopes: ScopeIndex,
    seen: HashSet<String>,
    work_tx: mpsc::Sender<WorkItem>,
    results_tx: mpsc::Sender<DownloadOutcome>,
    shutdown: ShutdownHandle,
}

impl Queuer {
    /// Accepts one normalized URL: deduplicates, ensures its record exists
    /// (insert on a fresh scope, adopt an existing one, refuse a conflicted
    /// one), and dispatches it. Returns whether acceptance may continue
    /// (`false` after shutdown or a dead work channel).
    async fn accept(&mut self, url: &str) -> Result<bool, AppError> {
        if self.shutdown.tripped() {
            return Ok(false);
        }
        if !self.seen.insert(url.to_owned()) {
            tracing::info!(
                "duplicate URL {} was submitted again; the first occurrence owns this \
                 download",
                redact_url(url),
            );
            return Ok(true);
        }
        match self.scopes.action_for(url) {
            ScopeAction::Conflict(ids) => {
                let refusal = unresolved_multiplicity_error(&ids, url, &self.output_dir);
                tracing::warn!(
                    "the batch cannot schedule {}: several unfinished records claim one \
                     destination plan; the run continues with the other URLs",
                    redact_url(url),
                );
                self.report(
                    url,
                    DownloadResult::Failed {
                        category: refusal.category(),
                        message: refusal.message().to_owned(),
                    },
                )
                .await;
                Ok(true)
            }
            ScopeAction::Insert => {
                // The provisional name comes from the requested URL; the
                // first probe refreshes it to the effective URL's name
                // (ADR-0020). One transaction per accepted URL: the queue
                // is persistent before any network work.
                let provisional = select_filename(&validate_url(url)?);
                self.repo.insert_download(
                    NewDownload {
                        url: url.to_owned(),
                        destination: self.output_dir.join(&provisional),
                        file_name: provisional,
                    },
                    (self.now)(),
                )?;
                Ok(self.dispatch(url).await)
            }
            ScopeAction::Schedule => Ok(self.dispatch(url).await),
        }
    }

    /// Hands one accepted URL to the workers. A trip between acceptance
    /// and dispatch reports the item as interrupted (its record stays
    /// `queued`); a dead work channel stops acceptance, and the worker
    /// joins surface the cause.
    async fn dispatch(&self, url: &str) -> bool {
        if self.shutdown.tripped() {
            self.report(url, DownloadResult::Interrupted).await;
            return false;
        }
        self.work_tx
            .send(WorkItem {
                url: url.to_owned(),
            })
            .await
            .is_ok()
    }

    async fn report(&self, url: &str, result: DownloadResult) {
        let _ = self
            .results_tx
            .send(DownloadOutcome {
                url: url.to_owned(),
                result,
            })
            .await;
    }
}

/// What the batch must do about one submitted URL's scope.
enum ScopeAction {
    /// No unfinished or completed record exists: insert a queued record.
    Insert,
    /// A record already owns the scope (one unfinished record, or a
    /// completed one): schedule without inserting; the engine reconciles.
    Schedule,
    /// Several unfinished records claim the scope: refuse, exactly as
    /// `plan_for` refuses it (`unresolved_multiplicity_error`).
    Conflict(Vec<i64>),
}

/// The records that already own URL scopes, snapshotted once from the
/// database. The scope is exactly [`crate::download::resume::plan_for`]'s:
/// the record's URL string (the batch's normalized form) and its planned
/// destination directory, text-comparable and stable across invocations.
struct ScopeIndex {
    unfinished: HashMap<String, Vec<i64>>,
    completed: HashSet<String>,
}

impl ScopeIndex {
    fn build(repo: &Repository, output_dir: &Path) -> Result<Self, AppError> {
        let mut unfinished: HashMap<String, Vec<i64>> = HashMap::new();
        let mut completed: HashSet<String> = HashSet::new();
        for record in repo.list_downloads()? {
            if !record
                .destination
                .parent()
                .is_some_and(|parent| parent == output_dir)
            {
                continue;
            }
            if RECOVERABLE.contains(&record.status) {
                unfinished.entry(record.url).or_default().push(record.id);
            } else if record.status == DownloadState::Completed {
                completed.insert(record.url);
            }
        }
        Ok(Self {
            unfinished,
            completed,
        })
    }

    fn action_for(&self, url: &str) -> ScopeAction {
        match self.unfinished.get(url).map_or(&[][..], Vec::as_slice) {
            [] if self.completed.contains(url) => ScopeAction::Schedule,
            [] => ScopeAction::Insert,
            [_] => ScopeAction::Schedule,
            many => ScopeAction::Conflict(many.to_vec()),
        }
    }
}

/// Validates and normalizes the inline URLs; a malformed URL is refused
/// before anything is queued or fetched.
fn validate_inline_urls(urls: &[String]) -> Result<Vec<String>, AppError> {
    urls.iter()
        .map(|url| validate_url(url).map(|parsed| parsed.to_string()))
        .collect()
}

/// Pass 1 over a list file: validates every line and counts the usable
/// URLs, so malformed input fails before anything is queued (all-or-nothing
/// acceptance). Returns the number of URL lines (blank and comment lines
/// never count).
async fn validate_url_list(path: &Path) -> Result<u64, AppError> {
    let file = open_list(path).await?;
    let mut lines = BufReader::new(file).lines();
    let mut count = 0u64;
    let mut line_number = 0u64;
    while let Some(raw) = next_line(path, &mut lines, line_number + 1).await? {
        line_number += 1;
        if url_from_line(path, line_number, &raw)?.is_some() {
            count += 1;
        }
    }
    Ok(count)
}

/// Opens the URL list file, typed for the user's terminal.
async fn open_list(path: &Path) -> Result<tokio::fs::File, AppError> {
    tokio::fs::File::open(path).await.map_err(|err| {
        AppError::new(
            ErrorCategory::Filesystem,
            format!(
                "the URL list file {} could not be read: {err}",
                path.display()
            ),
        )
        .with_source(err)
    })
}

/// Reads the next line of a list file; read failures are typed, with
/// undecodable bytes reported as input rather than environment problems.
async fn next_line(
    path: &Path,
    lines: &mut tokio::io::Lines<BufReader<tokio::fs::File>>,
    line_number: u64,
) -> Result<Option<String>, AppError> {
    lines.next_line().await.map_err(|err| {
        let error = if err.kind() == std::io::ErrorKind::InvalidData {
            AppError::new(
                ErrorCategory::Input,
                format!(
                    "the URL list file {} line {line_number} is not valid UTF-8: {err}",
                    path.display()
                ),
            )
        } else {
            AppError::new(
                ErrorCategory::Filesystem,
                format!(
                    "the URL list file {} could not be read: {err}",
                    path.display()
                ),
            )
        };
        error.with_source(err)
    })
}

/// Interprets one raw list line: `None` for blank and `#`-comment lines,
/// `Some(normalized URL)` otherwise; malformed lines are typed errors that
/// name the file and the 1-based line number. Both passes call this, so
/// validation and queueing can never disagree about a line.
fn url_from_line(path: &Path, line_number: u64, raw: &str) -> Result<Option<String>, AppError> {
    // A UTF-8 BOM is legal only at the very start of the file; Windows
    // editors add it silently, so it must not poison the first URL.
    let raw = if line_number == 1 {
        raw.strip_prefix('\u{feff}').unwrap_or(raw)
    } else {
        raw
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    let parsed = validate_url(trimmed).map_err(|err| {
        AppError::new(
            ErrorCategory::Input,
            format!(
                "the URL list file {} line {line_number}: {}",
                path.display(),
                err.message()
            ),
        )
        .with_source(err)
    })?;
    Ok(Some(parsed.to_string()))
}

/// The empty submission refusal; an empty list file names the file, a
/// fully empty invocation mirrors the Phase 1 CLI wording.
fn no_urls_error(request: &BatchRequest) -> AppError {
    match &request.url_file {
        Some(path) => AppError::new(
            ErrorCategory::Input,
            format!(
                "the URL list file {} contains no URLs: blank lines and #-comment \
                 lines are ignored, so nothing was queued",
                path.display()
            ),
        ),
        None => AppError::input(
            "provide one or more HTTP or HTTPS URLs, or a URL-list file with \
             --url-file",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::fixture::Fixture;
    use crate::common::server::{Behavior, MockServer};
    use crate::download::resume::ShutdownSignal;
    use crate::storage::repository::DownloadRecord;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::time::{Duration, Instant};

    /// An isolated, auto-removed scratch directory for one test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "swiftfetch-p7-{tag}-{}-{stamp}",
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
        static TICK: AtomicU64 = AtomicU64::new(1_800_000_000);
        TICK.fetch_add(1, AtomicOrdering::Relaxed)
    }

    /// A scratch directory with an empty database; the path is returned so
    /// restart tests can reopen the same database as a second connection.
    fn scratch_repo(tag: &str) -> (Scratch, Arc<Repository>, PathBuf) {
        let scratch = Scratch::new(tag);
        let database = scratch.path().join("queue.sqlite3");
        let repo = Arc::new(Repository::open(&database).expect("scratch repository opened"));
        (scratch, repo, database)
    }

    /// Runs one batch with the default client and no retries.
    async fn run(
        repo: &Arc<Repository>,
        request: BatchRequest,
        dir: &Path,
        limit: u32,
        shutdown: &ShutdownHandle,
    ) -> Result<BatchReport, AppError> {
        run_batch(
            HttpClient::default_client().expect("the default client builds"),
            Arc::clone(repo),
            request,
            dir,
            limit,
            &RetryPolicy::from_retries(0),
            shutdown,
            test_clock,
        )
        .await
    }

    fn inline(urls: &[String]) -> BatchRequest {
        BatchRequest {
            urls: urls.to_vec(),
            url_file: None,
        }
    }

    fn write_list(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("list file written");
        path
    }

    fn outcome<'a>(report: &'a BatchReport, url: &str) -> &'a DownloadOutcome {
        report
            .outcomes
            .iter()
            .find(|outcome| outcome.url == url)
            .unwrap_or_else(|| panic!("no outcome for {url}: {:?}", report.outcomes))
    }

    fn records(repo: &Repository) -> Vec<DownloadRecord> {
        repo.list_downloads().expect("list downloads")
    }

    // ---------------------------------------------------------------------
    // Acceptance: one URL, several URLs, URL-list files, blank lines,
    // comments, duplicates, and normalization.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn one_url_downloads_and_reports_completion() {
        let server = MockServer::start().await;
        let fixture = Fixture::default();
        server.mount_route("/only.bin", Behavior::default());
        let (scratch, repo, _db) = scratch_repo("single-url");
        let (_signal, handle) = ShutdownSignal::channel();
        let url = server.url("/only.bin");

        let report = run(
            &repo,
            inline(std::slice::from_ref(&url)),
            scratch.path(),
            3,
            &handle,
        )
        .await
        .expect("one URL is a valid batch");
        assert_eq!(report.completed_count(), 1);
        assert_eq!(report.failed_count(), 0);
        assert_eq!(report.interrupted_count(), 0);
        assert_eq!(report.peak_active_files, 1);
        match &outcome(&report, &url).result {
            DownloadResult::Completed {
                destination,
                bytes_written,
            } => {
                assert_eq!(destination, &scratch.path().join("only.bin"));
                assert_eq!(*bytes_written, fixture.len);
                assert_eq!(
                    std::fs::read(destination).expect("final file").as_slice(),
                    fixture.bytes().as_slice()
                );
            }
            other => panic!("expected a completed download, got {other:?}"),
        }
        let stored = records(&repo);
        assert_eq!(stored.len(), 1, "one record for one URL");
        assert_eq!(stored[0].status, DownloadState::Completed);
        assert_eq!(stored[0].url, url);
    }

    #[tokio::test]
    async fn several_urls_download_in_one_invocation() {
        let server = MockServer::start().await;
        let fixture = Fixture::default();
        let names = ["several-a.bin", "several-b.bin", "several-c.bin"];
        for name in names {
            server.mount_route(&format!("/{name}"), Behavior::default());
        }
        let (scratch, repo, _db) = scratch_repo("several");
        let (_signal, handle) = ShutdownSignal::channel();
        let urls: Vec<String> = names
            .iter()
            .map(|name| server.url(&format!("/{name}")))
            .collect();

        let report = run(&repo, inline(&urls), scratch.path(), 3, &handle)
            .await
            .expect("several URLs are one valid batch");
        assert_eq!(report.completed_count(), 3);
        for name in names {
            let path = scratch.path().join(name);
            assert_eq!(
                std::fs::read(&path).expect("final file").as_slice(),
                fixture.bytes().as_slice(),
                "{name} must hold the fixture bytes"
            );
        }
        let stored = records(&repo);
        assert_eq!(stored.len(), 3);
        assert!(stored
            .iter()
            .all(|record| record.status == DownloadState::Completed));
    }

    #[tokio::test]
    async fn a_url_list_file_supplies_downloads() {
        let server = MockServer::start().await;
        let fixture = Fixture::default();
        let names = ["listed-a.bin", "listed-b.bin", "listed-c.bin"];
        for name in names {
            server.mount_route(&format!("/{name}"), Behavior::default());
        }
        let (scratch, repo, _db) = scratch_repo("list-file");
        let (_signal, handle) = ShutdownSignal::channel();
        let content: String = names
            .iter()
            .map(|name| format!("{}\n", server.url(&format!("/{name}"))))
            .collect();
        let path = write_list(scratch.path(), "urls.txt", &content);

        let report = run(
            &repo,
            BatchRequest {
                urls: Vec::new(),
                url_file: Some(path),
            },
            scratch.path(),
            3,
            &handle,
        )
        .await
        .expect("a list file with URLs is a valid batch");
        assert_eq!(report.completed_count(), 3);
        for name in names {
            assert_eq!(
                std::fs::read(scratch.path().join(name))
                    .expect("final file")
                    .as_slice(),
                fixture.bytes().as_slice()
            );
        }
        assert_eq!(records(&repo).len(), 3);
    }

    #[tokio::test]
    async fn inline_urls_and_a_list_file_merge_and_deduplicate() {
        let server = MockServer::start().await;
        let names = ["merge-a.bin", "merge-b.bin", "merge-c.bin"];
        for name in names {
            server.mount_route(&format!("/{name}"), Behavior::default());
        }
        let (scratch, repo, _db) = scratch_repo("merge");
        let (_signal, handle) = ShutdownSignal::channel();
        let urls: Vec<String> = names
            .iter()
            .map(|name| server.url(&format!("/{name}")))
            .collect();
        // The file repeats the inline URL; only the first occurrence owns a
        // download.
        let content = format!("{}\n{}\n{}\n", urls[0], urls[1], urls[2]);
        let path = write_list(scratch.path(), "urls.txt", &content);

        let report = run(
            &repo,
            BatchRequest {
                urls: vec![urls[0].clone()],
                url_file: Some(path),
            },
            scratch.path(),
            3,
            &handle,
        )
        .await
        .expect("inline URLs and a list file merge");
        assert_eq!(
            report.outcomes.len(),
            3,
            "the duplicate collapses onto the inline occurrence: {:?}",
            report.outcomes
        );
        for url in &urls {
            outcome(&report, url);
        }
        assert_eq!(records(&repo).len(), 3);
    }

    #[tokio::test]
    async fn blank_comment_lines_and_a_bom_are_tolerated() {
        let server = MockServer::start().await;
        let (scratch, repo, _db) = scratch_repo("list-text-rules");
        let (_signal, handle) = ShutdownSignal::channel();
        let first = server.url("/text-rules-a.bin");
        let second = server.url("/text-rules-b.bin");
        server.mount_route("/text-rules-a.bin", Behavior::default());
        server.mount_route("/text-rules-b.bin", Behavior::default());
        // Windows editors love BOMs; comments and blanks must never become
        // URLs, and a BOM must not poison the first line.
        let content =
            format!("\u{feff}{first}\n\n   \n# a comment\n  # an indented comment\n{second}\n");
        let path = write_list(scratch.path(), "urls.txt", &content);

        let report = run(
            &repo,
            BatchRequest {
                urls: Vec::new(),
                url_file: Some(path),
            },
            scratch.path(),
            2,
            &handle,
        )
        .await
        .expect("comments and blank lines are skipped");
        assert_eq!(report.completed_count(), 2);
        assert_eq!(records(&repo).len(), 2);
        assert!(scratch.path().join("text-rules-a.bin").exists());
        assert!(scratch.path().join("text-rules-b.bin").exists());
    }

    #[tokio::test]
    async fn duplicate_urls_download_once() {
        let server = MockServer::start().await;
        server.mount_route("/dup.bin", Behavior::default());
        let (scratch, repo, _db) = scratch_repo("duplicates");
        let (_signal, handle) = ShutdownSignal::channel();
        let url = server.url("/dup.bin");

        let report = run(
            &repo,
            inline(&[url.clone(), url.clone(), url.clone()]),
            scratch.path(),
            3,
            &handle,
        )
        .await
        .expect("duplicates are not an error");
        assert_eq!(report.outcomes.len(), 1, "three copies, one download");
        assert_eq!(report.completed_count(), 1);
        assert_eq!(records(&repo).len(), 1);
    }

    #[tokio::test]
    async fn submitted_urls_are_normalized_before_queueing() {
        let server = MockServer::start().await;
        server.mount_route("/normalized.bin", Behavior::default());
        let (scratch, repo, _db) = scratch_repo("normalization");
        let (_signal, handle) = ShutdownSignal::channel();
        let url = server.url("/normalized.bin");
        // The loud twin is the same URL after normalization, so the pair
        // must collapse into one download with the normalized record.
        let loud = format!("HTTP://{}", &url["http://".len()..]);

        let report = run(
            &repo,
            inline(&[loud.clone(), url.clone()]),
            scratch.path(),
            2,
            &handle,
        )
        .await
        .expect("a case-variant scheme is still a URL");
        assert_eq!(report.outcomes.len(), 1, "normalized duplicates collapse");
        assert_eq!(report.outcomes[0].url, url, "the stored form is normalized");
        let stored = records(&repo);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].url, url);
    }

    // ---------------------------------------------------------------------
    // Refusals: empty input, malformed URLs, unreadable files.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn an_empty_submission_is_refused() {
        let (scratch, repo, _db) = scratch_repo("empty-submission");
        let (_signal, handle) = ShutdownSignal::channel();

        let err = run(&repo, BatchRequest::default(), scratch.path(), 3, &handle)
            .await
            .expect_err("no URLs is not a batch");
        assert_eq!(err.category(), ErrorCategory::Input);
        assert!(
            err.message().contains("one or more"),
            "the message explains what to provide: {}",
            err.message()
        );
        assert!(records(&repo).is_empty());
    }

    #[tokio::test]
    async fn an_empty_list_file_is_refused() {
        let (scratch, repo, _db) = scratch_repo("empty-list");
        let (_signal, handle) = ShutdownSignal::channel();

        for content in ["", "\n\n   \n# only comments\n  # indented\n"] {
            let path = write_list(scratch.path(), "empty.txt", content);
            let err = run(
                &repo,
                BatchRequest {
                    urls: Vec::new(),
                    url_file: Some(path),
                },
                scratch.path(),
                3,
                &handle,
            )
            .await
            .expect_err("a file without URLs is not a batch");
            assert_eq!(err.category(), ErrorCategory::Input);
            assert!(
                err.message().contains("empty.txt"),
                "the message names the file: {}",
                err.message()
            );
        }
        assert!(records(&repo).is_empty());
    }

    #[tokio::test]
    async fn a_malformed_inline_url_is_refused_before_any_work() {
        let server = MockServer::start().await;
        server.mount_route("/never-reached.bin", Behavior::default());
        let (scratch, repo, _db) = scratch_repo("malformed-inline");
        let (_signal, handle) = ShutdownSignal::channel();

        for bad in ["not a url", "ftp://host/file.bin"] {
            let err = run(
                &repo,
                inline(&[server.url("/never-reached.bin"), bad.to_owned()]),
                scratch.path(),
                3,
                &handle,
            )
            .await
            .expect_err("a malformed URL refuses the whole submission");
            assert_eq!(err.category(), ErrorCategory::Input);
            assert!(
                err.message().contains(bad),
                "the message names the offender: {}",
                err.message()
            );
        }
        assert!(records(&repo).is_empty(), "nothing is queued on refusal");
        assert!(
            server.requests().is_empty(),
            "validation precedes every request"
        );
    }

    #[tokio::test]
    async fn a_malformed_list_line_is_refused_with_its_line_number() {
        let server = MockServer::start().await;
        server.mount_route("/line-ok.bin", Behavior::default());
        let (scratch, repo, _db) = scratch_repo("malformed-line");
        let (_signal, handle) = ShutdownSignal::channel();
        let url = server.url("/line-ok.bin");
        let content = format!("{url}\n# a fine comment\ntotally wrong\n{url}\n");
        let path = write_list(scratch.path(), "urls.txt", &content);

        let err = run(
            &repo,
            BatchRequest {
                urls: Vec::new(),
                url_file: Some(path.clone()),
            },
            scratch.path(),
            3,
            &handle,
        )
        .await
        .expect_err("a malformed line refuses the whole submission");
        assert_eq!(err.category(), ErrorCategory::Input);
        assert!(
            err.message().contains("urls.txt") && err.message().contains("line 3"),
            "the message names the file and the line: {}",
            err.message()
        );
        assert!(
            records(&repo).is_empty(),
            "validation precedes queueing: all-or-nothing acceptance"
        );
        assert!(
            server.requests().is_empty(),
            "validation precedes every request"
        );
    }

    #[tokio::test]
    async fn an_unreadable_list_file_is_refused() {
        let (scratch, repo, _db) = scratch_repo("unreadable-list");
        let (_signal, handle) = ShutdownSignal::channel();
        let missing = scratch.path().join("missing-urls.txt");

        let err = run(
            &repo,
            BatchRequest {
                urls: Vec::new(),
                url_file: Some(missing),
            },
            scratch.path(),
            3,
            &handle,
        )
        .await
        .expect_err("a missing list file is refused");
        assert_eq!(err.category(), ErrorCategory::Filesystem);
        assert!(
            err.message().contains("missing-urls.txt"),
            "the message names the file: {}",
            err.message()
        );
        assert!(records(&repo).is_empty());
    }

    // ---------------------------------------------------------------------
    // Per-download independence, mixed results, and idempotence.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn one_failure_only_touches_its_own_record() {
        let server = MockServer::start().await;
        server.mount_route("/indep-a.bin", Behavior::default());
        server.mount_route("/indep-b.bin", Behavior::default());
        server.mount_route("/indep-404.bin", Behavior::default().with_status(404));
        let (scratch, repo, _db) = scratch_repo("independent-failure");
        let (_signal, handle) = ShutdownSignal::channel();
        let good_a = server.url("/indep-a.bin");
        let good_b = server.url("/indep-b.bin");
        let bad = server.url("/indep-404.bin");

        let report = run(
            &repo,
            inline(&[good_a.clone(), bad.clone(), good_b.clone()]),
            scratch.path(),
            2,
            &handle,
        )
        .await
        .expect("per-download failures are outcomes, not batch errors");
        assert_eq!(report.completed_count(), 2);
        assert_eq!(report.failed_count(), 1);
        match &outcome(&report, &bad).result {
            DownloadResult::Failed { category, message } => {
                assert_eq!(*category, ErrorCategory::Http);
                assert!(
                    message.contains("HTTP 404"),
                    "the message reports the genuine answer: {message}"
                );
            }
            other => panic!("expected the 404 to fail, got {other:?}"),
        }

        let stored = records(&repo);
        assert_eq!(stored.len(), 3);
        let stored_bad = stored
            .iter()
            .find(|record| record.url == bad)
            .expect("the 404 record");
        assert_eq!(stored_bad.status, DownloadState::Failed);
        assert_eq!(stored_bad.error_category, Some(ErrorCategory::Http));
        for url in [&good_a, &good_b] {
            let record = stored
                .iter()
                .find(|record| &record.url == url)
                .expect("a good record");
            assert_eq!(
                record.status,
                DownloadState::Completed,
                "an unrelated download must never be falsely marked: {record:?}"
            );
            assert_eq!(record.error_category, None);
        }
        assert!(scratch.path().join("indep-a.bin").exists());
        assert!(scratch.path().join("indep-b.bin").exists());
        assert!(!scratch.path().join("indep-404.bin").exists());
    }

    #[tokio::test]
    async fn mixed_success_reports_each_downloads_result() {
        let server = MockServer::start().await;
        server.mount_route("/mixed-a.bin", Behavior::default());
        server.mount_route("/mixed-b.bin", Behavior::default());
        server.mount_route("/mixed-404.bin", Behavior::default().with_status(404));
        server.mount_route("/mixed-500.bin", Behavior::default().with_status(500));
        let (scratch, repo, _db) = scratch_repo("mixed");
        let (_signal, handle) = ShutdownSignal::channel();
        let good = server.url("/mixed-a.bin");
        let also_good = server.url("/mixed-b.bin");
        let not_found = server.url("/mixed-404.bin");
        let broken = server.url("/mixed-500.bin");

        let report = run(
            &repo,
            inline(&[
                good.clone(),
                not_found.clone(),
                also_good.clone(),
                broken.clone(),
            ]),
            scratch.path(),
            4,
            &handle,
        )
        .await
        .expect("a mixed batch still finishes");
        assert_eq!(report.outcomes.len(), 4);
        assert_eq!(report.completed_count(), 2);
        assert_eq!(report.failed_count(), 2);
        assert_eq!(report.interrupted_count(), 0);

        match &outcome(&report, &not_found).result {
            DownloadResult::Failed { category, .. } => {
                assert_eq!(*category, ErrorCategory::Http)
            }
            other => panic!("expected the 404 to fail, got {other:?}"),
        }
        // Retries were budgeted at zero, so the 5xx failure is an honest
        // exhaustion rather than an invented permanent failure.
        match &outcome(&report, &broken).result {
            DownloadResult::Failed { category, message } => {
                assert_eq!(*category, ErrorCategory::RetryExhausted);
                assert!(
                    message.contains("could not finish within 1 attempts"),
                    "the message names the budget: {message}"
                );
            }
            other => panic!("expected the 5xx to exhaust, got {other:?}"),
        }

        let stored = records(&repo);
        let stored_broken = stored
            .iter()
            .find(|record| record.url == broken)
            .expect("the 5xx record");
        assert_eq!(stored_broken.status, DownloadState::Failed);
        assert_eq!(
            stored_broken.error_category,
            Some(ErrorCategory::RetryExhausted)
        );
        assert!(scratch.path().join("mixed-a.bin").exists());
        assert!(scratch.path().join("mixed-b.bin").exists());
        assert!(!scratch.path().join("mixed-404.bin").exists());
        assert!(!scratch.path().join("mixed-500.bin").exists());
    }

    #[tokio::test]
    async fn resubmitting_a_completed_url_is_an_idempotent_no_op() {
        let server = MockServer::start().await;
        server.mount_route("/idem.bin", Behavior::default());
        let (scratch, repo, _db) = scratch_repo("idempotent");
        let (_signal, handle) = ShutdownSignal::channel();
        let url = server.url("/idem.bin");

        let first = run(
            &repo,
            inline(std::slice::from_ref(&url)),
            scratch.path(),
            1,
            &handle,
        )
        .await
        .expect("the first run succeeds");
        assert_eq!(first.completed_count(), 1);
        let hits_after_first = server.route_hits("/idem.bin");

        let second = run(
            &repo,
            inline(std::slice::from_ref(&url)),
            scratch.path(),
            1,
            &handle,
        )
        .await
        .expect("the second run is a no-op, not an error");
        assert_eq!(second.completed_count(), 1);
        assert_eq!(
            server.route_hits("/idem.bin"),
            hits_after_first,
            "the completed download is never re-fetched"
        );
        assert_eq!(records(&repo).len(), 1, "no duplicate record appears");
    }

    #[tokio::test]
    async fn several_unfinished_records_for_one_scope_are_refused_per_download() {
        let server = MockServer::start().await;
        server.mount_route("/conflicted.bin", Behavior::default());
        let (scratch, repo, _db) = scratch_repo("conflict");
        let (_signal, handle) = ShutdownSignal::channel();
        let url = server.url("/conflicted.bin");
        for _ in 0..2 {
            repo.insert_download(
                NewDownload {
                    url: url.clone(),
                    destination: scratch.path().join("conflicted.bin"),
                    file_name: "conflicted.bin".to_owned(),
                },
                test_clock(),
            )
            .expect("staged record");
        }

        let report = run(
            &repo,
            inline(std::slice::from_ref(&url)),
            scratch.path(),
            3,
            &handle,
        )
        .await
        .expect("the conflict belongs to one URL, not the batch");
        assert_eq!(report.failed_count(), 1);
        match &outcome(&report, &url).result {
            DownloadResult::Failed { category, message } => {
                assert_eq!(*category, ErrorCategory::Input);
                assert!(
                    message.contains("cannot share one destination plan"),
                    "the refusal mirrors the engine's law: {message}"
                );
            }
            other => panic!("expected the conflicted URL to fail, got {other:?}"),
        }
        let stored = records(&repo);
        assert_eq!(stored.len(), 2, "no third record was inserted");
        assert!(stored
            .iter()
            .all(|record| record.status == DownloadState::Queued));
        assert!(
            server.requests().is_empty(),
            "a conflicted scope never touches the network"
        );
    }

    // ---------------------------------------------------------------------
    // Bounded concurrency and scale.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn active_file_downloads_never_exceed_the_configured_limit() {
        let server = MockServer::start().await;
        let mut urls = Vec::new();
        for index in 0..6 {
            let path = format!("/bounded-{index}.bin");
            server.mount_route(&path, Behavior::default().with_delay_body_ms(200));
            urls.push(server.url(&path));
        }
        let (scratch, repo, _db) = scratch_repo("bounded");
        let (_signal, handle) = ShutdownSignal::channel();

        let started = Instant::now();
        let report = run(&repo, inline(&urls), scratch.path(), 2, &handle)
            .await
            .expect("six downloads are one batch");
        let elapsed = started.elapsed();

        assert_eq!(report.completed_count(), 6);
        assert_eq!(
            report.peak_active_files, 2,
            "with six delayed downloads and a limit of two, both slots are used"
        );
        // The body delay sleeps between 512-byte frames (7 waits per 4 KiB
        // fixture), so one download takes ~1.4 s; six downloads over two
        // slots run in three waves (~4.2 s). A scheduler that started all
        // six at once (one wave) or three at once (two waves) finishes
        // well inside this floor, while serialized waves cannot.
        assert!(
            elapsed >= Duration::from_millis(3200),
            "three serialized waves of delayed bodies cannot finish in {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_concurrency_limit_of_one_serializes_in_submission_order() {
        let server = MockServer::start().await;
        let names = ["serial-a.bin", "serial-b.bin", "serial-c.bin"];
        for name in names {
            server.mount_route(&format!("/{name}"), Behavior::default());
        }
        let (scratch, repo, _db) = scratch_repo("serial");
        let (_signal, handle) = ShutdownSignal::channel();
        let urls: Vec<String> = names
            .iter()
            .map(|name| server.url(&format!("/{name}")))
            .collect();

        let report = run(&repo, inline(&urls), scratch.path(), 1, &handle)
            .await
            .expect("three downloads serialize");
        assert_eq!(report.completed_count(), 3);
        assert_eq!(report.peak_active_files, 1);
        let order: Vec<&str> = report
            .outcomes
            .iter()
            .map(|outcome| outcome.url.as_str())
            .collect();
        let expected: Vec<&str> = urls.iter().map(String::as_str).collect();
        assert_eq!(order, expected, "one worker completes in submission order");
    }

    #[tokio::test]
    async fn a_large_url_list_is_processed_within_bounded_resources() {
        let server = MockServer::start().await;
        server.mount_route("/large-list.bin", Behavior::default().with_status(404));
        let (scratch, repo, _db) = scratch_repo("large-list");
        let (_signal, handle) = ShutdownSignal::channel();
        let base = server.url("/large-list.bin");
        // Distinct query strings keep every line a separate submission
        // (duplicates would collapse by design) while hitting one route.
        let mut content = String::new();
        for index in 0..600 {
            content.push_str(&format!("{base}?line={index}\n"));
        }
        let path = write_list(scratch.path(), "large.txt", &content);

        let report = run(
            &repo,
            BatchRequest {
                urls: Vec::new(),
                url_file: Some(path),
            },
            scratch.path(),
            4,
            &handle,
        )
        .await
        .expect("a large list is one batch");
        assert_eq!(report.outcomes.len(), 600);
        assert_eq!(report.failed_count(), 600, "every URL answers 404");
        assert!(
            (2..=4).contains(&report.peak_active_files),
            "bounded by the limit and actually concurrent: {}",
            report.peak_active_files
        );
        let stored = records(&repo);
        assert_eq!(stored.len(), 600);
        assert!(stored
            .iter()
            .all(|record| record.status == DownloadState::Failed));
    }

    // ---------------------------------------------------------------------
    // Persistence, restart, and shutdown.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn an_interrupted_batch_leaves_its_records_for_a_later_invocation() {
        let server = MockServer::start().await;
        let fixture = Fixture::default();
        let names = ["restart-one.bin", "restart-two.bin", "restart-three.bin"];
        for name in names {
            server.mount_route(
                &format!("/{name}"),
                Behavior::default().with_delay_body_ms(250),
            );
        }
        let (scratch, repo, database) = scratch_repo("restart-batch");
        let dir = scratch.path().to_owned();
        let urls: Vec<String> = names
            .iter()
            .map(|name| server.url(&format!("/{name}")))
            .collect();
        let (signal, handle) = ShutdownSignal::channel();

        let first_run = tokio::spawn({
            let repo = Arc::clone(&repo);
            let handle = handle.clone();
            let dir = dir.clone();
            let urls = urls.clone();
            async move {
                run_batch(
                    HttpClient::default_client().expect("the default client builds"),
                    repo,
                    BatchRequest {
                        urls,
                        url_file: None,
                    },
                    &dir,
                    1,
                    &RetryPolicy::from_retries(0),
                    &handle,
                    test_clock,
                )
                .await
            }
        });

        // Deterministic trip: wait until the first file's unranged GET (the
        // transfer itself) is streaming — body delays keep it open — then
        // shut down while that one file owns the only worker slot.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let streaming = server.requests().iter().any(|request| {
                request.method == "GET"
                    && request.path == "/restart-one.bin"
                    && request.range.is_none()
            });
            if streaming {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the first transfer never started streaming"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        signal.trip();
        let report = first_run
            .await
            .expect("the first run task joins")
            .expect("shutdown is an outcome, not an error");
        assert_eq!(report.completed_count(), 0);
        assert_eq!(report.interrupted_count(), 3);

        // What a crash-safe run leaves behind: the active stream parked
        // paused with its partial, the two waiting entries still queued.
        let stored = records(&repo);
        assert_eq!(stored.len(), 3, "every accepted URL owns a record");
        let paused: Vec<&DownloadRecord> = stored
            .iter()
            .filter(|record| record.status == DownloadState::Paused)
            .collect();
        let queued: Vec<&DownloadRecord> = stored
            .iter()
            .filter(|record| record.status == DownloadState::Queued)
            .collect();
        assert_eq!(
            paused.len(),
            1,
            "the interrupted stream is resumable: {stored:?}"
        );
        assert_eq!(paused[0].url, urls[0]);
        assert_eq!(
            queued.len(),
            2,
            "never-started entries stay queued: {stored:?}"
        );

        // "A later invocation": reopen the database and run the same
        // submission; every unfinished entry is adopted and finished, and no
        // record is duplicated.
        let reopened = Arc::new(Repository::open(&database).expect("reopened"));
        let (_second_signal, second_handle) = ShutdownSignal::channel();
        let second = run(&reopened, inline(&urls), &dir, 1, &second_handle)
            .await
            .expect("the later invocation resumes");
        assert_eq!(second.completed_count(), 3);
        let stored = records(&reopened);
        assert_eq!(stored.len(), 3, "adoption never duplicates: {stored:?}");
        assert!(stored
            .iter()
            .all(|record| record.status == DownloadState::Completed));
        for name in names {
            assert_eq!(
                std::fs::read(dir.join(name))
                    .expect("final file")
                    .as_slice(),
                fixture.bytes().as_slice()
            );
        }
    }

    #[tokio::test]
    async fn queued_records_are_visible_before_their_transfer_starts() {
        let server = MockServer::start().await;
        let names = ["pending-first.bin", "pending-second.bin"];
        server.mount_route(
            "/pending-first.bin",
            Behavior::default().with_delay_body_ms(300),
        );
        server.mount_route("/pending-second.bin", Behavior::default());
        let (scratch, repo, database) = scratch_repo("queued-visible");
        let dir = scratch.path().to_owned();
        let urls: Vec<String> = names
            .iter()
            .map(|name| server.url(&format!("/{name}")))
            .collect();
        let (_signal, handle) = ShutdownSignal::channel();

        let run_task = tokio::spawn({
            let repo = Arc::clone(&repo);
            let handle = handle.clone();
            let dir = dir.clone();
            let urls = urls.clone();
            async move {
                run_batch(
                    HttpClient::default_client().expect("the default client builds"),
                    repo,
                    BatchRequest {
                        urls,
                        url_file: None,
                    },
                    &dir,
                    1,
                    &RetryPolicy::from_retries(0),
                    &handle,
                    test_clock,
                )
                .await
            }
        });

        // A concurrent reader on the same database (WAL) sees the second
        // URL queued while the first one is still downloading.
        let reader = Repository::open(&database).expect("a second reader opens the same database");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let stored = records(&reader);
            let first_active = stored.iter().any(|record| {
                matches!(
                    record.status,
                    DownloadState::Probing | DownloadState::Downloading | DownloadState::Completed
                )
            });
            let second_queued = stored
                .iter()
                .any(|record| record.status == DownloadState::Queued);
            if first_active && second_queued {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the persistent queue was never visible while work was active: {stored:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let report = run_task
            .await
            .expect("the run task joins")
            .expect("the batch finishes");
        assert_eq!(report.completed_count(), 2);
        assert_eq!(records(&repo).len(), 2);
    }

    #[tokio::test]
    async fn a_pretripped_shutdown_accepts_nothing() {
        let server = MockServer::start().await;
        server.mount_route("/stopped-before.bin", Behavior::default());
        let (scratch, repo, _db) = scratch_repo("pretripped");
        let (signal, handle) = ShutdownSignal::channel();
        signal.trip();

        let report = run(
            &repo,
            inline(&[server.url("/stopped-before.bin")]),
            scratch.path(),
            3,
            &handle,
        )
        .await
        .expect("a tripped shutdown ends the run, it does not fail it");
        assert!(report.outcomes.is_empty());
        assert_eq!(report.peak_active_files, 0);
        assert!(
            records(&repo).is_empty(),
            "nothing is accepted after a trip"
        );
        assert!(server.requests().is_empty(), "no work starts after a trip");
    }

    // ---------------------------------------------------------------------
    // Naming: the queued record's provisional name is refreshed from the
    // effective URL after redirects (ADR-0016 / ADR-0020).
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn a_queued_download_takes_its_name_from_the_effective_url() {
        let server = MockServer::start().await;
        let fixture = Fixture::default();
        let target = server.url("/target/real-file.bin");
        server.mount_route(
            "/moved",
            Behavior::default().with_redirect(302, target.clone()),
        );
        server.mount_route("/target/real-file.bin", Behavior::default());
        let (scratch, repo, _db) = scratch_repo("redirect-name");
        let (_signal, handle) = ShutdownSignal::channel();

        let report = run(
            &repo,
            inline(&[server.url("/moved")]),
            scratch.path(),
            1,
            &handle,
        )
        .await
        .expect("a redirecting URL downloads");
        assert_eq!(report.completed_count(), 1);
        assert!(
            scratch.path().join("real-file.bin").exists(),
            "the file is named after the effective URL"
        );
        assert!(
            !scratch.path().join("moved").exists(),
            "the provisional name never materializes"
        );
        let stored = records(&repo);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].file_name, "real-file.bin");
        assert_eq!(stored[0].destination, scratch.path().join("real-file.bin"));
        assert_eq!(
            std::fs::read(stored[0].destination.clone())
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
    }
}
