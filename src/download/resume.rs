//! Single-file resume, retry, and restart recovery (Phase 6).
//!
//! This is the persisted engine: it drives one URL into one output
//! directory through probe → stream → finalize while writing every
//! trustworthy step into the SQLite record first (`DOCS/phases.md`
//! Phase 6), and it decides, before anything is fetched, what an
//! unfinished record's disk-and-database state can safely support:
//!
//! - **Trust rule (ADR-0019):** the resumable offset is
//!   `min(database-confirmed segment progress, partial file length)`.
//!   Bytes confirmed by the database but missing on disk are deconfirmed
//!   (a write and its commit are not one transaction; the file watches the
//!   loss), so a crash between the file write and the database commit is
//!   safe in both directions.
//! - **Resume only on revalidated identity:** a ranged continuation
//!   requires a stored validator, unchanged identity evidence, proven
//!   `Range` support, an exact `206` whose `Content-Range` starts at the
//!   offset, and a response that carries the representation's validators
//!   (the mock does not honor `If-Range`, so the engine cross-checks the
//!   answer itself). A `200`, a `416`, an unverifiable answer, and a
//!   length contradiction all downgrade to a full restart inside the same
//!   attempt.
//! - **Bounded retries:** transient failures (5xx, timeouts, transport
//!   drops, classified structurally by
//!   [`crate::download::retry::classify`]) retry within
//!   `retry_attempts + 1` attempts per invocation, with exponential
//!   backoff and per-process jitter; anything else fails the record
//!   immediately.
//! - **Graceful shutdown:** a [`ShutdownSignal`] trips a `watch` channel
//!   the stream races against; shutdown persists confirmed progress,
//!   parks the record `paused` (finalization is atomic and always
//!   completes), and reports cancellation. Shutdown that lands between
//!   phases is honored at the next check; a metadata exchange in flight
//!   finishes or hits its own bounded timeout first.
//!
//! Statuses move only along the ADR-0018 graph; `update_status` rejects
//! anything else, so the walking helpers here are structuring, not
//! permission slips. Statuses write failures during failure handling are
//! best-effort: the record stays recoverable, which is the point.

#![allow(dead_code)] // commands arrive in Phase 11; narrow waiver per AGENTS.md

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::{Method, StatusCode};
use tokio::sync::watch;

use crate::download::http::{http_status_error, HttpClient, HttpFailure};
use crate::download::probe::{
    parse_content_range, probe, response_meta, validate_url, ContentEncoding, ContentLength,
    ProbeResult, RangeSupport,
};
use crate::download::progress::Progress;
use crate::download::retry::{classify, RetryPolicy, RetryVerdict};
use crate::download::segmented::{
    download_segments, PlannedSegment, SegmentBatchEnd, SegmentContext, SegmentSpan,
    SegmentationPolicy,
};
use crate::download::sequential::{
    expected_size_of, select_filename, with_transfer_context, SequentialDownload,
};
use crate::error::{AppError, ErrorCategory};
use crate::logging::redact_url;
use crate::storage::file_writer::{finalize, reserve_destination, PartialFile};
use crate::storage::repository::{
    DownloadRecord, DownloadState, NewDownload, NewSegment, Repository, SegmentRecord,
    SegmentState, TransferFacts,
};

/// Statuses from which a record can still be driven to completion.
///
/// Public within the crate because the Phase 7 batch scheduler decides
/// identically which of a scope's records are unfinished when it adopts a
/// submitted URL instead of duplicating it (`src/app/batch.rs`); the two
/// definitions must stay one.
pub(crate) const RECOVERABLE: [DownloadState; 6] = [
    DownloadState::Queued,
    DownloadState::Probing,
    DownloadState::Downloading,
    DownloadState::Paused,
    DownloadState::Retrying,
    DownloadState::Finalizing,
];

/// Confirmed-progress persistence interval for a streamed transfer. A
/// transfer shorter than this confirms once, at its end.
const PROGRESS_CONFIRM_INTERVAL_BYTES: u64 = 1_048_576;

/// Graceful-shutdown wiring shared between the engine and its caller
/// (Phase 6: Ctrl+C must "stop new work, cancel active I/O as safely as
/// possible, preserve trustworthy metadata, and leave transfers resumable").
///
/// The sender half belongs to the termination wiring (its command-side
/// caller arrives in Phase 11); the receiver half is raced against every
/// network read below and checked between chunk writes. The channel is
/// `bool` because the only question is "has shutdown been requested".
#[derive(Debug)]
pub(crate) struct ShutdownSignal {
    tx: watch::Sender<bool>,
}

impl ShutdownSignal {
    /// Creates the signal/handle pair.
    pub(crate) fn channel() -> (ShutdownSignal, ShutdownHandle) {
        let (tx, rx) = watch::channel(false);
        (ShutdownSignal { tx }, ShutdownHandle { rx })
    }

    /// Trips the shutdown: every engine checkpoint after this stops new
    /// work. Tripping twice is permitted and harmless.
    pub(crate) fn trip(&self) {
        let _ = self.tx.send(true);
    }
}

/// The engine-side half of a [`ShutdownSignal`].
///
/// Cloned wherever a task needs to observe the signal; clones share one
/// `watch` channel, so every clone sees the trip exactly once.
#[derive(Clone, Debug)]
pub(crate) struct ShutdownHandle {
    rx: watch::Receiver<bool>,
}

impl ShutdownHandle {
    /// A synchronous check used between chunk reads and writes.
    pub(crate) fn tripped(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves when the shutdown has been tripped. When the sender is gone
    /// without ever tripping (nobody can trip it anymore), waiting ends:
    /// the signal's owner controls the transfer's lifetime.
    pub(crate) async fn wait(&self) {
        let mut rx = self.rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// What one attempt of [`transfer_persisted`] produced.
enum AttemptOutcome {
    /// The transfer finished; the file exists at the returned path.
    Completed(SequentialDownload),
    /// Shutdown was honored mid-stream: confirmed progress and the `paused`
    /// status already persisted, nothing left to undo.
    Interrupted,
}

/// The recovery verdict for one URL + output-directory pair, derived only
/// from the database and the disk, before any network work.
#[derive(Debug)]
pub(crate) enum RecoveryPlan {
    /// No record (or none with trusted progress): fetch from zero. When a
    /// record exists its statuses walk forward; when none does, one is
    /// inserted after the first probe succeeds.
    Fresh {
        record: Option<DownloadRecord>,
        partial: Option<PathBuf>,
    },
    /// The record is complete and its file is really there: report it,
    /// touch nothing, send nothing (idempotent recovery).
    AlreadyComplete { record: DownloadRecord },
    /// The partial file already holds every expected byte *and* the
    /// database confirmed exactly that: finalize locally, no network. This
    /// is the crash-between-last-write-and-finalization case.
    FinalizeAvailable {
        record: DownloadRecord,
        segment: SegmentRecord,
        partial: PathBuf,
    },
    /// The rename already happened while the record was `finalizing` and
    /// only the completed status never landed: close the record out.
    CompleteInPlace { record: DownloadRecord },
    /// Continue at `offset` by appending the revalidated remainder.
    Resume {
        record: DownloadRecord,
        segment: SegmentRecord,
        partial: PathBuf,
        offset: u64,
    },
}

/// What identity revalidation concluded for a record's stored validators.
#[derive(Debug)]
enum Identity {
    /// A comparable validator pair matched; the stored value doubles as
    /// `If-Range` and the continuation only needs the response to not
    /// contradict it.
    Verified(String),
    /// The probe did not echo the stored validator (no comparable pair):
    /// the ranged answer itself must carry a comparable validator, or the
    /// continuation downgrades to a restart.
    Tentative(String),
    /// Comparable evidence contradicted the record.
    Changed(&'static str),
    /// The record stores no validator at all (defensive; the plan refuses
    /// to build a `Resume` for validatorless records).
    Unverifiable,
}

impl Identity {
    /// The `If-Range` value when one is sendable.
    fn if_range(&self) -> Option<&str> {
        match self {
            Identity::Verified(value) | Identity::Tentative(value) => Some(value),
            Identity::Changed(_) | Identity::Unverifiable => None,
        }
    }
}

/// Compares the record's stored validators against the probe's fresh ones.
///
/// The policy (ADR-0019): identity verifies through *comparable* pairs
/// only — an ETag matches an ETag, a `Last-Modified` matches a
/// `Last-Modified` — and the effective URL must be the one already
/// recorded (a redirect resolving elsewhere restarts the object's
/// identity). An ETag is the preferred `If-Range` carrier.
fn revalidate_identity(record: &DownloadRecord, facts: &ProbeResult) -> Identity {
    if let Some(recorded) = record.effective_url.as_deref() {
        if recorded != facts.effective_url.as_str() {
            return Identity::Changed("the redirect chain resolved to a different canonical URL");
        }
    }
    let stored_etag = record.etag.as_deref();
    let stored_modified = record.last_modified.as_deref();
    if let (Some(stored), Some(learned)) = (stored_etag, facts.etag.as_deref()) {
        if stored == learned {
            return Identity::Verified(stored.to_owned());
        }
        return Identity::Changed("the representation's ETag is no longer the recorded one");
    }
    if let (Some(stored), Some(learned)) = (stored_modified, facts.last_modified.as_deref()) {
        if stored == learned {
            // The ETag is still the better `If-Range` carrier when both are
            // stored and the last-modified pair happens to verify first.
            return Identity::Verified(stored_etag.unwrap_or(stored).to_owned());
        }
        return Identity::Changed(
            "the representation's Last-Modified is no longer the recorded one",
        );
    }
    if let Some(stored) = stored_etag.or(stored_modified) {
        return Identity::Tentative(stored.to_owned());
    }
    Identity::Unverifiable
}

/// Whether the ranged answer itself carries the representation's identity.
///
/// The mock ignores `If-Range`, so nothing but the answer's own validators
/// can be trusted as identity evidence here. Equal pairs pass; either one
/// contradicting restarts. A `206` with no validator at all is accepted
/// only when the probe itself already verified identity (a compliant
/// server that honored `If-Range` may answer validatorless in theory).
fn response_verifies(
    record: &DownloadRecord,
    meta: &crate::download::probe::ResponseMeta,
    identity: &Identity,
) -> Result<(), &'static str> {
    let etag_pair = record.etag.as_deref().zip(meta.etag.as_deref());
    let modified_pair = record
        .last_modified
        .as_deref()
        .zip(meta.last_modified.as_deref());
    if let Some((stored, answered)) = etag_pair {
        if stored != answered {
            return Err("the representation's ETag is no longer the recorded one");
        }
    }
    if let Some((stored, answered)) = modified_pair {
        if stored != answered {
            return Err("the representation's Last-Modified is no longer the recorded one");
        }
    }
    if etag_pair.is_none() && modified_pair.is_none() && matches!(identity, Identity::Tentative(_))
    {
        return Err(
            "the ranged answer carries none of the representation's validators, so its \
             identity cannot be confirmed",
        );
    }
    Ok(())
}

/// Everything one attempt needs.
struct Attempt<'a> {
    client: &'a HttpClient,
    repo: &'a Repository,
    url: &'a str,
    output_dir: &'a Path,
    progress: Arc<Progress>,
    shutdown: &'a ShutdownHandle,
    segmentation: &'a SegmentationPolicy,
    now: fn() -> u64,
}

/// Runs one attempt of the plan; the classification-worthy error (if any)
/// is returned for the caller's retry machinery.
async fn run_attempt(ctx: &Attempt<'_>, plan: RecoveryPlan) -> Result<AttemptOutcome, AppError> {
    match plan {
        RecoveryPlan::AlreadyComplete { record } => {
            let bytes_written = std::fs::metadata(&record.destination)
                .map(|meta| meta.len())
                .map_err(|err| {
                    AppError::new(
                        ErrorCategory::Filesystem,
                        format!(
                            "the completed file {} could not be measured: {err}",
                            record.destination.display()
                        ),
                    )
                    .with_source(err)
                })?;
            Ok(AttemptOutcome::Completed(SequentialDownload {
                destination: record.destination,
                file_name: record.file_name,
                bytes_written,
                expected_size: record.expected_size,
                etag: record.etag,
                last_modified: record.last_modified,
                effective_url: record.effective_url.unwrap_or(record.url),
            }))
        }
        RecoveryPlan::CompleteInPlace { record } => {
            ctx.repo
                .update_status(record.id, DownloadState::Completed, (ctx.now)(), None)?;
            let bytes_written = std::fs::metadata(&record.destination)
                .map(|meta| meta.len())
                .map_err(|err| {
                    AppError::new(
                        ErrorCategory::Filesystem,
                        format!(
                            "the finalized file {} could not be measured: {err}",
                            record.destination.display()
                        ),
                    )
                    .with_source(err)
                })?;
            tracing::info!(
                "download record {}: the finalized file is already in place; the record \
                 closes as completed without network work",
                record.id
            );
            Ok(AttemptOutcome::Completed(SequentialDownload {
                destination: record.destination,
                file_name: record.file_name,
                bytes_written,
                expected_size: record.expected_size,
                etag: record.etag,
                last_modified: record.last_modified,
                effective_url: record.effective_url.unwrap_or(record.url),
            }))
        }
        RecoveryPlan::FinalizeAvailable {
            record,
            segment,
            partial,
        } => {
            // A `finalizing` record skips the walk entirely (its rename may
            // already have happened); every other state walks first.
            if record.status != DownloadState::Finalizing {
                walk_to_downloading(ctx.repo, record.id, record.status, ctx.now)?;
            }
            finalize_available(ctx, record, segment, partial).await
        }
        RecoveryPlan::Fresh {
            record: None,
            partial: _,
        } => {
            // The insert-only-after-proof contract: the first probe precedes
            // any record. A transfer that dies during its own first probe
            // leaves no record at all (disclosed in ADR-0019).
            let facts = probe(ctx.client, ctx.url).await?;
            let effective_url = validate_url(&facts.effective_url)?;
            let file_name = select_filename(&effective_url);
            let redacted = redact_url(&facts.effective_url);
            tracing::info!(
                "sequential transfer of {}: probed size {}",
                redacted,
                facts
                    .expected_size
                    .map_or_else(|| "unknown".to_owned(), |size| size.to_string()),
            );
            tracing::debug!("destination file name: {file_name}");
            let planned = ctx.output_dir.join(&file_name);
            let record_id = ctx.repo.insert_download(
                NewDownload {
                    url: ctx.url.to_owned(),
                    destination: planned,
                    file_name: file_name.clone(),
                },
                (ctx.now)(),
            )?;
            ctx.repo
                .update_transfer_facts(record_id, &probe_facts(&facts), (ctx.now)())?;
            ctx.repo
                .update_status(record_id, DownloadState::Probing, (ctx.now)(), None)?;
            walk_to_downloading(ctx.repo, record_id, DownloadState::Probing, ctx.now)?;
            full_transfer(
                ctx,
                record_id,
                file_name,
                effective_url,
                redacted,
                &facts,
                None,
                None,
            )
            .await
        }
        RecoveryPlan::Fresh {
            record: Some(record),
            partial,
        } => {
            // A restart has nothing to protect: no validator is sent, the
            // current remote is the truth, and the statuses walk first so
            // every failure below reports from `downloading`.
            //
            // A *pristine* record (Phase 7: queued at acceptance, never
            // probed) is the one exception to "the recorded name stands":
            // it exists before any probe could learn the redirect target,
            // so its planned name comes from the requested URL; the first
            // probe refreshes name and destination from the effective URL
            // (ADR-0016 naming, ADR-0020 pristine refresh).
            let pristine = is_pristine(&record, partial.as_deref());
            walk_to_downloading(ctx.repo, record.id, record.status, ctx.now)?;
            let facts = probe(ctx.client, ctx.url).await?;
            let effective_url = validate_url(&facts.effective_url)?;
            let file_name = if pristine {
                let name = select_filename(&effective_url);
                let planned = ctx.output_dir.join(&name);
                if name != record.file_name || planned != record.destination {
                    tracing::debug!(
                        "download record {}: the pristine planned name ({}) is refreshed \
                         to the effective URL's name ({name})",
                        record.id,
                        record.file_name,
                    );
                    ctx.repo
                        .set_planned_name(record.id, &name, &planned, (ctx.now)())?;
                }
                name
            } else {
                record.file_name
            };
            ctx.repo
                .update_transfer_facts(record.id, &probe_facts(&facts), (ctx.now)())?;
            let redacted = redact_url(&facts.effective_url);
            full_transfer(
                ctx,
                record.id,
                file_name,
                effective_url,
                redacted,
                &facts,
                partial,
                None,
            )
            .await
        }
        RecoveryPlan::Resume {
            record,
            segment,
            partial,
            offset,
        } => resume_attempt(ctx, record, segment, partial, offset).await,
    }
}

/// Drives one URL into one output directory, honoring the persisted
/// recovery contract; returns when the transfer completed, failed
/// terminally, or shutdown was requested.
///
/// The caller owns shutdown (`shutdown`), the retry budget (`policy`), and
/// the segmentation limits (`segmentation`); the progress handle exists for
/// diagnostics and tests. The nine parameters are the deliberate contract
/// of the engine entry point (the Phase 11 command wiring is its only
/// caller).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn transfer_persisted(
    client: &HttpClient,
    repo: &Repository,
    url: &str,
    output_dir: &Path,
    progress: Arc<Progress>,
    policy: &RetryPolicy,
    segmentation: &SegmentationPolicy,
    now: fn() -> u64,
    shutdown: &ShutdownHandle,
) -> Result<SequentialDownload, AppError> {
    let ctx = Attempt {
        client,
        repo,
        url,
        output_dir,
        progress,
        shutdown,
        segmentation,
        now,
    };
    let mut transient_failures: u32 = 0;
    let shutdown_message = format!(
        "shutdown was requested during the transfer of {}; the persisted download is \
         paused and resumable",
        redact_url(url),
    );
    loop {
        if ctx.shutdown.tripped() {
            return Err(AppError::new(
                ErrorCategory::Cancelled,
                shutdown_message.clone(),
            ));
        }
        let plan = plan_for(ctx.repo, url, output_dir)?;
        let outcome = match run_attempt(&ctx, plan).await {
            // Finished: nothing to classify.
            Ok(AttemptOutcome::Completed(download)) => return Ok(download),
            Ok(AttemptOutcome::Interrupted) => {
                return Err(AppError::new(
                    ErrorCategory::Cancelled,
                    shutdown_message.clone(),
                ))
            }
            Err(err) => err,
        };
        match classify(&outcome) {
            RetryVerdict::Permanent => {
                if let Some(record) = current_record(ctx.repo, url, output_dir)? {
                    let failure = mark_failed_record(ctx.repo, record.id, &outcome, ctx.now, None);
                    warn_write(&failure, record.id, "the failed status");
                }
                return Err(outcome);
            }
            RetryVerdict::Transient => {
                if transient_failures >= policy.max_retries {
                    if let Some(record) = current_record(ctx.repo, url, output_dir)? {
                        let failure = mark_failed_record(
                            ctx.repo,
                            record.id,
                            &outcome,
                            ctx.now,
                            Some(ErrorCategory::RetryExhausted),
                        );
                        warn_write(&failure, record.id, "the failed status");
                    }
                    return Err(exhaustion_error(outcome, policy.attempt_budget(), url));
                }
                transient_failures += 1;
                let record = current_record(ctx.repo, url, output_dir)?;
                if let Some(record) = record {
                    let persistence = mark_retrying(
                        ctx.repo,
                        record.id,
                        record.status,
                        record.retry_count + 1,
                        ctx.now,
                    );
                    warn_write(&persistence, record.id, "the retrying reservation");
                }
                let attempt = transient_failures - 1;
                tracing::info!(
                    "transient failure during the transfer of {} (attempt {} of a {}-attempt \
                     budget); retrying after backoff: {}",
                    redact_url(url),
                    attempt + 1,
                    policy.attempt_budget(),
                    // Error messages are redact-safe by construction (every
                    // diagnostic site formats through `redact_url`).
                    outcome.message(),
                );
                let delay = policy.delay_for(attempt);
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = ctx.shutdown.wait() => {
                        if let Some(record) = current_record(ctx.repo, url, output_dir)? {
                            let parking =
                                park_retrying(ctx.repo, record.id, record.status, ctx.now);
                            warn_write(&parking, record.id, "the paused status");
                        }
                        return Err(AppError::new(
                            ErrorCategory::Cancelled,
                            format!(
                                "shutdown was requested during the retry backoff for {}; the \
                                 persisted download is parked and resumable",
                                redact_url(url),
                            ),
                        ));
                    }
                }
            }
        }
    }
}

/// The typed exhaustion error after the budget ran out; the hidden source
/// keeps the last failure's whole chain for further diagnosis.
fn exhaustion_error(last: AppError, attempts: u32, url: &str) -> AppError {
    let message = format!(
        "the transfer of {} could not finish within {} attempts; the last failure was: {}",
        redact_url(url),
        attempts,
        last.message(),
    );
    AppError::new(ErrorCategory::RetryExhausted, message).with_source(last)
}

/// The one recoverable record of this invocation, if any. Multiple would
/// be ambiguous (and are refused by `plan_for` on the next loop); zero
/// records belong to attempts that fail before their insertion.
fn current_record(
    repo: &Repository,
    url: &str,
    output_dir: &Path,
) -> Result<Option<DownloadRecord>, AppError> {
    Ok(repo.list_downloads()?.into_iter().find(|record| {
        record.url == url
            && record
                .destination
                .parent()
                .is_some_and(|parent| parent == output_dir)
            && RECOVERABLE.contains(&record.status)
    }))
}

/// Best-effort extra bookkeeping never masks a transfer's own error.
fn warn_write(result: &Result<(), AppError>, record_id: i64, what: &str) {
    if let Err(err) = result {
        tracing::warn!(
            "the download record {record_id} could not record {what}: {}",
            err.message()
        );
    }
}

/// Marks a record in a workable state as `failed` with the error category
/// (`exhaustion` overrides it additionally).
fn mark_failed_record(
    repo: &Repository,
    record_id: i64,
    err: &AppError,
    now: fn() -> u64,
    category: Option<ErrorCategory>,
) -> Result<(), AppError> {
    let Some(record) = repo.get_download(record_id)? else {
        return Ok(());
    };
    let category = category.unwrap_or_else(|| err.category());
    match record.status {
        DownloadState::Probing
        | DownloadState::Downloading
        | DownloadState::Retrying
        | DownloadState::Finalizing => {
            repo.update_status(record_id, DownloadState::Failed, now(), Some(category))
        }
        other => {
            tracing::warn!(
                "the download record {record_id} is in the {} state; a failure cannot \
                 be recorded from there, so the record stays recoverable",
                other.as_str(),
            );
            Ok(())
        }
    }
}

/// Reserves a transient failure: the record moves into `retrying` when it
/// was streaming-shaped, and the cumulative retry count grows. States
/// without a `retrying` edge (e.g. `probing` for pre-stream failures) are
/// left for the next attempt to walk.
fn mark_retrying(
    repo: &Repository,
    record_id: i64,
    status: DownloadState,
    retry_count: u64,
    now: fn() -> u64,
) -> Result<(), AppError> {
    if status == DownloadState::Downloading {
        repo.update_status(record_id, DownloadState::Retrying, now(), None)?;
    }
    repo.set_retry_count(record_id, retry_count, now())
}

/// On shutdown during the backoff: park a `retrying` record (which holds
/// the legal edge) and leave anything else as-is.
fn park_retrying(
    repo: &Repository,
    record_id: i64,
    status: DownloadState,
    now: fn() -> u64,
) -> Result<(), AppError> {
    if status == DownloadState::Retrying {
        return repo.update_status(record_id, DownloadState::Paused, now(), None);
    }
    Ok(())
}

/// The refusal for one scope claimed by several unfinished records: the
/// engine plans against exactly one, and the Phase 7 batch scheduler
/// rejects the conflicting URL with this same message instead of guessing.
///
/// The message names the URL raw (it is the caller's own argument, shown
/// on the user's terminal, never sent to tracing — log sites redact).
pub(crate) fn unresolved_multiplicity_error(
    record_ids: &[i64],
    url: &str,
    output_dir: &Path,
) -> AppError {
    let ids = record_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    AppError::new(
        ErrorCategory::Input,
        format!(
            "the database holds {} unfinished downloads for {url} in {}: records \
             ({ids}) cannot share one destination plan; resolve the multiplicity \
             before the transfer",
            record_ids.len(),
            output_dir.display(),
        ),
    )
}

/// Decides what can be safely done with the passed URL + directory from
/// the database and the disk alone.
///
/// Scope is exact: the record's URL string and the record's planned
/// destination directory must match the invocation's arguments
/// text-exactly (the same directory value recurs across invocations;
/// normalization is a later phase's concern, disclosed in ADR-0019).
pub(crate) fn plan_for(
    repo: &Repository,
    url: &str,
    output_dir: &Path,
) -> Result<RecoveryPlan, AppError> {
    let in_scope: Vec<DownloadRecord> = repo
        .list_downloads()?
        .into_iter()
        .filter(|record| record.url == url)
        .filter(|record| {
            record
                .destination
                .parent()
                .is_some_and(|parent| parent == output_dir)
        })
        .collect();

    let unfinished: Vec<DownloadRecord> = in_scope
        .iter()
        .filter(|record| RECOVERABLE.contains(&record.status))
        .cloned()
        .collect();
    match unfinished.as_slice() {
        [] => {}
        [record] => return reconcile_download(repo, record.clone()),
        many => {
            let ids = many.iter().map(|record| record.id).collect::<Vec<_>>();
            return Err(unresolved_multiplicity_error(&ids, url, output_dir));
        }
    }

    // No unfinished record: an already-completed record in scope makes the
    // invocation an idempotent no-op (when the file is really there).
    if let Some(completed) = in_scope
        .iter()
        .find(|record| record.status == DownloadState::Completed)
    {
        if std::fs::metadata(&completed.destination).is_ok_and(|meta| meta.is_file()) {
            return Ok(RecoveryPlan::AlreadyComplete {
                record: completed.clone(),
            });
        }
        return Err(AppError::new(
            ErrorCategory::Database,
            format!(
                "the download record {} says {} was completed, but the file is missing; a \
                 completed download never re-runs (the lifecycle graph has no such \
                 transition), so remove the record or start the URL again as a new transfer",
                completed.id,
                completed.destination.display(),
            ),
        ));
    }

    // Terminal records (failed/cancelled) are history for *their* record:
    // this invocation is a new attempt and creates its own record.
    Ok(RecoveryPlan::Fresh {
        record: None,
        partial: None,
    })
}

/// Reconciles one unfinished record against the disk (the Phase 6
/// "startup reconciliation" of unfinished records before any resume).
fn reconcile_download(repo: &Repository, record: DownloadRecord) -> Result<RecoveryPlan, AppError> {
    match record.status {
        DownloadState::Failed | DownloadState::Cancelled => {
            return Err(AppError::new(
                ErrorCategory::Input,
                format!(
                    "the download record {} is in the terminal {} state and cannot be \
                     resumed; the retry flow owns records like this",
                    record.id,
                    record.status.as_str()
                ),
            ));
        }
        DownloadState::Extracting => {
            return Err(AppError::new(
                ErrorCategory::Input,
                format!(
                    "the download record {} is extracting; resume is not defined while \
                     extraction owns the archive",
                    record.id
                ),
            ));
        }
        DownloadState::Completed => {
            return Err(AppError::new(
                ErrorCategory::Input,
                format!(
                    "the download record {} is completed and cannot be resumed",
                    record.id
                ),
            ));
        }
        DownloadState::Queued
        | DownloadState::Probing
        | DownloadState::Downloading
        | DownloadState::Paused
        | DownloadState::Retrying
        | DownloadState::Finalizing => {}
    }

    let Some(dir) = record
        .destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(AppError::new(
            ErrorCategory::Input,
            format!(
                "the download record {} has no directory in its destination ({})",
                record.id,
                record.destination.display()
            ),
        ));
    };

    let candidates = scan_partials(dir, &record.file_name)?;
    // The list is ordered newest-first; only the newest may be appended to
    // (older sequences belong to dead processes alone).
    let Some(partial_path) = candidates
        .into_iter()
        .next()
        .map(|candidate| candidate.path)
    else {
        // No engine-owned partial left. For a `finalizing` record this
        // usually means the rename already ran; close the record out when
        // the file at the planned destination checks out against the
        // record's size claim, otherwise restart.
        if record.status == DownloadState::Finalizing {
            let file_len = std::fs::metadata(&record.destination)
                .map(|meta| meta.len())
                .ok();
            let matches = record
                .expected_size
                .map_or(file_len.is_some(), |expected| file_len == Some(expected));
            if matches {
                return Ok(RecoveryPlan::CompleteInPlace { record });
            }
        }
        tracing::info!(
            "download record {}: the engine's partial file no longer exists; the transfer \
             restarts from zero",
            record.id
        );
        return Ok(RecoveryPlan::Fresh {
            record: Some(record),
            partial: None,
        });
    };

    let file_len = std::fs::metadata(&partial_path)
        .map(|meta| meta.len())
        .map_err(|err| {
            AppError::new(
                ErrorCategory::Filesystem,
                format!(
                    "the partial file {} could not be measured during reconciliation: {err}",
                    partial_path.display()
                ),
            )
            .with_source(err)
        })?;

    // The newest viable whole-range row: `Failed` marks rows retired by
    // earlier restarts whose span stopped fitting.
    let rows = repo.list_segments(record.id)?;
    let mut segment: Option<SegmentRecord> = None;
    for row in rows.iter().rev() {
        if row.min_byte == 0 && row.state != SegmentState::Failed {
            segment = Some(row.clone());
            break;
        }
    }
    let Some(segment) = segment else {
        // No size fact was ever confirmed for this transfer (e.g. an
        // unknown-length stream): file bytes cannot be anchored to a
        // validated length, so appending is refused; restart instead.
        tracing::info!(
            "download record {}: no confirmed byte-range progress exists; the transfer \
             restarts from zero",
            record.id
        );
        return Ok(RecoveryPlan::Fresh {
            record: Some(record),
            partial: Some(partial_path),
        });
    };

    let confirmed = segment.received_bytes;
    // The crash-between-last-write-and-finalization case: everything that
    // was supposed to exist does, both on disk and in the database.
    let complete_on_disk = record
        .expected_size
        .is_some_and(|expected| file_len == expected && confirmed == expected);
    if complete_on_disk {
        return Ok(RecoveryPlan::FinalizeAvailable {
            record,
            segment,
            partial: partial_path,
        });
    }

    // A file longer than the record's expected size cannot belong to the
    // validated resource: restart instead of appending.
    if record
        .expected_size
        .is_some_and(|expected| file_len > expected)
    {
        tracing::info!(
            "download record {}: the partial file is larger than the expected size; the \
             transfer restarts from zero",
            record.id
        );
        return Ok(RecoveryPlan::Fresh {
            record: Some(record),
            partial: Some(partial_path),
        });
    }

    // The trust rule (ADR-0019): confirmed bytes must exist on disk. When
    // the file is shorter, the record is deconfirmed *now*, so no later
    // attempt ever appends onto a claim the file cannot back.
    let offset = confirmed.min(file_len);
    if offset < confirmed {
        repo.update_segment(segment.id, segment.state, offset)?;
    }
    if offset == 0 {
        return Ok(RecoveryPlan::Fresh {
            record: Some(record),
            partial: Some(partial_path),
        });
    }

    Ok(RecoveryPlan::Resume {
        record,
        segment,
        partial: partial_path,
        offset,
    })
}

/// Finds the engine-owned partial files of one file name next to the final
/// file (`<name>.swiftfetch-<pid>-<seq>.part`), ordered newest first
/// (highest sequence, then pid).
fn scan_partials(dir: &Path, file_name: &str) -> Result<Vec<PartialCandidate>, AppError> {
    let prefix = format!("{file_name}.swiftfetch-");
    let entries = std::fs::read_dir(dir).map_err(|err| {
        AppError::new(
            ErrorCategory::Filesystem,
            format!(
                "the output directory {} could not be read to look for previous transfers' \
                 partial files: {err}",
                dir.display()
            ),
        )
        .with_source(err)
    })?;
    let mut found: Vec<PartialCandidate> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            AppError::new(
                ErrorCategory::Filesystem,
                format!(
                    "the output directory {} could not be read to look for previous \
                     transfers' partial files: {err}",
                    dir.display()
                ),
            )
            .with_source(err)
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(stem) = rest.strip_suffix(".part") else {
            continue;
        };
        let Some((pid_text, seq_text)) = stem.rsplit_once('-') else {
            continue;
        };
        let (Ok(pid), Ok(seq)) = (pid_text.parse::<u32>(), seq_text.parse::<u64>()) else {
            // A file that merely looks like a partial is skipped, never
            // trusted and never deleted (the failure law preserves partial
            // data for diagnostics).
            continue;
        };
        found.push(PartialCandidate {
            path: dir.join(&name),
            seq,
            pid,
        });
    }
    // Newest first: highest sequence, then pid (a later sequence means the
    // same process advanced; a higher pid means a later process replaced it).
    found.sort_by_key(|candidate| std::cmp::Reverse((candidate.seq, candidate.pid)));
    Ok(found)
}

/// One discovered partial file with its parseable identity.
#[derive(Debug)]
struct PartialCandidate {
    path: PathBuf,
    seq: u64,
    pid: u32,
}

/// Walks the record into `downloading` along whatever legal predecessor
/// edges it currently sits on (`downloading` itself is a no-op).
fn walk_to_downloading(
    repo: &Repository,
    record_id: i64,
    current: DownloadState,
    now: fn() -> u64,
) -> Result<(), AppError> {
    match current {
        DownloadState::Downloading => Ok(()),
        DownloadState::Queued => {
            repo.update_status(record_id, DownloadState::Probing, now(), None)?;
            repo.update_status(record_id, DownloadState::Downloading, now(), None)
        }
        DownloadState::Probing | DownloadState::Paused | DownloadState::Retrying => {
            repo.update_status(record_id, DownloadState::Downloading, now(), None)
        }
        other => Err(AppError::new(
            ErrorCategory::Input,
            format!(
                "the download record {record_id} is in the {} state, which cannot enter a \
                 transfer; the lifecycle graph has no transition from here",
                other.as_str()
            ),
        )),
    }
}

/// The facts of a probe, shaped for the repository's merge.
fn probe_facts(facts: &ProbeResult) -> TransferFacts {
    TransferFacts {
        effective_url: Some(facts.effective_url.clone()),
        expected_size: facts.expected_size,
        etag: facts.etag.clone(),
        last_modified: facts.last_modified.clone(),
    }
}

/// Whether a record has never been probed and owns nothing on disk — the
/// only state in which its planned name is still provisional.
///
/// Phase 7 queues records at acceptance time, before any probe can learn
/// the redirect target, so such a record's name derives from the requested
/// URL. Facts or an engine-owned partial file mean an earlier attempt made
/// the name real (the partial file is named after it), and then the name
/// must never change under the same record (`DOCS/decision.md` ADR-0020).
fn is_pristine(record: &DownloadRecord, partial: Option<&Path>) -> bool {
    partial.is_none()
        && record.effective_url.is_none()
        && record.expected_size.is_none()
        && record.etag.is_none()
        && record.last_modified.is_none()
}

/// The plain (unranged) fetch path: validate a full-content response, then
/// stream and finalize, reporting byte-anchored failures for classification.
///
/// `response_in_hand` carries the rare case where a ranged attempt received
/// a legal full `200`: that response *is* the fresh content (bytes are
/// never wasted).
#[allow(clippy::too_many_arguments)] // the fetch is parameterized, not accidental
async fn full_transfer(
    ctx: &Attempt<'_>,
    record_id: i64,
    file_name: String,
    effective_url: reqwest::Url,
    redacted: String,
    facts: &ProbeResult,
    partial_path: Option<PathBuf>,
    response_in_hand: Option<reqwest::Response>,
) -> Result<AttemptOutcome, AppError> {
    let mut partial_path = partial_path;
    // Phase 8: segmentation is an opportunistic strategy, chosen from the
    // probe's proven facts before a full-body GET is wasted. A response
    // already in hand (a ranged restart that received a legal full 200) is
    // always streamed sequentially: its bytes are fresh content.
    if response_in_hand.is_none() {
        if let Some((total, spans)) = ctx.segmentation.plan_for_probe(facts) {
            tracing::info!(
                "transfer of {}: the probe proved byte ranges and a known {total}-byte \
                 size; fetching {} concurrent segments",
                redacted,
                spans.len()
            );
            let temp = match partial_path.take() {
                Some(path) => PartialFile::resume_at(path, 0).await?,
                None => PartialFile::create(ctx.output_dir, &file_name).await?,
            };
            let segmented_path = temp.path().to_path_buf();
            match segmented_transfer(
                ctx,
                record_id,
                &file_name,
                &effective_url,
                &redacted,
                facts,
                total,
                spans,
                temp,
            )
            .await?
            {
                SegmentedOutcome::Completed(download) => {
                    return Ok(AttemptOutcome::Completed(download))
                }
                SegmentedOutcome::Interrupted => return Ok(AttemptOutcome::Interrupted),
                SegmentedOutcome::Fallback { reason } => {
                    // The aborted plan's rows are retired and its partial is
                    // truncated inside the segmented attempt; the same
                    // attempt continues with a clean sequential fetch.
                    tracing::info!(
                        "the segmented transfer of {} is unsafe and falls back to a \
                         sequential transfer: {reason}",
                        redacted
                    );
                    partial_path = Some(segmented_path);
                }
            }
        }
    }
    let mut response = match response_in_hand {
        Some(response) => response,
        None => {
            let response = ctx.client.send(Method::GET, &effective_url, None).await?;
            let status = response.status();
            if status != StatusCode::OK {
                return Err(http_status_error(
                    status,
                    format!("the remote server responded HTTP {status} for {redacted}"),
                ));
            }
            response
        }
    };
    // One header parse, one trust policy: reuse the probe's strict parser
    // for this transfer's own response (Phase 4's discipline).
    let meta = response_meta(&response);
    if meta.encoding == ContentEncoding::Transform {
        return Err(AppError::new(
            ErrorCategory::RangeValidation,
            format!(
                "the transfer of {redacted} declared a content transformation \
                 (Content-Encoding), which cannot be written as the resource's bytes; \
                 the transfer is refused"
            ),
        ));
    }
    let expected = expected_size_of(meta.cl, facts.expected_size, &redacted)?;

    // Only after the response proved to be content does anything exist on
    // disk; a restart reuses its own partial truncated to zero.
    let temp = match partial_path {
        Some(path) => PartialFile::resume_at(path, 0).await?,
        None => PartialFile::create(ctx.output_dir, &file_name).await?,
    };
    let segment = ensure_segment(ctx.repo, record_id, expected, 0)?;
    ctx.progress.continue_from(0);

    let setup = StreamSetup {
        redacted,
        expected,
        segment,
        confirmed: 0,
    };
    match stream_transfer(ctx, &mut response, temp, setup, record_id).await? {
        (StreamEnd::Interrupted, _) => Ok(AttemptOutcome::Interrupted),
        (StreamEnd::Completed { total }, Some(temp)) => {
            let moved = finalize_claimed(
                ctx,
                record_id,
                DownloadState::Downloading,
                temp,
                &file_name,
                &ctx.output_dir.join(&file_name),
            )
            .await?;
            Ok(AttemptOutcome::Completed(SequentialDownload {
                destination: moved,
                file_name,
                bytes_written: total,
                expected_size: expected,
                etag: meta.etag.clone().or(facts.etag.clone()),
                last_modified: meta.last_modified.clone().or(facts.last_modified.clone()),
                effective_url: facts.effective_url.clone(),
            }))
        }
        (StreamEnd::Completed { .. }, None) => {
            unreachable!("a completed stream hands its partial back for the rename")
        }
    }
}

/// How one segmented attempt ended before finalization.
enum SegmentedOutcome {
    /// Every span was written, synced, and recorded; the file was finalized.
    Completed(SequentialDownload),
    /// Shutdown parked the transfer with its confirmed segment rows intact.
    Interrupted,
    /// Range behavior was unsafe; the caller must continue sequentially in
    /// the same attempt (the plan's rows are retired and its bytes were
    /// discarded before this value is returned).
    Fallback { reason: &'static str },
}

/// Runs one planned segmented transfer over an already-created (or
/// truncated) partial file.
///
/// The plan's rows are inserted `Pending` before any worker starts, workers
/// own their spans, and every ending is honest about what the store and the
/// partial may claim:
///
/// - completion finalizes only after every worker's row is `Completed`;
/// - shutdown parks the record `paused` with the workers' confirmed rows;
/// - a fallback or a worker error retires the rows and truncates the
///   partial to zero before returning, so the sequential fallback (or a
///   retry) never appends to an aborted plan's output.
#[allow(clippy::too_many_arguments)] // the attempt is parameterized, not accidental
async fn segmented_transfer(
    ctx: &Attempt<'_>,
    record_id: i64,
    file_name: &str,
    effective_url: &reqwest::Url,
    redacted: &str,
    facts: &ProbeResult,
    total: u64,
    spans: Vec<SegmentSpan>,
    temp: PartialFile,
) -> Result<SegmentedOutcome, AppError> {
    // A new plan supersedes any rows an earlier attempt left behind; the
    // retired rows remain as history (rows are never deleted).
    retire_segments(ctx.repo, record_id)?;
    let mut planned = Vec::with_capacity(spans.len());
    let segment_count = spans.len();
    for span in spans {
        let id = ctx.repo.insert_segment(
            record_id,
            NewSegment {
                min_byte: span.min_byte,
                max_byte: span.max_byte,
                state: SegmentState::Pending,
                received_bytes: 0,
            },
        )?;
        planned.push(PlannedSegment { id, span });
    }
    ctx.progress.continue_from(0);

    let partial_path = temp.path().to_path_buf();
    let context = SegmentContext {
        client: ctx.client.clone(),
        repo: Arc::new(ctx.repo.clone()),
        effective_url: effective_url.clone(),
        redacted: redacted.to_owned(),
        total,
        partial_path: partial_path.clone(),
        progress: Arc::clone(&ctx.progress),
        shutdown: ctx.shutdown.clone(),
    };
    match download_segments(context, planned).await {
        Ok(SegmentBatchEnd::Completed) => {
            // Every worker synced its span and recorded completion; before
            // the rename, one last check that the file on disk really holds
            // the validated total — a lost or short write must never
            // finalize (the same retryable length-violation shape as the
            // workers' own premature-end refusal).
            let on_disk = tokio::fs::metadata(&partial_path).await.map_err(|err| {
                AppError::new(
                    ErrorCategory::Filesystem,
                    format!(
                        "the partial file {} could not be measured before \
                             finalization: {err}",
                        partial_path.display()
                    ),
                )
                .with_source(err)
            })?;
            if on_disk.len() != total {
                let err = AppError::new(
                    ErrorCategory::RangeValidation,
                    format!(
                        "the segmented transfer of {redacted} wrote {} of the expected \
                         {total} bytes; the incomplete file will not be finalized",
                        on_disk.len()
                    ),
                )
                .with_source(HttpFailure::transport());
                drop(temp);
                discard_segments(ctx.repo, record_id, &partial_path).await;
                return Err(err);
            }
            // The rename is the only step that may still fail.
            let moved = finalize_claimed(
                ctx,
                record_id,
                DownloadState::Downloading,
                temp,
                file_name,
                &ctx.output_dir.join(file_name),
            )
            .await?;
            tracing::info!(
                "download {record_id}: the {segment_count}-segment transfer of {redacted} finalized"
            );
            Ok(SegmentedOutcome::Completed(SequentialDownload {
                destination: moved,
                file_name: file_name.to_owned(),
                bytes_written: total,
                expected_size: Some(total),
                etag: facts.etag.clone(),
                last_modified: facts.last_modified.clone(),
                effective_url: facts.effective_url.clone(),
            }))
        }
        Ok(SegmentBatchEnd::Interrupted) => {
            ctx.repo
                .update_status(record_id, DownloadState::Paused, (ctx.now)(), None)?;
            tracing::info!(
                "shutdown: the segmented transfer of {redacted} paused with its confirmed \
                 segments"
            );
            Ok(SegmentedOutcome::Interrupted)
        }
        Ok(SegmentBatchEnd::Fallback { reason }) => {
            drop(temp);
            discard_segments(ctx.repo, record_id, &partial_path).await;
            Ok(SegmentedOutcome::Fallback { reason })
        }
        Err(err) => {
            drop(temp);
            discard_segments(ctx.repo, record_id, &partial_path).await;
            Err(err)
        }
    }
}

/// Retires every non-`failed` segment row of a record, preserving the rows
/// as inspectable history (the Phase 6 retirement convention).
fn retire_segments(repo: &Repository, record_id: i64) -> Result<(), AppError> {
    for row in repo.list_segments(record_id)? {
        if row.state != SegmentState::Failed {
            repo.update_segment(row.id, SegmentState::Failed, 0)?;
        }
    }
    Ok(())
}

/// Best-effort cleanup of an aborted plan: retire its rows and truncate its
/// partial to zero. Cleanup failures are logged and never mask the transfer
/// error that caused the discard; a later attempt reopens the same file with
/// the same offset-zero truncation discipline.
async fn discard_segments(repo: &Repository, record_id: i64, partial_path: &Path) {
    if let Err(err) = retire_segments(repo, record_id) {
        tracing::warn!(
            "the aborted segment plan of download record {record_id} could not be retired: {}",
            err.message()
        );
    }
    if let Err(err) = PartialFile::resume_at(partial_path.to_path_buf(), 0).await {
        tracing::warn!(
            "the partial file {} could not be truncated after the aborted segment plan: {}",
            partial_path.display(),
            err.message()
        );
    }
}

/// The ranged continuation path of a reconciled resume plan.
async fn resume_attempt(
    ctx: &Attempt<'_>,
    record: DownloadRecord,
    _reconciled_segment: SegmentRecord,
    partial: PathBuf,
    offset: u64,
) -> Result<AttemptOutcome, AppError> {
    // Statuses first: every later failure reports from `downloading`.
    walk_to_downloading(ctx.repo, record.id, record.status, ctx.now)?;
    let facts = probe(ctx.client, ctx.url).await?;
    // The revalidated facts describe the current remote honestly from now
    // on; every path below (continue *and* restart) relies on them.
    ctx.repo
        .update_transfer_facts(record.id, &probe_facts(&facts), (ctx.now)())?;
    let effective_url = validate_url(&facts.effective_url)?;
    let redacted = redact_url(&facts.effective_url);

    // Gate 1: stored validators must revalidate.
    let identity = revalidate_identity(&record, &facts);
    let mut restart: Option<&'static str> = match &identity {
        Identity::Verified(_) | Identity::Tentative(_) => None,
        Identity::Changed(reason) => Some(*reason),
        Identity::Unverifiable => Some("the record stores no usable validator"),
    };
    // Gate 2: range support must be re-proven before a ranged fetch.
    if restart.is_none() && !matches!(facts.range, RangeSupport::Yes) {
        restart = Some("the server did not re-prove byte-range support during revalidation");
    }
    // Gate 3: the stored validator must ride as a header value.
    if let Some(value) = identity.if_range() {
        if restart.is_none() && reqwest::header::HeaderValue::from_str(value).is_err() {
            restart = Some("the stored validator cannot be represented as an HTTP header value");
        }
    }
    if let Some(reason) = restart {
        tracing::info!(
            "download record {}: cannot append ({reason}); restarting the transfer from zero",
            record.id
        );
        return full_transfer(
            ctx,
            record.id,
            record.file_name,
            effective_url,
            redacted,
            &facts,
            Some(partial),
            None,
        )
        .await;
    }
    let if_range = match &identity {
        Identity::Verified(value) | Identity::Tentative(value) => value.clone(),
        Identity::Changed(_) | Identity::Unverifiable => {
            unreachable!("a restart returned for every other identity verdict")
        }
    };

    // The ranged challenge: exactly one byte range, validated on arrival
    // against the trusted offset and the record's identity.
    let range_value = format!("bytes={offset}-");
    let mut response = ctx
        .client
        .send_with_validator(
            Method::GET,
            &effective_url,
            Some(&range_value),
            Some(if_range.as_str()),
        )
        .await?;
    let status = response.status();
    match status {
        StatusCode::PARTIAL_CONTENT => {}
        StatusCode::OK => {
            tracing::info!(
                "download record {}: the ranged answer was a full response (the server \
                 ignored the range); restarting from zero with its content",
                record.id
            );
            return full_transfer(
                ctx,
                record.id,
                record.file_name,
                effective_url,
                redacted,
                &facts,
                Some(partial),
                Some(response),
            )
            .await;
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            tracing::info!(
                "download record {}: the remote answered 416 to bytes from the offset; \
                 restarting the transfer from zero",
                record.id
            );
            return full_transfer(
                ctx,
                record.id,
                record.file_name,
                effective_url,
                redacted,
                &facts,
                Some(partial),
                None,
            )
            .await;
        }
        _ => {
            return Err(http_status_error(
                status,
                format!("the remote server responded HTTP {status} for {redacted}"),
            ));
        }
    }

    // The answer must be byte-anchored: Content-Range (if present at all)
    // parsed strictly, starting exactly at the trusted offset.
    let raw_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let Some(spec) = raw_range.as_deref().and_then(parse_content_range) else {
        tracing::info!(
            "download record {}: the ranged answer carried no usable Content-Range header; \
             restarting the transfer from zero",
            record.id
        );
        return full_transfer(
            ctx,
            record.id,
            record.file_name,
            effective_url,
            redacted,
            &facts,
            Some(partial),
            None,
        )
        .await;
    };
    if spec.first != offset || spec.last < spec.first {
        tracing::info!(
            "download record {}: the ranged answer does not start at the reconciled \
             offset ({offset}); restarting the transfer from zero",
            record.id
        );
        return full_transfer(
            ctx,
            record.id,
            record.file_name,
            effective_url,
            redacted,
            &facts,
            Some(partial),
            None,
        )
        .await;
    }

    // One header parse, one trust policy: a transformation can never back
    // the appended bytes, and a Content-Length that contradicts its own
    // Content-Range restarts the fetch.
    let meta = response_meta(&response);
    if meta.encoding == ContentEncoding::Transform {
        return Err(AppError::new(
            ErrorCategory::RangeValidation,
            format!(
                "the transfer of {redacted} declared a content transformation \
                 (Content-Encoding), which cannot be written as the resource's bytes; \
                 the transfer is refused"
            ),
        ));
    }
    if let ContentLength::Valid(len) = meta.cl {
        let span = spec
            .last
            .checked_sub(spec.first)
            .and_then(|difference| difference.checked_add(1));
        if span != Some(len) {
            tracing::info!(
                "download record {}: the ranged answer's Content-Length contradicts its \
                 Content-Range; restarting the transfer from zero",
                record.id
            );
            return full_transfer(
                ctx,
                record.id,
                record.file_name,
                effective_url,
                redacted,
                &facts,
                Some(partial),
                None,
            )
            .await;
        }
    }

    // The answer must not contradict the record's identity (the mock may
    // ignore `If-Range`; the engine checks the answer itself).
    if let Err(reason) = response_verifies(&record, &meta, &identity) {
        tracing::info!(
            "download record {}: cannot append ({reason}); restarting the transfer from zero",
            record.id
        );
        return full_transfer(
            ctx,
            record.id,
            record.file_name,
            effective_url,
            redacted,
            &facts,
            Some(partial),
            None,
        )
        .await;
    }

    // The declared total, when stated, is authoritative; a contradiction
    // with the record restarts instead of resolving the lie locally.
    let mut expected = record.expected_size;
    if let Some(total) = spec.total {
        if let Some(recorded) = record.expected_size {
            if recorded != total {
                tracing::info!(
                    "download record {}: the representation's total size is no longer the \
                     recorded one; restarting the transfer from zero",
                    record.id
                );
                return full_transfer(
                    ctx,
                    record.id,
                    record.file_name,
                    effective_url,
                    redacted,
                    &facts,
                    Some(partial),
                    None,
                )
                .await;
            }
        }
        expected = Some(total);
    }

    // Bytes from the offset onward may be appended; everything before them
    // was truncated/synced by the reopen itself.
    let temp = PartialFile::resume_at(partial, offset).await?;
    let segment = ensure_segment(ctx.repo, record.id, expected, offset)?;
    ctx.progress.continue_from(offset);

    let setup = StreamSetup {
        redacted,
        expected,
        segment,
        confirmed: offset,
    };
    match stream_transfer(ctx, &mut response, temp, setup, record.id).await? {
        (StreamEnd::Interrupted, _) => Ok(AttemptOutcome::Interrupted),
        (StreamEnd::Completed { total }, Some(temp)) => {
            let file_name = record.file_name.clone();
            let destination = record.destination.clone();
            let moved = finalize_claimed(
                ctx,
                record.id,
                DownloadState::Downloading,
                temp,
                &file_name,
                &destination,
            )
            .await?;
            Ok(AttemptOutcome::Completed(SequentialDownload {
                destination: moved,
                file_name,
                bytes_written: total,
                expected_size: expected,
                etag: meta.etag.clone().or(facts.etag.clone()),
                last_modified: meta.last_modified.clone().or(facts.last_modified.clone()),
                effective_url: facts.effective_url.clone(),
            }))
        }
        (StreamEnd::Completed { .. }, None) => {
            unreachable!("a completed stream hands its partial back for the rename")
        }
    }
}

/// Finalizes a validated partial file whose record sits in the
/// "streaming-complete" shape: claim the final name, same-volume-rename,
/// correct the record's destination when the claim was a sibling, and mark
/// the record completed. Finalization failures record `failed` along the
/// allowed edges and return the error for classification.
async fn finalize_claimed(
    ctx: &Attempt<'_>,
    record_id: i64,
    current: DownloadState,
    temp: PartialFile,
    file_name: &str,
    planned: &Path,
) -> Result<PathBuf, AppError> {
    // `finalizing` is written as late as honest: the claim is imminent. A
    // record already in `finalizing` (crash recovery) skips the write.
    if current != DownloadState::Finalizing {
        ctx.repo
            .update_status(record_id, DownloadState::Finalizing, (ctx.now)(), None)?;
    }
    let reserved = match reserve_destination(ctx.output_dir, file_name) {
        Ok(reserved) => reserved,
        Err(err) => {
            let failure = mark_failed_record(ctx.repo, record_id, &err, ctx.now, None);
            warn_write(&failure, record_id, "the failed status");
            return Err(err);
        }
    };
    let moved = match finalize(temp, reserved).await {
        Ok(moved) => moved,
        Err(err) => {
            let failure = mark_failed_record(ctx.repo, record_id, &err, ctx.now, None);
            warn_write(&failure, record_id, "the failed status");
            return Err(err);
        }
    };
    if moved != planned {
        ctx.repo.set_destination(record_id, &moved, (ctx.now)())?;
    }
    ctx.repo
        .update_status(record_id, DownloadState::Completed, (ctx.now)(), None)?;
    tracing::info!("download {record_id}: finalized at {}", moved.display());
    Ok(moved)
}

/// The `FinalizeAvailable` plan's own finishing path (no network work at
/// all): the file is already complete and confirmed.
async fn finalize_available(
    ctx: &Attempt<'_>,
    record: DownloadRecord,
    segment: SegmentRecord,
    partial: PathBuf,
) -> Result<AttemptOutcome, AppError> {
    // The record must be explicit about the segment being complete *before*
    // the rename swaps the file into place (the rename is the event that
    // makes "done" real; the row documents the basis that allowed it).
    ctx.repo
        .update_segment(segment.id, SegmentState::Completed, segment.received_bytes)?;
    let file_len = std::fs::metadata(&partial)
        .map(|meta| meta.len())
        .map_err(|err| {
            AppError::new(
                ErrorCategory::Filesystem,
                format!(
                    "the partial file {} could not be measured for finalization: {err}",
                    partial.display()
                ),
            )
            .with_source(err)
        })?;
    let temp = PartialFile::resume_at(partial, file_len).await?;
    let file_name = record.file_name.clone();
    let destination = record.destination.clone();
    let moved = finalize_claimed(
        ctx,
        record.id,
        record.status,
        temp,
        &file_name,
        &destination,
    )
    .await?;
    tracing::info!(
        "download record {}: finalized {} without network work",
        record.id,
        moved.display()
    );
    Ok(AttemptOutcome::Completed(SequentialDownload {
        destination: moved,
        file_name,
        bytes_written: file_len,
        expected_size: record.expected_size,
        etag: record.etag,
        last_modified: record.last_modified,
        effective_url: record.effective_url.unwrap_or(record.url),
    }))
}

/// Ensures exactly one viable whole-range segment row exists for a
/// sequential transfer with a known, nonzero size; returns `None` for
/// anchorless transfers (the size is unknown or zero).
///
/// When the newest row's span still matches the current expected size, its
/// confirmed claim is reset to the attempt's start offset. When the size
/// changed since the row was planned, the old rows are retired as `Failed`
/// (rows are never deleted — the history stays inspectable) and a fresh
/// row plans the new span.
fn ensure_segment(
    repo: &Repository,
    record_id: i64,
    expected: Option<u64>,
    offset: u64,
) -> Result<Option<SegmentRecord>, AppError> {
    let Some(expected) = expected.filter(|&size| size > 0) else {
        return Ok(None);
    };
    let rows = repo.list_segments(record_id)?;
    let usable: Vec<&SegmentRecord> = rows
        .iter()
        .filter(|row| row.min_byte == 0 && row.state != SegmentState::Failed)
        .collect();
    if let Some(newest) = usable.last() {
        if newest.max_byte == expected - 1 {
            // The planned span still matches the current remote: keep the
            // row and make its confirmed claim the attempt's start offset.
            repo.update_segment(newest.id, SegmentState::Downloading, offset)?;
            return Ok(Some((**newest).clone()));
        }
    }
    // Every remaining row is superseded: a mismatched whole-range plan, or a
    // segmented plan's spans whose bytes this sequential pass overwrites.
    // Retirement keeps the history inspectable (rows are never deleted)
    // without leaving a stale in-progress claim behind.
    for row in &rows {
        if row.state != SegmentState::Failed {
            repo.update_segment(row.id, SegmentState::Failed, 0)?;
        }
    }
    repo.insert_segment(
        record_id,
        NewSegment {
            min_byte: 0,
            max_byte: expected - 1,
            state: SegmentState::Downloading,
            received_bytes: 0,
        },
    )?;
    let rows = repo.list_segments(record_id)?;
    Ok(rows.last().cloned())
}

/// What the streaming loop ended with.
enum StreamEnd {
    /// The transfer streamed its validated length (or the server-defined
    /// end of stream, when no size claim existed).
    Completed { total: u64 },
    /// Shutdown was honored: everything trustworthy is already persisted.
    Interrupted,
}

/// Values the streaming loop needs to be honest about the transfer.
struct StreamSetup {
    redacted: String,
    expected: Option<u64>,
    segment: Option<SegmentRecord>,
    /// Bytes confirmed before this stream session (the resumable offset).
    confirmed: u64,
}

/// Streams one validated response (fully or as a ranged continuation) into
/// its partial file, confirming progress periodically, and honoring
/// shutdown at every boundary. The partial file returns to the caller for
/// finalization (a completed stream never drops it; an interrupted one
/// leaves its synced bytes on disk for the next attempt).
async fn stream_transfer(
    ctx: &Attempt<'_>,
    response: &mut reqwest::Response,
    mut partial: PartialFile,
    setup: StreamSetup,
    record_id: i64,
) -> Result<(StreamEnd, Option<PartialFile>), AppError> {
    let mut streamed: u64 = 0;
    let mut last_confirmed = setup.confirmed;
    loop {
        if ctx.shutdown.tripped() {
            let end = interrupt_stream(ctx, &mut partial, &setup, record_id, streamed).await?;
            return Ok((end, None));
        }
        let chunk = tokio::select! {
            chunk = ctx.client.read_chunk(response) => {
                chunk.map_err(|err| with_transfer_context(
                    err,
                    setup.confirmed + streamed,
                    setup.expected,
                    &setup.redacted,
                ))?
            }
            () = ctx.shutdown.wait() => {
                let end =
                    interrupt_stream(ctx, &mut partial, &setup, record_id, streamed).await?;
                return Ok((end, None));
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let size = chunk.len() as u64;
        ctx.progress.record_received(size);
        let written = partial.write_chunk(chunk.as_ref()).await.map_err(|err| {
            with_transfer_context(
                err,
                setup.confirmed + streamed,
                setup.expected,
                &setup.redacted,
            )
        })?;
        streamed += written;
        ctx.progress.record_written(written);
        let total = setup.confirmed + streamed;
        if let Some(segment) = &setup.segment {
            if total >= last_confirmed + PROGRESS_CONFIRM_INTERVAL_BYTES {
                partial.sync().await?;
                ctx.repo
                    .update_segment(segment.id, SegmentState::Downloading, total)?;
                last_confirmed = total;
            }
        }
    }
    // End of stream: with a size claim, the totals must agree exactly.
    let total = setup.confirmed + streamed;
    if let Some(expected) = setup.expected {
        if total != expected {
            return Err(premature_end_error(total, expected, &setup.redacted));
        }
    }
    // The file's bytes are now final: sync *before* the segment row claims
    // completion, then hand over to the rename.
    partial.sync().await?;
    if let Some(segment) = &setup.segment {
        ctx.repo
            .update_segment(segment.id, SegmentState::Completed, total)?;
    }
    tracing::debug!(
        "the transfer of {} streamed {} additional bytes this session",
        setup.redacted,
        streamed
    );
    Ok((StreamEnd::Completed { total }, Some(partial)))
}

/// The shutdown path of the stream: sync, persist the confirmed total, and
/// park the record `paused`. Writing the record's confirmed progress after
/// the fsync is what makes the pair trustworthy after a hard kill.
async fn interrupt_stream(
    ctx: &Attempt<'_>,
    partial: &mut PartialFile,
    setup: &StreamSetup,
    record_id: i64,
    streamed: u64,
) -> Result<StreamEnd, AppError> {
    let confirmed = setup.confirmed + streamed;
    partial.sync().await?;
    if let Some(segment) = &setup.segment {
        ctx.repo
            .update_segment(segment.id, SegmentState::Downloading, confirmed)?;
    }
    ctx.repo
        .update_status(record_id, DownloadState::Paused, (ctx.now)(), None)?;
    tracing::info!(
        "shutdown: the persisted transfer of {} paused at {confirmed} confirmed bytes",
        setup.redacted
    );
    Ok(StreamEnd::Interrupted)
}

/// Applies Phase 4's mid-transfer failure accounting to an error so
/// diagnostics always state how much data was durable.
///
/// Note the exact Phase 4 message shape: the streamed total counts the
/// confirmed prefix plus this session's writes.
fn premature_end_error(total: u64, expected: u64, redacted: &str) -> AppError {
    // A stream that ends before its claimed size is a transport-level
    // outcome (dropped connection, lying server body); the hidden marker
    // makes the bounded retry treat it as eligible, and the next attempt
    // resumes from the confirmed offset.
    AppError::new(
        ErrorCategory::RangeValidation,
        format!(
            "the transfer of {redacted} ended after {total} of the expected {expected} \
             bytes; the file is not complete and will not be finalized"
        ),
    )
    .with_source(HttpFailure::transport())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::fixture::Fixture;
    use crate::common::server::{Behavior, MockServer, RangeMode, Validators};
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::time::Duration;

    /// An isolated, auto-removed scratch directory for one test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "swiftfetch-p6-{tag}-{}-{stamp}",
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
        static TICK: AtomicU64 = AtomicU64::new(1_700_000_000);
        TICK.fetch_add(1, AtomicOrdering::Relaxed)
    }

    fn scratch_repo(tag: &str) -> (Scratch, Repository) {
        let scratch = Scratch::new(tag);
        let repo = Repository::open(&scratch.path().join("queue.sqlite3"))
            .expect("scratch repository opened");
        (scratch, repo)
    }

    /// Runs the persisted engine against one mock route, never shut down.
    async fn drive(
        repo: &Repository,
        server: &MockServer,
        path: &str,
        dir: &Path,
        retries: u32,
    ) -> Result<SequentialDownload, AppError> {
        drive_with_shutdown(repo, server, path, dir, retries, None).await
    }

    /// Like [`drive`], but the test may pass a pre-existing shutdown pair it
    /// controls (`Some((signal, handle))`).
    async fn drive_with_shutdown(
        repo: &Repository,
        server: &MockServer,
        path: &str,
        dir: &Path,
        retries: u32,
        shutdown: Option<(ShutdownSignal, ShutdownHandle)>,
    ) -> Result<SequentialDownload, AppError> {
        let client = HttpClient::default_client().expect("the default client builds");
        let url = server.url(path);
        let (_signal, handle) = shutdown.unwrap_or_else(ShutdownSignal::channel);
        transfer_persisted(
            &client,
            repo,
            &url,
            dir,
            Progress::shared(),
            &RetryPolicy::from_retries(retries),
            &SegmentationPolicy::default(),
            test_clock,
            &handle,
        )
        .await
    }

    /// Whether some recorded request is the engine's ranged challenge.
    fn has_ranged_request(server: &MockServer, path: &str, offset: u64) -> bool {
        server.requests().iter().any(|request| {
            request.method == "GET"
                && request.path == path
                && request.range.as_deref() == Some(&format!("bytes={offset}-"))
        })
    }

    /// A byte count the engine sent as `If-Range`, if any request did.
    fn if_range_header(server: &MockServer, path: &str) -> Option<String> {
        server
            .requests()
            .iter()
            .find(|request| request.method == "GET" && request.path == path)
            .and_then(|request| request.headers.get("if-range"))
            .cloned()
    }

    /// Inserts a record (`queued`), seeds facts, and walks it to `downloading`.
    fn seeded_downloading_record(
        repo: &Repository,
        dir: &Path,
        url: &str,
        file_name: &str,
        expected: Option<u64>,
    ) -> i64 {
        let id = repo
            .insert_download(
                NewDownload {
                    url: url.to_owned(),
                    destination: dir.join(file_name),
                    file_name: file_name.to_owned(),
                },
                test_clock(),
            )
            .expect("record inserted");
        repo.update_transfer_facts(
            id,
            &TransferFacts {
                effective_url: Some(url.to_owned()),
                expected_size: expected,
                etag: None,
                last_modified: None,
            },
            test_clock(),
        )
        .expect("facts seeded");
        repo.update_status(id, DownloadState::Probing, test_clock(), None)
            .expect("walk to probing");
        repo.update_status(id, DownloadState::Downloading, test_clock(), None)
            .expect("walk to downloading");
        id
    }

    /// Stages an engine-shaped partial file holding `bytes`.
    fn stage_partial(dir: &Path, file_name: &str, bytes: &[u8]) -> PathBuf {
        let partial = dir.join(format!(
            "{file_name}.swiftfetch-{}-1.part",
            std::process::id()
        ));
        std::fs::write(&partial, bytes).expect("partial staged");
        partial
    }

    /// Inserts the whole-range segment row of a sequential transfer.
    fn stage_segment(repo: &Repository, record_id: i64, expected: u64, received: u64) {
        repo.insert_segment(
            record_id,
            NewSegment {
                min_byte: 0,
                max_byte: expected - 1,
                state: SegmentState::Downloading,
                received_bytes: received,
            },
        )
        .expect("segment staged");
    }

    // ---------------------------------------------------------------------
    // plan_for: the recovery decision table (database + disk only).
    // ---------------------------------------------------------------------

    #[test]
    fn an_empty_database_plans_a_fresh_transfer() {
        let (scratch, repo) = scratch_repo("plan-empty");
        let plan =
            plan_for(&repo, "http://mock.test/plan.bin", scratch.path()).expect("a fresh plan");
        match plan {
            RecoveryPlan::Fresh {
                record: None,
                partial: None,
            } => {}
            other => panic!("expected a recordless fresh plan, got {other:?}"),
        }
    }

    #[test]
    fn a_completed_in_scope_record_is_reported_without_rerunning() {
        let (scratch, repo) = scratch_repo("plan-complete");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", Some(4));
        repo.update_status(id, DownloadState::Finalizing, test_clock(), None)
            .expect("finalizing");
        repo.update_status(id, DownloadState::Completed, test_clock(), None)
            .expect("completed");
        std::fs::write(scratch.path().join("plan.bin"), b"done").expect("file staged");

        match plan_for(&repo, url, scratch.path()).expect("the no-op plan") {
            RecoveryPlan::AlreadyComplete { record } => {
                assert_eq!(record.id, id);
                assert_eq!(record.status, DownloadState::Completed);
            }
            other => panic!("expected the already-complete plan, got {other:?}"),
        }
    }

    #[test]
    fn a_completed_record_with_a_missing_file_is_a_typed_error() {
        let (scratch, repo) = scratch_repo("plan-missing");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", Some(4));
        repo.update_status(id, DownloadState::Finalizing, test_clock(), None)
            .expect("finalizing");
        repo.update_status(id, DownloadState::Completed, test_clock(), None)
            .expect("completed");

        let err = plan_for(&repo, url, scratch.path()).expect_err("the file is gone");
        assert_eq!(err.category(), ErrorCategory::Database);
        assert!(
            err.message().contains("completed download never re-runs"),
            "the message explains the refusal: {}",
            err.message()
        );
    }

    #[test]
    fn two_unfinished_records_for_one_scope_are_refused() {
        let (scratch, repo) = scratch_repo("plan-dupes");
        let url = "http://mock.test/plan.bin";
        handle_dummy_insert(&repo, url, scratch.path());
        handle_dummy_insert(&repo, url, scratch.path());

        let err = plan_for(&repo, url, scratch.path()).expect_err("two records");
        assert_eq!(err.category(), ErrorCategory::Input);
        assert!(
            err.message().contains("unfinished downloads"),
            "the message names the ambiguity: {}",
            err.message()
        );
    }

    fn handle_dummy_insert(repo: &Repository, url: &str, dir: &Path) {
        repo.insert_download(
            NewDownload {
                url: url.to_owned(),
                destination: dir.join("plan.bin"),
                file_name: "plan.bin".to_owned(),
            },
            test_clock(),
        )
        .expect("record inserted");
    }

    #[test]
    fn the_reconcile_trusts_the_smaller_of_disk_and_database_progress() {
        let (scratch, repo) = scratch_repo("plan-min-rule");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", Some(300));
        stage_segment(&repo, id, 300, 150);
        stage_partial(scratch.path(), "plan.bin", &[0u8; 100]);

        match plan_for(&repo, url, scratch.path()).expect("the resume plan") {
            RecoveryPlan::Resume { offset, .. } => assert_eq!(offset, 100),
            other => panic!("expected a resume plan, got {other:?}"),
        }
        // The deconfirmation is persisted immediately: the row now says 100.
        let rows = repo.list_segments(id).expect("segments");
        assert_eq!(rows.last().expect("row").received_bytes, 100);
    }

    #[test]
    fn disk_bytes_beyond_the_database_can_be_untrusted_down_to_the_claim() {
        let (scratch, repo) = scratch_repo("plan-disk-ahead");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", Some(300));
        stage_segment(&repo, id, 300, 60);
        stage_partial(scratch.path(), "plan.bin", &[7u8; 100]);

        match plan_for(&repo, url, scratch.path()).expect("the resume plan") {
            RecoveryPlan::Resume { offset, .. } => assert_eq!(offset, 60),
            other => panic!("expected a resume plan, got {other:?}"),
        }
    }

    #[test]
    fn a_partial_larger_than_the_expected_size_restarts() {
        let (scratch, repo) = scratch_repo("plan-oversize");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", Some(100));
        stage_segment(&repo, id, 100, 100);
        stage_partial(scratch.path(), "plan.bin", &[0u8; 150]);

        match plan_for(&repo, url, scratch.path()).expect("the restart plan") {
            RecoveryPlan::Fresh {
                record: Some(record),
                partial,
            } => {
                assert_eq!(record.id, id);
                assert!(
                    partial.is_some(),
                    "the oversized partial is reused truncated"
                );
            }
            other => panic!("expected a fresh-restart plan, got {other:?}"),
        }
    }

    #[test]
    fn the_crash_between_last_write_and_finalization_finalizes_locally() {
        let (scratch, repo) = scratch_repo("plan-finalize");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", Some(100));
        stage_segment(&repo, id, 100, 100);
        stage_partial(scratch.path(), "plan.bin", &[0u8; 100]);

        match plan_for(&repo, url, scratch.path()).expect("the finalize plan") {
            RecoveryPlan::FinalizeAvailable { record, .. } => assert_eq!(record.id, id),
            other => panic!("expected the finalize-available plan, got {other:?}"),
        }
    }

    #[test]
    fn a_finalizing_record_with_the_renamed_file_closes_in_place() {
        let (scratch, repo) = scratch_repo("plan-in-place");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", Some(100));
        repo.update_status(id, DownloadState::Finalizing, test_clock(), None)
            .expect("finalizing");
        std::fs::write(scratch.path().join("plan.bin"), vec![0u8; 100]).expect("file staged");

        match plan_for(&repo, url, scratch.path()).expect("the in-place plan") {
            RecoveryPlan::CompleteInPlace { record } => assert_eq!(record.id, id),
            other => panic!("expected the complete-in-place plan, got {other:?}"),
        }
    }

    #[test]
    fn a_record_without_its_partial_restarts() {
        let (scratch, repo) = scratch_repo("plan-no-partial");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", Some(100));
        stage_segment(&repo, id, 100, 50);

        match plan_for(&repo, url, scratch.path()).expect("the restart plan") {
            RecoveryPlan::Fresh {
                record: Some(record),
                partial: None,
            } => assert_eq!(record.id, id),
            other => panic!("expected a recordless-restart plan, got {other:?}"),
        }
    }

    #[test]
    fn a_record_without_a_size_anchor_never_appends() {
        let (scratch, repo) = scratch_repo("plan-no-anchor");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", None);
        stage_partial(scratch.path(), "plan.bin", &[0u8; 50]);

        match plan_for(&repo, url, scratch.path()).expect("the restart plan") {
            RecoveryPlan::Fresh {
                record: Some(record),
                ..
            } => assert_eq!(record.id, id),
            other => panic!("expected a restart plan, got {other:?}"),
        }
    }

    #[test]
    fn terminal_records_are_history_for_their_own_plan() {
        let (scratch, repo) = scratch_repo("plan-failed");
        let url = "http://mock.test/plan.bin";
        let id = seeded_downloading_record(&repo, scratch.path(), url, "plan.bin", Some(100));
        repo.update_status(
            id,
            DownloadState::Failed,
            test_clock(),
            Some(ErrorCategory::Http),
        )
        .expect("failed");

        match plan_for(&repo, url, scratch.path()).expect("the fresh plan") {
            RecoveryPlan::Fresh {
                record: None,
                partial: None,
            } => {}
            other => panic!("a failed record is a new transfer's history, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // Identity revalidation rules.
    // ---------------------------------------------------------------------

    #[test]
    fn identity_revalidation_compares_comparable_pairs_only() {
        let (scratch, repo) = scratch_repo("identity");
        let url = "http://mock.test/identity.bin";
        let id = repo
            .insert_download(
                NewDownload {
                    url: url.to_owned(),
                    destination: scratch.path().join("identity.bin"),
                    file_name: "identity.bin".to_owned(),
                },
                test_clock(),
            )
            .expect("inserted");
        repo.update_transfer_facts(
            id,
            &TransferFacts {
                effective_url: Some(url.to_owned()),
                expected_size: Some(4_096),
                etag: Some("\"e-1\"".to_owned()),
                last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
            },
            test_clock(),
        )
        .expect("facts");
        repo.update_status(id, DownloadState::Probing, test_clock(), None)
            .expect("probing");
        let record = repo
            .get_download(id)
            .expect("read back")
            .expect("the record exists");

        let verified = ProbeResult {
            expected_size: Some(4_096),
            range: RangeSupport::Yes,
            etag: Some("\"e-1\"".to_owned()),
            last_modified: None,
            effective_url: url.to_owned(),
        };
        match revalidate_identity(&record, &verified) {
            Identity::Verified(value) => assert_eq!(value, "\"e-1\""),
            other => panic!("matching etags verify identity, got {other:?}"),
        }

        let changed = ProbeResult {
            etag: Some("\"e-2\"".to_owned()),
            ..verified.clone()
        };
        assert!(
            matches!(revalidate_identity(&record, &changed), Identity::Changed(_)),
            "a different etag is a changed representation"
        );

        let redirected = ProbeResult {
            effective_url: "http://mock.test/identity-2.bin".to_owned(),
            ..verified.clone()
        };
        assert!(
            matches!(
                revalidate_identity(&record, &redirected),
                Identity::Changed(_)
            ),
            "a redirect resolving elsewhere restarts the identity"
        );

        let last_modified_only = ProbeResult {
            etag: None,
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
            ..verified.clone()
        };
        match revalidate_identity(&record, &last_modified_only) {
            Identity::Verified(value) => assert_eq!(value, "\"e-1\""),
            other => panic!("the matching last-modified verifies; the etag rides as the If-Range, got {other:?}"),
        }

        let silent = ProbeResult {
            etag: None,
            last_modified: None,
            ..verified.clone()
        };
        match revalidate_identity(&record, &silent) {
            Identity::Tentative(value) => assert_eq!(value, "\"e-1\""),
            other => {
                panic!("a stored validator without comparable facts is tentative, got {other:?}")
            }
        }

        // A record without any stored validator can never revalidate.
        let bare_id = repo
            .insert_download(
                NewDownload {
                    url: "http://mock.test/bare.bin".to_owned(),
                    destination: scratch.path().join("bare.bin"),
                    file_name: "bare.bin".to_owned(),
                },
                test_clock(),
            )
            .expect("inserted");
        repo.update_status(bare_id, DownloadState::Probing, test_clock(), None)
            .expect("probing");
        let bare = repo
            .get_download(bare_id)
            .expect("read back")
            .expect("the record exists");
        assert!(
            matches!(revalidate_identity(&bare, &silent), Identity::Unverifiable),
            "no stored validator means no revalidation"
        );
    }

    // ---------------------------------------------------------------------
    // Shutdown wiring.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn the_shutdown_handle_waits_for_a_trip_or_a_dropped_sender() {
        let (signal, handle) = ShutdownSignal::channel();
        assert!(!handle.tripped(), "a fresh signal is not tripped");
        let waiter = { handle.clone() };
        let wait = tokio::spawn(async move { waiter.wait().await });
        signal.trip();
        tokio::time::timeout(Duration::from_secs(2), wait)
            .await
            .expect("the trip resolves the waiter")
            .expect("the waiter task");
        assert!(handle.tripped(), "the handle observes the trip");

        // A dropped sender resolves the wait too: nobody can ever trip.
        let (signal, handle) = ShutdownSignal::channel();
        drop(signal);
        let wait = tokio::spawn(async move { handle.wait().await });
        tokio::time::timeout(Duration::from_secs(2), wait)
            .await
            .expect("the dropped sender resolves the waiter")
            .expect("the waiter task");
    }

    #[tokio::test]
    async fn a_tripped_shutdown_before_the_first_attempt_makes_no_record() {
        let (scratch, repo) = scratch_repo("shutdown-early");
        let client = HttpClient::default_client().expect("client builds");
        let (signal, handle) = ShutdownSignal::channel();
        signal.trip();
        let err = transfer_persisted(
            &client,
            &repo,
            "http://mock.test/never.bin",
            scratch.path(),
            Progress::shared(),
            &RetryPolicy::from_retries(5),
            &SegmentationPolicy::default(),
            test_clock,
            &handle,
        )
        .await
        .expect_err("shutdown was already requested");
        assert_eq!(err.category(), ErrorCategory::Cancelled);
        assert!(
            repo.list_downloads().expect("list").is_empty(),
            "the pre-transfer trip inserts nothing"
        );
    }

    // ---------------------------------------------------------------------
    // End-to-end engine behavior against the mock server.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn a_fresh_transfer_is_byte_exact_and_persisted() {
        let server = MockServer::start().await;
        server.mount_route(
            "/plain-flow.bin",
            Behavior::default().with_etag(Validators::Static),
        );
        let (scratch, repo) = scratch_repo("fresh-flow");
        let fixture = Fixture::default();

        let download = drive(&repo, &server, "/plain-flow.bin", scratch.path(), 5)
            .await
            .expect("the fresh transfer completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice(),
            "the finalized file is byte-exact"
        );
        assert_eq!(download.file_name, "plain-flow.bin");

        let record = repo
            .get_download(repo.list_downloads().expect("list")[0].id)
            .expect("read back")
            .expect("the record exists");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(record.expected_size, Some(fixture.len));
        assert!(
            record.etag.is_some(),
            "the static validator fixture publishes an etag for later resumes"
        );
        assert_eq!(
            record.destination,
            scratch.path().join("plain-flow.bin"),
            "the record's destination is the finalized file"
        );
        let rows = repo.list_segments(record.id).expect("segments");
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].state, SegmentState::Completed));
        assert_eq!(rows[0].received_bytes, fixture.len);

        // Probe (HEAD + ranged evidence GET) plus the transfer GET.
        assert_eq!(server.route_hits("/plain-flow.bin"), 3);
    }

    /// The identity gate of the resume: the record stores an etag from an
    /// earlier epoch, the static remote has its own, so revalidation rejects
    /// the append and the engine restarts from zero - without ever sending
    /// the tail-offset challenge.
    #[tokio::test]
    async fn a_changed_validator_restarts_instead_of_appending() {
        let server = MockServer::start().await;
        server.mount_route(
            "/changing-resume.bin",
            Behavior::default().with_etag(Validators::Static),
        );
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("changing-validator");
        let dir = scratch.path().to_owned();

        // A record whose etag cannot match the remote's static value.
        let id = seeded_downloading_record(
            &repo,
            &dir,
            &server.url("/changing-resume.bin"),
            "changing-resume.bin",
            Some(fixture.len),
        );
        repo.update_transfer_facts(
            id,
            &TransferFacts {
                effective_url: Some(server.url("/changing-resume.bin")),
                expected_size: Some(fixture.len),
                etag: Some("\"e-stale\"".to_owned()),
                last_modified: None,
            },
            test_clock(),
        )
        .expect("the stale validator stored");
        stage_segment(&repo, id, fixture.len, 512);
        stage_partial(&dir, "changing-resume.bin", &fixture.slice(0, 511));

        let download = drive(&repo, &server, "/changing-resume.bin", &dir, 5)
            .await
            .expect("the restart completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice(),
            "the restarted content is byte-exact with no appended prefix"
        );
        assert!(
            !has_ranged_request(&server, "/changing-resume.bin", 512),
            "a changed representation is never challenged with the tail offset"
        );
        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(
            record.retry_count, 0,
            "the restart is engine logic, not a retry"
        );
    }

    #[tokio::test]
    async fn a_range_hostile_server_restarts_instead_of_appending() {
        let server = MockServer::start().await;
        server.mount_route(
            "/range-hostile.bin",
            Behavior::default()
                .with_etag(Validators::Static)
                .with_range(RangeMode::Ignore),
        );

        // Learn the remote's static validator first, so the record below can
        // reverify cleanly (the probe also demonstrates the hostile answer:
        // a ranged request is answered with a full 200).
        let client = HttpClient::default_client().expect("client builds");
        let url = server.url("/range-hostile.bin");
        let learned = probe(&client, &url).await.expect("the probe succeeds");
        assert!(
            !matches!(learned.range, RangeSupport::Yes),
            "the hostile range answer is never recorded as evidence for ranges"
        );
        let etag = learned.etag.expect("the static fixture publishes an etag");

        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("range-hostile");
        let dir = scratch.path().to_owned();

        // A record whose validator matches the remote, so the restart below
        // comes from range hostility alone, not from a changed identity.
        let id =
            seeded_downloading_record(&repo, &dir, &url, "range-hostile.bin", Some(fixture.len));
        repo.update_transfer_facts(
            id,
            &TransferFacts {
                effective_url: Some(url.clone()),
                expected_size: Some(fixture.len),
                etag: Some(etag),
                last_modified: None,
            },
            test_clock(),
        )
        .expect("the matching validator stored");
        stage_segment(&repo, id, fixture.len, 512);
        stage_partial(&dir, "range-hostile.bin", &fixture.slice(0, 511));

        let download = drive(&repo, &server, "/range-hostile.bin", &dir, 5)
            .await
            .expect("the restart completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice(),
            "the restarted content is byte-exact with no appended prefix"
        );
        assert!(
            !has_ranged_request(&server, "/range-hostile.bin", 512),
            "a range-hostile remote is never challenged with the tail offset"
        );
        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(
            record.retry_count, 0,
            "the restart is engine logic, not a retry"
        );
    }

    /// The flagship Phase 6 scenario: shutdown mid-stream parks the record
    /// `paused` with trustworthy progress; the next invocation proves, via
    /// the captured headers, that it appended a validated, ranged, and
    /// revalidated remainder - and the result is byte-exact.
    #[tokio::test]
    async fn a_shutdown_mid_stream_resumes_with_the_range_and_validator_headers() {
        let server = MockServer::start().await;
        server.mount_route(
            "/resume-target.bin",
            Behavior::default()
                .with_etag(Validators::Static)
                .with_last_modified(Validators::Static)
                .with_delay_body_ms(60),
        );
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("shutdown-resume");
        let dir = scratch.path().to_owned();
        let client = Arc::new(HttpClient::default_client().expect("client builds"));
        let repo = Arc::new(repo);
        let progress = Progress::shared();
        let (signal, handle) = ShutdownSignal::channel();

        let url = server.url("/resume-target.bin");
        let task = {
            let repo = Arc::clone(&repo);
            let client = Arc::clone(&client);
            let task_progress = Arc::clone(&progress);
            let handle = handle.clone();
            tokio::spawn(async move {
                let result = transfer_persisted(
                    &client,
                    &repo,
                    &url,
                    &dir,
                    task_progress,
                    &RetryPolicy::from_retries(5),
                    &SegmentationPolicy::default(),
                    test_clock,
                    &handle,
                )
                .await;
                (result, repo, dir)
            })
        };

        // Wait for a real partial prefix, then trip the shutdown.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if progress.received() >= 512 && progress.received() < fixture.len {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the transfer never wrote a partial prefix before finishing"
            );
        }
        signal.trip();
        let (outcome, repo, dir) = task.await.expect("task joined");
        let err = outcome.expect_err("the shutdown is reported as cancellation");
        assert_eq!(err.category(), ErrorCategory::Cancelled);
        assert!(
            err.message().contains("paused and resumable"),
            "the message promises resumability: {}",
            err.message()
        );

        // The parked state is trustworthy: statuses paused, the partial file
        // holds exactly as many bytes as the segment row confirms.
        let record = repo
            .get_download(repo.list_downloads().expect("list")[0].id)
            .expect("read back")
            .expect("the record exists");
        assert_eq!(record.status, DownloadState::Paused);
        assert_eq!(record.retry_count, 0, "a shutdown is never a retry");
        let partial_path = scan_partials(&dir, "resume-target.bin")
            .expect("dir readable")
            .into_iter()
            .next()
            .expect("the interrupted partial exists");
        let partial_len = std::fs::metadata(&partial_path.path)
            .expect("partial measured")
            .len();
        assert!(partial_len >= 512 && partial_len < fixture.len);
        let rows = repo.list_segments(record.id).expect("segments");
        assert_eq!(
            rows[0].received_bytes, partial_len,
            "the confirmed offset matches the disk"
        );
        let stored_etag = record
            .etag
            .expect("the static fixture always carries an etag");

        // The second invocation resumes from exactly that offset, sending
        // the stored validator for revalidation.
        let requests_before = server.requests().len();
        let download = drive_with_shutdown(
            &repo,
            &server,
            "/resume-target.bin",
            &dir,
            5,
            Some(ShutdownSignal::channel()),
        )
        .await
        .expect("the resumed transfer completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice(),
            "prefix + appended remainder is byte-exact"
        );
        assert_eq!(download.file_name, "resume-target.bin");

        let resumed = &server.requests()[requests_before..];
        let challenge = resumed
            .iter()
            .find(|request| {
                request.method == "GET"
                    && request.range.as_deref() == Some(&format!("bytes={partial_len}-"))
            })
            .expect("the resume sent its ranged challenge");
        assert_eq!(
            challenge.headers.get("if-range").map(String::as_str),
            Some(stored_etag.as_str()),
            "the challenge revalidates with the stored validator"
        );

        let finalized = repo
            .get_download(record.id)
            .expect("read back")
            .expect("present");
        assert_eq!(finalized.status, DownloadState::Completed);
        assert_eq!(finalized.destination, dir.join("resume-target.bin"));
    }

    /// A remote whose ranged answers contradict themselves (`BrokenHeaders`)
    /// must never be trusted for an append, even though a plausible
    /// interpretation of the lies exists: the probe's evidence lands as
    /// unknown, the tail-offset challenge is never sent, and the whole
    /// content is fetched from zero.
    #[tokio::test]
    async fn broken_range_headers_restart_instead_of_trusting_the_206() {
        let server = MockServer::start().await;
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("broken-headers");
        let dir = scratch.path().to_owned();
        let id = seeded_downloading_record(
            &repo,
            &dir,
            &server.url("/broken-headers.bin"),
            "broken-headers.bin",
            Some(fixture.len),
        );
        stage_segment(&repo, id, fixture.len, 512);
        stage_partial(&dir, "broken-headers.bin", &fixture.slice(0, 511));

        server.mount_route(
            "/broken-headers.bin",
            Behavior::default().with_range(RangeMode::BrokenHeaders),
        );
        let download = drive(&repo, &server, "/broken-headers.bin", &dir, 5)
            .await
            .expect("the restart completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice(),
            "the restarted content is byte-exact; the lying 206 was never trusted"
        );
        assert!(
            !has_ranged_request(&server, "/broken-headers.bin", 512),
            "contradictory range evidence is never followed by a tail challenge"
        );
        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(
            record.retry_count, 0,
            "the restart is engine logic, not a retry"
        );
    }

    #[tokio::test]
    async fn the_finalize_available_plan_finalizes_without_any_network() {
        let server = MockServer::start().await;
        server.mount_route("/finalize-now.bin", Behavior::default());
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("finalize-available");
        let id = seeded_downloading_record(
            &repo,
            scratch.path(),
            &server.url("/finalize-now.bin"),
            "finalize-now.bin",
            Some(fixture.len),
        );
        stage_segment(&repo, id, fixture.len, fixture.len);
        stage_partial(
            scratch.path(),
            "finalize-now.bin",
            fixture.bytes().as_slice(),
        );

        let download = drive(&repo, &server, "/finalize-now.bin", scratch.path(), 5)
            .await
            .expect("the local finalize completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
        assert_eq!(
            server.route_hits("/finalize-now.bin"),
            0,
            "no request was made"
        );
        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        let rows = repo.list_segments(id).expect("segments");
        assert!(matches!(rows[0].state, SegmentState::Completed));
    }

    #[tokio::test]
    async fn truncated_bodies_are_retried_and_the_failures_are_counted() {
        let server = MockServer::start().await;
        server.mount_route(
            "/truncating.bin",
            Behavior::default().with_truncate_first(4, 512),
        );
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("truncating");

        let download = drive(&repo, &server, "/truncating.bin", scratch.path(), 5)
            .await
            .expect("two dropped attempts, then success");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
        let record = repo
            .get_download(repo.list_downloads().expect("list")[0].id)
            .expect("read back")
            .expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(
            record.retry_count, 2,
            "each transient failure accumulates one retry"
        );
        // Three attempts, each probed: 3 × (HEAD + evidence GET) + transfer
        // GETs, of which two were truncated and one finished the job — every
        // request reached the server, so nine responses were counted. (The
        // probe's own evidence requests occupy the first two truncation
        // slots, so the countdown is four.)
        assert_eq!(server.route_hits("/truncating.bin"), 9);
    }

    #[tokio::test]
    async fn a_fresh_probe_presented_with_a_sentinel_outage_retries_once() {
        let server = MockServer::start().await;
        server.mount_route(
            "/sentinel-outage.bin",
            Behavior::default().with_fail_first(1, 503),
        );
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("sentinel-outage");

        let download = drive(&repo, &server, "/sentinel-outage.bin", scratch.path(), 5)
            .await
            .expect("one blipped probe, then success");
        assert_eq!(download.bytes_written, fixture.len);
        let record = repo
            .get_download(repo.list_downloads().expect("list")[0].id)
            .expect("read back")
            .expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(
            record.retry_count, 0,
            "the blip happened before any record existed, so no record counts it"
        );
    }

    #[tokio::test]
    async fn exhausting_the_budget_fails_the_record_with_the_exhausted_category() {
        let server = MockServer::start().await;
        server.mount_route(
            "/exhausting.bin",
            Behavior::default().with_truncate_first(9, 512),
        );
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("exhausting");

        let err = drive(&repo, &server, "/exhausting.bin", scratch.path(), 1)
            .await
            .expect_err("a budget of one retry cannot survive two drops");
        assert_eq!(err.category(), ErrorCategory::RetryExhausted);
        assert!(
            err.message().contains("could not finish within 2 attempts"),
            "the message names the budget: {}",
            err.message()
        );

        let record = repo
            .get_download(repo.list_downloads().expect("list")[0].id)
            .expect("read back")
            .expect("the record exists");
        assert_eq!(record.status, DownloadState::Failed);
        assert_eq!(record.error_category, Some(ErrorCategory::RetryExhausted));
        assert_eq!(record.retry_count, 1);
        let partials = scan_partials(scratch.path(), "exhausting.bin").expect("dir readable");
        // The truncated attempts streamed at most one chunk; whatever the
        // client delivered, no partial may ever be finalized and no final
        // file may exist.
        for partial in &partials {
            assert!(
                std::fs::metadata(&partial.path)
                    .expect("partial measured")
                    .len()
                    < fixture.len,
                "a truncated attempt never reaches the byte-exact length"
            );
        }
        assert!(!scratch.path().join("exhausting.bin").exists());
    }

    #[tokio::test]
    async fn a_permanent_failure_during_a_resume_fails_the_record_and_keeps_the_partial() {
        let server = MockServer::start().await;
        server.mount_route("/resume-404.bin", Behavior::default().with_status(404));
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("resume-404");
        let dir = scratch.path().to_owned();
        let id = seeded_downloading_record(
            &repo,
            &dir,
            &server.url("/resume-404.bin"),
            "resume-404.bin",
            Some(fixture.len),
        );
        stage_segment(&repo, id, fixture.len, 512);
        stage_partial(&dir, "resume-404.bin", &fixture.slice(0, 511));

        let err = drive(&repo, &server, "/resume-404.bin", &dir, 5)
            .await
            .expect_err("a 404 during a resume is permanent");
        assert!(
            err.message().contains("HTTP 404"),
            "the message reports the genuine answer: {}",
            err.message()
        );

        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.status, DownloadState::Failed);
        assert_eq!(record.error_category, Some(ErrorCategory::Http));
        assert_eq!(
            record.retry_count, 0,
            "a permanent failure never consumes a retry"
        );
        let partial = scan_partials(&dir, "resume-404.bin")
            .expect("dir readable")
            .into_iter()
            .next()
            .expect("the partial survives a permanent failure");
        assert_eq!(
            std::fs::metadata(&partial.path)
                .expect("partial measured")
                .len(),
            512
        );
        assert!(!dir.join("resume-404.bin").exists());
    }

    #[tokio::test]
    async fn shutdown_during_the_retry_backoff_parks_the_record_paused_then_resumes() {
        let server = MockServer::start().await;
        server.mount_route(
            "/backoff-shutdown.bin",
            Behavior::default().with_truncate_first(2, 512),
        );
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("backoff-shutdown");
        let dir = scratch.path().to_owned();
        let repo = Arc::new(repo);
        let url = server.url("/backoff-shutdown.bin");

        // A large base delay makes the backoff window effectively endless,
        // so the trip deterministically lands inside it.
        let slow = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_secs(60),
            factor: 2,
            max_delay: Duration::from_secs(60),
        };

        let (signal, handle) = ShutdownSignal::channel();
        let task = {
            let repo = Arc::clone(&repo);
            let handle = handle.clone();
            let client = Arc::new(HttpClient::default_client().expect("client builds"));
            tokio::spawn(async move {
                let result = transfer_persisted(
                    &client,
                    &repo,
                    &url,
                    &dir,
                    Progress::shared(),
                    &slow,
                    &SegmentationPolicy::default(),
                    test_clock,
                    &handle,
                )
                .await;
                (result, Arc::clone(&repo), dir)
            })
        };

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let record_id = loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(record) = repo
                .list_downloads()
                .expect("list")
                .into_iter()
                .find(|record| record.status == DownloadState::Retrying)
            {
                break record.id;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the first attempt never entered its backoff"
            );
        };
        signal.trip();
        let (outcome, repo, dir) = task.await.expect("task joined");
        let err = outcome.expect_err("shutdown during backoff is cancellation");
        assert_eq!(err.category(), ErrorCategory::Cancelled);
        assert!(
            err.message().contains("backoff"),
            "the message says where the shutdown landed: {}",
            err.message()
        );
        let parked = repo
            .get_download(record_id)
            .expect("read")
            .expect("present");
        assert_eq!(parked.status, DownloadState::Paused);
        assert_eq!(
            parked.retry_count, 1,
            "the counted transient failure stays counted"
        );

        // The parked record resumes to a byte-exact completion. The partial
        // holds 512 disk bytes with a 0-byte database claim: the restart
        // truncates and refetches without duplicating anything.
        let download = drive_with_shutdown(
            &repo,
            &server,
            "/backoff-shutdown.bin",
            &dir,
            5,
            Some(ShutdownSignal::channel()),
        )
        .await
        .expect("the parked transfer completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
    }

    #[tokio::test]
    async fn a_validatorless_record_never_sends_a_conditional_request() {
        let server = MockServer::start().await;
        server.mount_route(
            "/unvalidated.bin",
            Behavior::default().with_truncate_first(1, 512),
        );
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("validatorless");
        let dir = scratch.path().to_owned();
        let id = seeded_downloading_record(
            &repo,
            &dir,
            &server.url("/unvalidated.bin"),
            "unvalidated.bin",
            Some(fixture.len),
        );
        stage_segment(&repo, id, fixture.len, 512);
        stage_partial(&dir, "unvalidated.bin", &fixture.slice(0, 511));

        // The first GET drops mid-body; the retry goes through the restart
        // path (no validator to revalidate), which must not send If-Range.
        let download = drive(&repo, &server, "/unvalidated.bin", &dir, 5)
            .await
            .expect("the validatorless transfer completes by restart");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
        assert_eq!(
            if_range_header(&server, "/unvalidated.bin"),
            None,
            "no conditional request may ever carry an invented validator"
        );
    }

    // ---------------------------------------------------------------------
    // The pristine refresh (Phase 7, ADR-0020): a queued record that was
    // never probed and owns nothing on disk takes its planned name from the
    // effective URL; facts or a partial file pin the recorded name instead.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn a_pristine_queued_record_takes_its_name_from_the_effective_url() {
        let server = MockServer::start().await;
        server.mount_route("/effective-name.bin", Behavior::default());
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("pristine-name");
        let dir = scratch.path().to_owned();
        let url = server.url("/effective-name.bin");
        let id = repo
            .insert_download(
                NewDownload {
                    url: url.clone(),
                    destination: dir.join("provisional.bin"),
                    file_name: "provisional.bin".to_owned(),
                },
                test_clock(),
            )
            .expect("queued record inserted");

        let download = drive(&repo, &server, "/effective-name.bin", &dir, 0)
            .await
            .expect("the queued download runs");
        assert_eq!(download.file_name, "effective-name.bin");
        assert_eq!(download.destination, dir.join("effective-name.bin"));
        assert_eq!(download.bytes_written, fixture.len);

        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(
            record.file_name, "effective-name.bin",
            "the provisional name is refreshed to the effective URL's name"
        );
        assert_eq!(record.destination, dir.join("effective-name.bin"));
        assert!(
            !dir.join("provisional.bin").exists(),
            "the provisional name never materializes"
        );
    }

    #[tokio::test]
    async fn a_record_with_confirmed_facts_keeps_its_recorded_name() {
        let server = MockServer::start().await;
        server.mount_route("/kept-name.bin", Behavior::default());
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("pinned-facts");
        let dir = scratch.path().to_owned();
        // Every field matches the route, yet the planned name deliberately
        // differs: facts mean an earlier attempt made the name real, so it
        // must survive the restart (ADR-0020).
        let id = seeded_downloading_record(
            &repo,
            &dir,
            &server.url("/kept-name.bin"),
            "pinned.bin",
            Some(fixture.len),
        );

        let download = drive(&repo, &server, "/kept-name.bin", &dir, 0)
            .await
            .expect("the restart completes");
        assert_eq!(download.file_name, "pinned.bin");
        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(record.file_name, "pinned.bin");
        assert_eq!(record.destination, dir.join("pinned.bin"));
        assert!(!dir.join("kept-name.bin").exists());
    }

    #[tokio::test]
    async fn an_engine_owned_partial_pins_the_recorded_name() {
        let server = MockServer::start().await;
        server.mount_route("/pinned-partial.bin", Behavior::default());
        let fixture = Fixture::default();
        let (scratch, repo) = scratch_repo("pinned-partial");
        let dir = scratch.path().to_owned();
        let id = repo
            .insert_download(
                NewDownload {
                    url: server.url("/pinned-partial.bin"),
                    destination: dir.join("pinned-partial.bin"),
                    file_name: "pinned-partial.bin".to_owned(),
                },
                test_clock(),
            )
            .expect("queued record inserted");
        // An aborted earlier attempt left its engine-owned partial behind;
        // no facts were confirmed, so the plan restarts from zero — but the
        // name cannot change under the restart (the partial is named after
        // it, and the restart reuses that file).
        stage_partial(&dir, "pinned-partial.bin", b"aborted attempt bytes");

        let download = drive(&repo, &server, "/pinned-partial.bin", &dir, 0)
            .await
            .expect("the restarted transfer completes");
        assert_eq!(download.file_name, "pinned-partial.bin");
        assert_eq!(download.bytes_written, fixture.len);
        let record = repo.get_download(id).expect("read back").expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(record.file_name, "pinned-partial.bin");
        assert_eq!(
            std::fs::read(dir.join("pinned-partial.bin"))
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
    }

    // ---------------------------------------------------------------------
    // Phase 8: segmented transfers against the mock server. A test-scale
    // policy keeps every scenario small; the production defaults (32 MiB /
    // 8 MiB) are exercised by the planner's unit tests.
    // ---------------------------------------------------------------------

    /// A test-scale segmentation policy; the real thresholds start at
    /// 32 MiB (`DOCS/architecture.md` §6), which no mock fixture reaches.
    fn small_segments(segment_size: u64, max_segments: u32) -> SegmentationPolicy {
        SegmentationPolicy {
            min_file_size: 1,
            segment_size,
            max_segments,
        }
    }

    /// Runs the persisted engine under one segmentation policy.
    async fn drive_segmented(
        repo: &Repository,
        server: &MockServer,
        path: &str,
        dir: &Path,
        retries: u32,
        policy: &SegmentationPolicy,
    ) -> Result<SequentialDownload, AppError> {
        let client = HttpClient::default_client().expect("the default client builds");
        let url = server.url(path);
        let (_signal, handle) = ShutdownSignal::channel();
        transfer_persisted(
            &client,
            repo,
            &url,
            dir,
            Progress::shared(),
            &RetryPolicy::from_retries(retries),
            policy,
            test_clock,
            &handle,
        )
        .await
    }

    /// Every raw `Range` header seen by one route, in arrival order.
    fn ranged_values(server: &MockServer, path: &str) -> Vec<String> {
        server
            .requests()
            .iter()
            .filter(|request| request.method == "GET" && request.path == path)
            .filter_map(|request| request.range.clone())
            .collect()
    }

    /// The ranged GETs that carry a planned span (not the probe's
    /// `bytes=0-0` evidence request).
    fn transfer_ranges(server: &MockServer, path: &str) -> Vec<String> {
        ranged_values(server, path)
            .into_iter()
            .filter(|value| value != "bytes=0-0")
            .collect()
    }

    /// Unranged GETs of one route (the sequential path's signature).
    fn unranged_gets(server: &MockServer, path: &str) -> usize {
        server
            .requests()
            .iter()
            .filter(|request| {
                request.method == "GET" && request.path == path && request.range.is_none()
            })
            .count()
    }

    /// The segment rows of the only download record (tests use one scope).
    fn sole_rows(repo: &Repository) -> Vec<SegmentRecord> {
        let record = repo
            .list_downloads()
            .expect("downloads listed")
            .into_iter()
            .next()
            .expect("one record exists");
        repo.list_segments(record.id).expect("segments listed")
    }

    /// One segmented fallback's final ledger: the aborted plan's four spans
    /// are retired `Failed` (claiming no bytes) and the sequential
    /// continuation books exactly one completed whole-range row.
    fn assert_fallback_ledger(repo: &Repository, total: u64) {
        let rows = sole_rows(repo);
        let failed: Vec<_> = rows
            .iter()
            .filter(|row| row.state == SegmentState::Failed)
            .collect();
        let completed: Vec<_> = rows
            .iter()
            .filter(|row| row.state == SegmentState::Completed)
            .collect();
        assert_eq!(
            rows.len(),
            failed.len() + completed.len(),
            "no span may hover between states after a fallback: {rows:?}"
        );
        assert_eq!(
            failed.len(),
            4,
            "the whole aborted plan is retired: {rows:?}"
        );
        assert!(
            failed.iter().all(|row| row.received_bytes == 0),
            "a retired span claims nothing: {rows:?}"
        );
        assert_eq!(
            completed.len(),
            1,
            "the fallback books one sequential row: {rows:?}"
        );
        let sequential = completed[0];
        assert_eq!(
            (
                sequential.min_byte,
                sequential.max_byte,
                sequential.received_bytes
            ),
            (0, total - 1, total),
        );
    }

    /// The error category a failed segmented invocation recorded.
    fn failed_record_category(repo: &Repository) -> (DownloadState, Option<ErrorCategory>) {
        let record = repo
            .list_downloads()
            .expect("downloads listed")
            .into_iter()
            .next()
            .expect("one record exists");
        (record.status, record.error_category)
    }

    #[tokio::test]
    async fn a_segmented_transfer_writes_every_planned_span_and_stays_byte_exact() {
        let fixture = Fixture::new(77, 10_000);
        let server = MockServer::start().await;
        server.mount_route("/segmented.bin", Behavior::default().with_fixture(fixture));
        let (scratch, repo) = scratch_repo("segmented-happy");
        let dir = scratch.path().to_owned();

        // ceil(10000 / 3000) = 4 spans at the configured maximum; the last
        // span is short (1000 bytes).
        let policy = small_segments(3_000, 4);
        let download = drive_segmented(&repo, &server, "/segmented.bin", &dir, 0, &policy)
            .await
            .expect("the segmented transfer completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice(),
            "segmented output is byte-identical to the source"
        );

        // Exactly the planned spans were requested (order is scheduling).
        let mut requested = transfer_ranges(&server, "/segmented.bin");
        requested.sort();
        assert_eq!(
            requested,
            vec![
                "bytes=0-2999",
                "bytes=3000-5999",
                "bytes=6000-8999",
                "bytes=9000-9999",
            ],
            "the plan covers the object exactly once"
        );
        assert_eq!(
            unranged_gets(&server, "/segmented.bin"),
            0,
            "a successful segmented transfer never falls back to a full GET"
        );

        // Every span is recorded completed with exactly its own byte count.
        let rows = sole_rows(&repo);
        let expected = [
            (0u64, 2_999u64),
            (3_000, 5_999),
            (6_000, 8_999),
            (9_000, 9_999),
        ];
        assert_eq!(rows.len(), expected.len());
        for (row, (min, max)) in rows.iter().zip(expected) {
            assert_eq!((row.min_byte, row.max_byte), (min, max));
            assert_eq!(row.state, SegmentState::Completed);
            assert_eq!(
                row.received_bytes,
                max - min + 1,
                "a completed span claims its full length"
            );
        }
        let record = repo
            .get_download(repo.list_downloads().expect("list")[0].id)
            .expect("read back")
            .expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert!(
            scan_partials(&dir, "segmented.bin")
                .expect("dir readable")
                .is_empty(),
            "no partial survives a completed finalization"
        );
    }

    #[tokio::test]
    async fn configured_segment_maximums_produce_two_three_and_four_spans() {
        let fixture = Fixture::new(88, 30_000);
        for expected_count in [2u32, 3, 4] {
            let server = MockServer::start().await;
            server.mount_route("/spans.bin", Behavior::default().with_fixture(fixture));
            let (scratch, repo) = scratch_repo(&format!("spans-{expected_count}"));
            let dir = scratch.path().to_owned();
            let policy = small_segments(4_096, expected_count);
            let download = drive_segmented(&repo, &server, "/spans.bin", &dir, 0, &policy)
                .await
                .unwrap_or_else(|err| panic!("max {expected_count} must complete: {err}"));
            assert_eq!(download.bytes_written, fixture.len);
            assert_eq!(
                std::fs::read(&download.destination)
                    .expect("final file")
                    .as_slice(),
                fixture.bytes().as_slice()
            );
            let rows = sole_rows(&repo);
            assert_eq!(
                rows.len(),
                usize::try_from(expected_count).expect("small count"),
                "max {expected_count} must plan exactly {expected_count} spans for this size"
            );
            assert!(
                rows.iter().all(|row| row.state == SegmentState::Completed),
                "every span of a completed transfer is completed"
            );
        }
    }

    #[tokio::test]
    async fn per_file_workers_stay_within_the_configured_segment_maximum() {
        let fixture = Fixture::new(99, 20_000);
        let server = MockServer::start().await;
        server.mount_route(
            "/parallel.bin",
            Behavior::default()
                .with_fixture(fixture)
                .with_delay_body_ms(20),
        );
        let (scratch, repo) = scratch_repo("segmented-parallel");
        let dir = scratch.path().to_owned();

        // 20 000 / 5 000 = exactly 4 spans at the configured maximum.
        let policy = small_segments(5_000, 4);
        let download = drive_segmented(&repo, &server, "/parallel.bin", &dir, 0, &policy)
            .await
            .expect("the segmented transfer completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );

        let peak = server.peak_active_gets();
        assert!(
            peak <= 4,
            "the per-file worker maximum bounds active ranges, peak was {peak}"
        );
        assert!(
            peak >= 2,
            "segments must run concurrently (delayed bodies), peak was {peak}"
        );
        let rows = sole_rows(&repo);
        assert_eq!(rows.len(), 4);
    }

    #[tokio::test]
    async fn a_file_below_the_segmentation_threshold_stays_sequential() {
        let fixture = Fixture::new(123, 1_000);
        let server = MockServer::start().await;
        server.mount_route("/small.bin", Behavior::default().with_fixture(fixture));
        let (scratch, repo) = scratch_repo("segmented-small");
        let dir = scratch.path().to_owned();

        // The threshold sits above the file: ranges were proven, but the
        // size gate refuses segmentation.
        let policy = SegmentationPolicy {
            min_file_size: 2_000,
            segment_size: 512,
            max_segments: 4,
        };
        let download = drive_segmented(&repo, &server, "/small.bin", &dir, 0, &policy)
            .await
            .expect("the sequential transfer completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
        assert_eq!(
            ranged_values(&server, "/small.bin"),
            vec!["bytes=0-0"],
            "only the probe's evidence range may be requested"
        );
        assert_eq!(unranged_gets(&server, "/small.bin"), 1);
        // A sequential transfer books one whole-range row (the Phase 6
        // convention); what the size gate refuses is the multi-span plan.
        let rows = sole_rows(&repo);
        assert_eq!(rows.len(), 1, "the size gate must not plan spans");
        assert_eq!((rows[0].min_byte, rows[0].max_byte), (0, 999));
        assert_eq!(rows[0].state, SegmentState::Completed);
    }

    #[tokio::test]
    async fn an_unknown_size_resource_is_never_segmented() {
        let fixture = Fixture::new(321, 4_096);
        let server = MockServer::start().await;
        server.mount_route(
            "/unknown-size.bin",
            Behavior::default()
                .with_fixture(fixture)
                .with_head_length_omitted()
                .with_omit_content_length(true)
                .with_omit_range_total(true),
        );
        let (scratch, repo) = scratch_repo("segmented-unknown");
        let dir = scratch.path().to_owned();

        // Range support is proven (`bytes 0-0/*`), but no trusted size
        // exists: the plan gate must refuse segmentation.
        let policy = small_segments(512, 4);
        let download = drive_segmented(&repo, &server, "/unknown-size.bin", &dir, 0, &policy)
            .await
            .expect("the sequential transfer completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
        assert_eq!(
            ranged_values(&server, "/unknown-size.bin"),
            vec!["bytes=0-0"],
            "no segment range may be requested without a known size"
        );
        assert_eq!(unranged_gets(&server, "/unknown-size.bin"), 1);
    }

    #[tokio::test]
    async fn a_server_that_ignores_ranges_after_the_probe_falls_back_sequentially() {
        let fixture = Fixture::new(456, 8_192);
        let server = MockServer::start().await;
        server.mount_route(
            "/range-switch.bin",
            Behavior::default()
                .with_fixture(fixture)
                .with_range_switch_after(1, RangeMode::Ignore),
        );
        let (scratch, repo) = scratch_repo("segmented-ignore");
        let dir = scratch.path().to_owned();

        let policy = small_segments(2_048, 4);
        let download = drive_segmented(&repo, &server, "/range-switch.bin", &dir, 0, &policy)
            .await
            .expect("the fallback completes the transfer");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice(),
            "the sequential fallback is byte-exact"
        );
        assert_eq!(
            transfer_ranges(&server, "/range-switch.bin").len(),
            4,
            "the plan was attempted before the switch was detected"
        );
        assert_eq!(
            unranged_gets(&server, "/range-switch.bin"),
            1,
            "exactly the fallback GET is unranged"
        );
        assert_fallback_ledger(&repo, fixture.len);
        let record = repo
            .get_download(repo.list_downloads().expect("list")[0].id)
            .expect("read back")
            .expect("present");
        assert_eq!(record.status, DownloadState::Completed);
    }

    #[tokio::test]
    async fn a_mismatched_content_range_after_the_probe_falls_back_sequentially() {
        let fixture = Fixture::new(654, 8_192);
        let server = MockServer::start().await;
        server.mount_route(
            "/broken-range.bin",
            Behavior::default()
                .with_fixture(fixture)
                .with_range_switch_after(1, RangeMode::BrokenHeaders),
        );
        let (scratch, repo) = scratch_repo("segmented-broken");
        let dir = scratch.path().to_owned();

        let policy = small_segments(2_048, 4);
        let download = drive_segmented(&repo, &server, "/broken-range.bin", &dir, 0, &policy)
            .await
            .expect("the fallback completes the transfer");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice(),
            "a lying Content-Range never becomes file content"
        );
        assert_eq!(unranged_gets(&server, "/broken-range.bin"), 1);
        assert_fallback_ledger(&repo, fixture.len);
    }

    #[tokio::test]
    async fn an_overrunning_segment_never_finalizes_a_false_download() {
        let fixture = Fixture::new(1_000, 8_192);
        let server = MockServer::start().await;
        server.mount_route(
            "/overrun.bin",
            Behavior::default()
                .with_fixture(fixture)
                .with_overrun_range_by(1_024),
        );
        let (scratch, repo) = scratch_repo("segmented-overrun");
        let dir = scratch.path().to_owned();

        // The first span's body streams 1024 bytes past its Content-Range;
        // the sibling span (clamped at the fixture's end) cannot overrun.
        let policy = small_segments(4_096, 4);
        let err = drive_segmented(&repo, &server, "/overrun.bin", &dir, 0, &policy)
            .await
            .expect_err("an overrunning body must fail the transfer");
        assert_eq!(err.category(), ErrorCategory::RetryExhausted);
        let (status, category) = failed_record_category(&repo);
        assert_eq!(status, DownloadState::Failed);
        assert_eq!(category, Some(ErrorCategory::RetryExhausted));
        assert!(
            !dir.join("overrun.bin").exists(),
            "an overrunning segment never yields a finalized file"
        );
        assert!(sole_rows(&repo)
            .iter()
            .all(|row| row.state == SegmentState::Failed));
    }

    #[tokio::test]
    async fn a_segment_http_error_fails_the_record_without_finalizing() {
        let fixture = Fixture::new(2_000, 4_096);
        let server = MockServer::start().await;
        server.mount_route(
            "/segment-500.bin",
            Behavior::default()
                .with_fixture(fixture)
                .with_segment_status(500),
        );
        let (scratch, repo) = scratch_repo("segmented-500");
        let dir = scratch.path().to_owned();

        let policy = small_segments(2_048, 4);
        let err = drive_segmented(&repo, &server, "/segment-500.bin", &dir, 0, &policy)
            .await
            .expect_err("a 500 on every segment must exhaust the budget");
        assert_eq!(err.category(), ErrorCategory::RetryExhausted);
        let (status, category) = failed_record_category(&repo);
        assert_eq!(status, DownloadState::Failed);
        assert_eq!(category, Some(ErrorCategory::RetryExhausted));
        assert!(
            !dir.join("segment-500.bin").exists(),
            "a segment status error never yields a finalized file"
        );
        assert_eq!(
            unranged_gets(&server, "/segment-500.bin"),
            0,
            "the 500 kept the transfer on the segmented path"
        );
        assert!(sole_rows(&repo)
            .iter()
            .all(|row| row.state == SegmentState::Failed));
    }

    #[tokio::test]
    async fn a_truncated_segment_is_retried_within_the_budget_and_never_finalized_early() {
        let fixture = Fixture::new(3_000, 8_192);
        let server = MockServer::start().await;
        server.mount_route(
            "/short-segment.bin",
            Behavior::default()
                .with_fixture(fixture)
                .with_truncate_after_probe(1, 100),
        );
        let (scratch, repo) = scratch_repo("segmented-truncated");
        let dir = scratch.path().to_owned();

        // One segment-shaped GET is cut after 100 bytes; the retry attempt
        // probes again and segments healthily.
        let policy = small_segments(4_096, 4);
        let download = drive_segmented(&repo, &server, "/short-segment.bin", &dir, 2, &policy)
            .await
            .expect("the retry completes the transfer");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
        let record = repo
            .get_download(repo.list_downloads().expect("list")[0].id)
            .expect("read back")
            .expect("present");
        assert_eq!(record.status, DownloadState::Completed);
        assert_eq!(
            record.retry_count, 1,
            "exactly one transient segment failure was counted"
        );
        let rows = sole_rows(&repo);
        let mut completed: Vec<(u64, u64)> = rows
            .iter()
            .filter(|row| row.state == SegmentState::Completed)
            .map(|row| (row.min_byte, row.max_byte))
            .collect();
        completed.sort_unstable();
        assert_eq!(
            completed,
            vec![(0, 4_095), (4_096, 8_191)],
            "the finishing attempt's spans all complete: {rows:?}"
        );
        assert!(
            rows.iter()
                .filter(|row| row.state != SegmentState::Completed)
                .all(|row| row.state == SegmentState::Failed && row.received_bytes == 0),
            "the truncated attempt's spans stay retired history: {rows:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_during_a_segmented_transfer_parks_it_paused_and_resumable() {
        let fixture = Fixture::new(4_000, 60_000);
        let server = MockServer::start().await;
        server.mount_route(
            "/segment-shutdown.bin",
            Behavior::default()
                .with_fixture(fixture)
                .with_delay_body_ms(20),
        );
        let (scratch, repo) = scratch_repo("segmented-shutdown");
        let dir = scratch.path().to_owned();
        let repo = Arc::new(repo);
        let policy = small_segments(16_384, 4);
        let progress = Progress::shared();
        let (signal, handle) = ShutdownSignal::channel();
        let url = server.url("/segment-shutdown.bin");

        let task = {
            let repo = Arc::clone(&repo);
            let handle = handle.clone();
            let client = Arc::new(HttpClient::default_client().expect("client builds"));
            let progress = Arc::clone(&progress);
            tokio::spawn(async move {
                let result = transfer_persisted(
                    &client,
                    &repo,
                    &url,
                    &dir,
                    progress,
                    &RetryPolicy::from_retries(0),
                    &policy,
                    test_clock,
                    &handle,
                )
                .await;
                (result, repo, dir)
            })
        };

        // Wait for real segmented progress, then trip the shutdown.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if progress.received() >= 4_096 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the segmented transfer never reported progress"
            );
        }
        signal.trip();
        let (outcome, repo, dir) = task.await.expect("task joined");
        let err = outcome.expect_err("shutdown is reported as cancellation");
        assert_eq!(err.category(), ErrorCategory::Cancelled);

        // The parked state is honest: paused, with per-span confirmed
        // progress and a partial file that backs it.
        let record = repo
            .list_downloads()
            .expect("list")
            .into_iter()
            .next()
            .expect("record exists");
        assert_eq!(record.status, DownloadState::Paused);
        let rows = repo.list_segments(record.id).expect("segments listed");
        assert_eq!(rows.len(), 4, "the plan's rows survive the shutdown");
        assert!(
            rows.iter()
                .any(|row| row.state == SegmentState::Downloading && row.received_bytes > 0),
            "at least one worker confirmed bytes before stopping: {rows:?}"
        );
        let partial = scan_partials(&dir, "segment-shutdown.bin")
            .expect("dir readable")
            .into_iter()
            .next()
            .expect("the partial survives the shutdown");
        assert!(std::fs::metadata(&partial.path).expect("measured").len() > 0);

        // A later invocation finishes the transfer byte-exactly. The same
        // small policy is used: whether recovery re-segments or reconciles a
        // sequential continuation is Phase 9 territory, but either way the
        // parked bytes are never trusted blindly.
        let download = drive_segmented(&repo, &server, "/segment-shutdown.bin", &dir, 0, &policy)
            .await
            .expect("the parked transfer completes");
        assert_eq!(download.bytes_written, fixture.len);
        assert_eq!(
            std::fs::read(&download.destination)
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice()
        );
        let rows = sole_rows(&repo);
        assert!(
            rows.iter()
                .all(|row| matches!(row.state, SegmentState::Completed | SegmentState::Failed)),
            "a completed record leaves no span mid-flight: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.state == SegmentState::Completed),
            "the finishing invocation records its completed spans: {rows:?}"
        );
    }
}
