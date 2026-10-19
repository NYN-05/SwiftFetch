//! Segmented (parallel byte-range) transfers (Phase 8).
//!
//! A segmented transfer splits one resource whose exact size the probe
//! established — and whose byte-range support the probe *proved* — into a
//! bounded list of non-overlapping inclusive ranges, and fetches them
//! concurrently through independent workers. The laws of this module:
//!
//! - **Eligibility is evidence-based.** Segmentation engages only when the
//!   probe answered [`crate::download::probe::RangeSupport::Yes`], the
//!   expected size is known, the size reaches the configured minimum, and
//!   the preferred segment count is at least two (a one-segment plan is a
//!   sequential transfer wearing a costume). Policy values follow
//!   `DOCS/architecture.md` §6 (32 MiB minimum, ~8 MiB segments, 4
//!   maximum), configurable within their fixed shape.
//! - **The planner covers the object exactly once.** Spans are contiguous,
//!   non-overlapping, start at byte 0, end at `total - 1`, and number no
//!   more than the configured maximum. Every arithmetic step is
//!   overflow-safe (`div_ceil` internally and a saturating extension guard
//!   at the `u64` edge), and the non-divisible tail lands in a final short
//!   segment.
//! - **Every ranged answer is validated before a single byte is written.**
//!   A worker requires `206 Partial Content` whose `Content-Range` is
//!   exactly its planned interval (with, when stated, the planned total),
//!   whose `Content-Length` (when stated) equals the interval's length, and
//!   whose body carries no content transformation. A body that ends early
//!   or overruns its interval is a typed validation failure — never file
//!   content — and no byte outside the planned interval is ever written.
//! - **Ignored, malformed, or unsupported ranges fall back safely.** A
//!   `200`, a `416`, an unparseable/mismatched `Content-Range`, a length
//!   contradiction, or a changed total cancels the sibling workers and
//!   reports a fallback: the caller discards the aborted plan's output
//!   (the partial is truncated to zero) and re-fetches the resource
//!   sequentially in the same attempt.
//! - **One failed segment can never masquerade as a completed file.** The
//!   record's rows move `Pending → Downloading → Completed` per worker; a
//!   worker only claims `Completed` after its bytes are synced, and the
//!   engine only finalizes when every planned worker completed and the
//!   partial file holds exactly the expected total.
//! - **Bounded by construction.** Workers are one task per planned span
//!   and the plan never exceeds the configured maximum, so a file's
//!   concurrent transfers stay within its per-file limit, and the batch
//!   layer's file-level limit bounds the global product
//!   (`DOCS/architecture.md` §6: multiplying file and segment settings must
//!   never result in unbounded work).
//! - **Shutdown is honored at every boundary.** A tripped shutdown makes
//!   each worker sync and persist its confirmed progress, and the caller
//!   parks the record `paused` — the same trust-preserving park as the
//!   sequential engine (ADR-0019).
//!
//! Crash-consistent segment recovery — reusing already-completed segments
//! after a process or system interruption — is Phase 9's work
//! (`DOCS/phases.md`); this phase persists per-segment state but always
//! restarts or reconciles conservatively.
//!
//! Phase 8 wiring note: commands reach this module through
//! [`crate::download::resume::transfer_persisted`] from Phase 11; until
//! then it carries a narrow `dead_code` waiver per `AGENTS.md`.

#![allow(dead_code)] // consumed by the resume engine; commands arrive in Phase 11

use std::path::PathBuf;
use std::sync::Arc;

use reqwest::{Method, StatusCode, Url};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::config::AppConfig;
use crate::download::http::{http_status_error, HttpClient, HttpFailure};
use crate::download::probe::{
    parse_content_range, response_meta, ContentEncoding, ContentLength, ProbeResult, RangeSupport,
};
use crate::download::progress::Progress;
use crate::download::resume::ShutdownHandle;
use crate::error::{AppError, ErrorCategory};
use crate::storage::file_writer::PartialFile;
use crate::storage::repository::{Repository, SegmentState};

/// Minimum expected size for a segmented transfer (`DOCS/architecture.md`
/// §6 starting value: 32 MiB). Files below it stream sequentially: the
/// added connections are unlikely to pay for themselves.
pub(crate) const MIN_FILE_SIZE: u64 = 32 * 1024 * 1024;

/// Nominal segment size (`DOCS/architecture.md` §6 starting value:
/// approximately 8 MiB). The real spans can be larger when the configured
/// maximum segment count clamps the preferred count.
pub(crate) const SEGMENT_SIZE: u64 = 8 * 1024 * 1024;

/// Confirmed-progress persistence interval for one segment worker. A span
/// shorter than this confirms once, at its end; the interval mirrors the
/// sequential engine's cadence so a crash never deconfirms more than the
/// same bounded window.
const PROGRESS_CONFIRM_INTERVAL_BYTES: u64 = 1_048_576;

/// The segmentation configuration: a size threshold, a nominal span size,
/// and the per-file worker maximum.
///
/// The shape is fixed; the values start from `DOCS/architecture.md` §6 and
/// are only tuned with benchmark evidence. [`Self::from_config`] maps the
/// user-facing configuration onto it (Phase 11 wires the CLI; the mapping
/// already lives here so the default and the configured value cannot
/// drift).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SegmentationPolicy {
    /// Smallest expected size that may be segmented.
    pub(crate) min_file_size: u64,
    /// Nominal byte size of one segment.
    pub(crate) segment_size: u64,
    /// Maximum number of segments (and worker tasks) per file.
    pub(crate) max_segments: u32,
}

impl Default for SegmentationPolicy {
    fn default() -> Self {
        Self {
            min_file_size: MIN_FILE_SIZE,
            segment_size: SEGMENT_SIZE,
            max_segments: 4,
        }
    }
}

impl SegmentationPolicy {
    /// Builds the segmentation policy from the user-facing configuration.
    ///
    /// Only the per-file segment maximum is user-configurable today
    /// (`--segments`); the threshold and nominal size are architecture
    /// starting values until a benchmark phase tunes them.
    pub(crate) fn from_config(config: &AppConfig) -> Self {
        Self {
            max_segments: config.max_segments_per_file,
            ..Self::default()
        }
    }

    /// Plans the segmentation of a probed resource, or `None` when the
    /// probe's evidence does not support it.
    ///
    /// The evidence gate (`DOCS/phases.md` Phase 8 step 1): byte-range
    /// support must have been *proven* (`RangeSupport::Yes`), the expected
    /// size must be known, and the derived plan must hold at least two
    /// spans. Everything else is a sequential transfer.
    pub(crate) fn plan_for_probe(&self, facts: &ProbeResult) -> Option<(u64, Vec<SegmentSpan>)> {
        if facts.range != RangeSupport::Yes {
            return None;
        }
        let total = facts.expected_size?;
        let spans = self.plan(total)?;
        Some((total, spans))
    }

    /// Plans the byte ranges for one resource of `total` bytes, or `None`
    /// when segmentation must not engage (below the minimum size, fewer
    /// than two segments, or a degenerate zero segment size).
    ///
    /// The plan covers `[0, total - 1]` exactly once with contiguous,
    /// non-overlapping inclusive spans and never more than
    /// [`Self::max_segments`] of them:
    ///
    /// ```text
    /// preferred = ceil(total / segment_size)        // the natural count
    /// span      = max(segment_size, ceil(total / max_segments))
    /// ```
    ///
    /// When `preferred <= max_segments` the spans are `segment_size` bytes
    /// with the remainder in a final short segment; otherwise `span` grows
    /// so that `ceil(total / span) <= max_segments`, and the remainder of
    /// that division again lands in the final short segment. Counting is
    /// exact: `ceil(total / span) <= max_segments` holds because
    /// `span >= ceil(total / max_segments)`.
    pub(crate) fn plan(&self, total: u64) -> Option<Vec<SegmentSpan>> {
        if total == 0 || total < self.min_file_size || self.segment_size == 0 {
            return None;
        }
        if self.max_segments < 2 {
            // One "segment" is a sequential transfer; segmentation needs at
            // least two independent ranges to exist at all.
            return None;
        }
        let preferred = total.div_ceil(self.segment_size);
        if preferred < 2 {
            return None;
        }
        let max = u64::from(self.max_segments);
        let spread = total.div_ceil(max);
        let span = self.segment_size.max(spread);
        let capacity = usize::try_from(preferred.min(max)).unwrap_or(0);
        let mut spans = Vec::with_capacity(capacity);
        let mut start = 0u64;
        while start < total {
            // The saturating extension keeps the arithmetic total at the
            // u64 edge: when start + span would overflow, the span is
            // clamped to the object's end (which is representable).
            let end = start.saturating_add(span).min(total) - 1;
            spans.push(SegmentSpan {
                min_byte: start,
                max_byte: end,
            });
            start = end + 1;
        }
        debug_assert!(
            spans.len() <= usize::try_from(self.max_segments).unwrap_or(usize::MAX),
            "the planner must never exceed the configured segment maximum"
        );
        Some(spans)
    }
}

/// One planned inclusive byte range, already validated for ordering and
/// overflow by [`SegmentationPolicy::plan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SegmentSpan {
    pub(crate) min_byte: u64,
    pub(crate) max_byte: u64,
}

impl SegmentSpan {
    /// The inclusive length of the span; the planner guarantees `max >=
    /// min`, so the arithmetic cannot underflow.
    pub(crate) fn len(self) -> u64 {
        self.max_byte - self.min_byte + 1
    }

    /// The exact `Range` header value for this span.
    pub(crate) fn range_header(self) -> String {
        format!("bytes={}-{}", self.min_byte, self.max_byte)
    }
}

/// One planned span bound to its persisted segment row.
#[derive(Debug)]
pub(crate) struct PlannedSegment {
    pub(crate) id: i64,
    pub(crate) span: SegmentSpan,
}

/// Everything the segment workers of one file share, built once per
/// segmented attempt and cloned (through one `Arc`) into every worker task.
///
/// The engine owns the repository handle, the partial file's lifecycle, and
/// the plan; a worker owns only its byte range and its independent file
/// handle. Workers are spawned tasks, so the shared context is `'static`
/// by construction.
pub(crate) struct SegmentContext {
    pub(crate) client: HttpClient,
    pub(crate) repo: Arc<Repository>,
    pub(crate) effective_url: Url,
    /// Redact-safe rendering of the effective URL for diagnostics.
    pub(crate) redacted: String,
    /// The validated representation total every span is a slice of.
    pub(crate) total: u64,
    /// The engine-owned partial file, created or truncated to zero by the
    /// caller before any worker starts.
    pub(crate) partial_path: PathBuf,
    pub(crate) progress: Arc<Progress>,
    pub(crate) shutdown: ShutdownHandle,
}

/// What the worker pool as a whole ended with.
#[derive(Debug)]
pub(crate) enum SegmentBatchEnd {
    /// Every worker wrote and confirmed its full range.
    Completed,
    /// The server behaved in a way that makes ranges unsafe for this
    /// resource (ignored, malformed, contradicted); the caller must
    /// discard the aborted output and fall back to a sequential transfer.
    Fallback { reason: &'static str },
    /// Shutdown was honored: every worker persisted its confirmed progress,
    /// and the caller parks the record `paused`.
    Interrupted,
}

/// Runs one worker per planned segment and aggregates their endings.
///
/// The pool is the plan itself: `planned.len()` tasks, which the planner
/// bounded by the configured per-file maximum. The first worker failure (or
/// fallback) trips a cancellation channel as a courtesy signal — siblings
/// stop at their next boundary, keep their confirmed progress, and are
/// joined before this function returns, so no task outlives its caller.
pub(crate) async fn download_segments(
    context: SegmentContext,
    planned: Vec<PlannedSegment>,
) -> Result<SegmentBatchEnd, AppError> {
    let context = Arc::new(context);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mut workers = JoinSet::new();
    for planned in planned {
        workers.spawn(segment_worker(
            Arc::clone(&context),
            planned,
            cancel_rx.clone(),
        ));
    }
    drop(cancel_rx);
    let mut fallback: Option<&'static str> = None;
    let mut first_error: Option<AppError> = None;
    let mut interrupted = false;
    while let Some(joined) = workers.join_next().await {
        match joined {
            Ok(Ok(WorkerEnd::Completed | WorkerEnd::Abandoned)) => {}
            Ok(Ok(WorkerEnd::Interrupted)) => interrupted = true,
            Ok(Ok(WorkerEnd::Fallback { reason })) => {
                let _ = cancel_tx.send(true);
                fallback.get_or_insert(reason);
            }
            Ok(Err(err)) => {
                let _ = cancel_tx.send(true);
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
            Err(join_err) => {
                let _ = cancel_tx.send(true);
                if first_error.is_none() {
                    // The retry taxonomy has no internal category; a panic
                    // in a worker is a bug, reported as a database-layer
                    // failure like the batch scheduler does (ADR-0020).
                    first_error = Some(AppError::new(
                        ErrorCategory::Database,
                        format!(
                            "a segment worker task failed unexpectedly (this is a bug): \
                             {join_err}"
                        ),
                    ));
                }
            }
        }
    }
    // Shutdown outranks the rest: parking is the correct resting state for
    // whatever the workers managed to confirm.
    if interrupted {
        return Ok(SegmentBatchEnd::Interrupted);
    }
    if let Some(reason) = fallback {
        return Ok(SegmentBatchEnd::Fallback { reason });
    }
    if let Some(err) = first_error {
        return Err(err);
    }
    Ok(SegmentBatchEnd::Completed)
}

/// How one worker ended.
enum WorkerEnd {
    /// The full span is written, synced, and recorded `Completed`.
    Completed,
    /// Shutdown was honored: confirmed progress is synced and recorded.
    Interrupted,
    /// A sibling failed or chose the fallback: confirmed progress is synced
    /// and recorded; this worker's result does not decide the outcome.
    Abandoned,
    /// This worker saw range behavior that makes segmentation unsafe; the
    /// whole pool falls back to a sequential transfer.
    Fallback { reason: &'static str },
}

/// Fetches one planned span into the partial file at its exact offset.
///
/// The worker owns one independent file handle positioned once at the span
/// start (never a shared cursor) and refuses to write a single byte outside
/// the span; the row moves to `Downloading` before the body streams and to
/// `Completed` only after the full span is synced.
async fn segment_worker(
    context: Arc<SegmentContext>,
    planned: PlannedSegment,
    mut cancel: watch::Receiver<bool>,
) -> Result<WorkerEnd, AppError> {
    let span = planned.span;
    let range_value = span.range_header();
    tracing::debug!(
        "segmented transfer of {}: segment {} of the record fetches {range_value}",
        context.redacted,
        planned.id,
    );

    let mut response = context
        .client
        .send(Method::GET, &context.effective_url, Some(&range_value))
        .await?;
    let status = response.status();
    match status {
        StatusCode::PARTIAL_CONTENT => {}
        // The server ignored the range and answered with the full
        // representation: nothing here can be written as a segment.
        StatusCode::OK => {
            return Ok(WorkerEnd::Fallback {
                reason: "the server answered a planned byte range with the full representation",
            })
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            return Ok(WorkerEnd::Fallback {
                reason: "the server answered a planned byte range as unsatisfiable (416)",
            })
        }
        _ => {
            return Err(http_status_error(
                status,
                format!(
                    "the remote server responded HTTP {status} for {} (segment \
                     {range_value})",
                    context.redacted
                ),
            ))
        }
    }

    // The answer must be byte-anchored to the planned interval: an exact,
    // parseable `Content-Range`, an agreeing total when stated, and a
    // `Content-Length` that reconciles with the interval. Any deviation
    // means this server's range behavior cannot be trusted for the
    // resource; the pool falls back to a sequential transfer.
    let spec = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_content_range);
    let Some(spec) = spec else {
        return Ok(WorkerEnd::Fallback {
            reason: "the ranged answer carried no usable Content-Range header",
        });
    };
    if spec.first != span.min_byte || spec.last != span.max_byte {
        return Ok(WorkerEnd::Fallback {
            reason: "the ranged answer does not cover the planned byte range exactly",
        });
    }
    if let Some(total) = spec.total {
        if total != context.total {
            return Ok(WorkerEnd::Fallback {
                reason: "the representation's total size no longer matches the planned transfer",
            });
        }
    }
    // One header parse, one trust policy: a content transformation can
    // never be written as the resource's bytes (a hard refusal, exactly as
    // the sequential engine treats it).
    let meta = response_meta(&response);
    if meta.encoding == ContentEncoding::Transform {
        return Err(AppError::new(
            ErrorCategory::RangeValidation,
            format!(
                "the transfer of {} declared a content transformation \
                 (Content-Encoding), which cannot be written as the resource's bytes; \
                 the transfer is refused",
                context.redacted
            ),
        ));
    }
    if let ContentLength::Valid(len) = meta.cl {
        if len != span.len() {
            return Ok(WorkerEnd::Fallback {
                reason: "the ranged answer's Content-Length contradicts its Content-Range",
            });
        }
    }

    // The answer passed every gate: this worker owns the span now.
    context
        .repo
        .update_segment(planned.id, SegmentState::Downloading, 0)?;
    let mut partial = PartialFile::open_for_segment(&context.partial_path, span.min_byte).await?;

    let span_len = span.len();
    let mut written: u64 = 0;
    let mut last_confirmed: u64 = 0;
    loop {
        if context.shutdown.tripped() || *cancel.borrow() {
            let shutdown = context.shutdown.tripped();
            return stop_worker(&context, planned.id, &mut partial, written, shutdown).await;
        }
        let chunk = tokio::select! {
            chunk = context.client.read_chunk(&mut response) => chunk?,
            () = context.shutdown.wait() => {
                return stop_worker(&context, planned.id, &mut partial, written, true).await;
            }
            _ = cancel.changed() => {
                return stop_worker(&context, planned.id, &mut partial, written, false).await;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let size = chunk.len() as u64;
        if size == 0 {
            continue;
        }
        let remaining = span_len - written;
        if size > remaining {
            // The body overran its promised range: the whole chunk is
            // refused (a straddling chunk must not partially land), the
            // confirmed prefix is kept durable, and the attempt fails as a
            // retryable length violation — never as a completed file.
            sync_claim(&mut partial, &context.repo, planned.id, written).await?;
            return Err(overrun_error(&context.redacted, &range_value, span_len));
        }
        context.progress.record_received(size);
        let written_now = partial.write_chunk(chunk.as_ref()).await?;
        written += written_now;
        context.progress.record_written(written_now);
        if written - last_confirmed >= PROGRESS_CONFIRM_INTERVAL_BYTES {
            sync_claim(&mut partial, &context.repo, planned.id, written).await?;
            last_confirmed = written;
        }
    }

    if written != span_len {
        sync_claim(&mut partial, &context.repo, planned.id, written).await?;
        return Err(premature_end_error(
            written,
            span_len,
            &context.redacted,
            &range_value,
        ));
    }
    partial.sync().await?;
    context
        .repo
        .update_segment(planned.id, SegmentState::Completed, span_len)?;
    tracing::debug!(
        "segmented transfer of {}: segment {} completed {span_len} bytes at byte {}",
        context.redacted,
        planned.id,
        span.min_byte,
    );
    Ok(WorkerEnd::Completed)
}

/// The shutdown/abandonment boundary of a worker: sync, claim the confirmed
/// bytes, and report which externality stopped it.
async fn stop_worker(
    context: &SegmentContext,
    segment_id: i64,
    partial: &mut PartialFile,
    written: u64,
    shutdown: bool,
) -> Result<WorkerEnd, AppError> {
    sync_claim(partial, &context.repo, segment_id, written).await?;
    if shutdown {
        Ok(WorkerEnd::Interrupted)
    } else {
        Ok(WorkerEnd::Abandoned)
    }
}

/// Syncs the worker's bytes and records the confirmed count on its row.
async fn sync_claim(
    partial: &mut PartialFile,
    repo: &Repository,
    segment_id: i64,
    written: u64,
) -> Result<(), AppError> {
    partial.sync().await?;
    repo.update_segment(segment_id, SegmentState::Downloading, written)
}

/// A body that streamed past its validated `Content-Range` interval is not
/// file content; the hidden marker classifies the failure as transient, so
/// a bounded retry may re-fetch the span (ADR-0019's policy shape).
fn overrun_error(redacted: &str, range_value: &str, span_len: u64) -> AppError {
    AppError::new(
        ErrorCategory::RangeValidation,
        format!(
            "the ranged answer {range_value} of {redacted} streamed more than the {span_len} \
             bytes its Content-Range promised; the overrun was not written and the transfer \
             will not finalize"
        ),
    )
    .with_source(HttpFailure::transport())
}

/// A span that ended before its promised length is incomplete; like the
/// sequential engine's premature end, it is retryable transport behavior.
fn premature_end_error(written: u64, span_len: u64, redacted: &str, range_value: &str) -> AppError {
    AppError::new(
        ErrorCategory::RangeValidation,
        format!(
            "the ranged answer {range_value} of {redacted} ended after {written} of the \
             expected {span_len} bytes; the file is not complete and will not be finalized"
        ),
    )
    .with_source(HttpFailure::transport())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(min_file_size: u64, segment_size: u64, max_segments: u32) -> SegmentationPolicy {
        SegmentationPolicy {
            min_file_size,
            segment_size,
            max_segments,
        }
    }

    /// Asserts that a plan starts at 0, ends at `total - 1`, is contiguous,
    /// and covers the object exactly once.
    fn assert_exact_cover(spans: &[SegmentSpan], total: u64) {
        assert!(!spans.is_empty(), "a plan always has at least one span");
        assert_eq!(spans[0].min_byte, 0, "the plan starts at byte 0");
        assert_eq!(
            spans.last().expect("non-empty").max_byte,
            total - 1,
            "the plan ends at the object's last byte"
        );
        let mut next = 0u64;
        let mut covered = 0u64;
        for span in spans {
            assert!(span.max_byte >= span.min_byte, "a span is never inverted");
            assert_eq!(span.min_byte, next, "spans are contiguous, no gaps");
            covered = covered
                .checked_add(span.len())
                .expect("the coverage sum never overflows");
            next = span.max_byte + 1;
        }
        assert_eq!(covered, total, "the plan covers the object exactly once");
    }

    fn probe_facts(range: RangeSupport, expected_size: Option<u64>) -> ProbeResult {
        ProbeResult {
            expected_size,
            range,
            etag: None,
            last_modified: None,
            effective_url: "http://mock.test/segmented.bin".to_owned(),
        }
    }

    #[test]
    fn defaults_follow_the_architecture_starting_values() {
        let defaults = SegmentationPolicy::default();
        assert_eq!(defaults.min_file_size, 32 * 1024 * 1024);
        assert_eq!(defaults.segment_size, 8 * 1024 * 1024);
        assert_eq!(defaults.max_segments, 4);
        assert_eq!(
            SegmentationPolicy::from_config(&AppConfig::default()),
            defaults,
            "the default config's segment maximum maps onto the default policy"
        );
    }

    #[test]
    fn the_configured_segment_maximum_reaches_the_policy() {
        let config = AppConfig {
            max_segments_per_file: 7,
            ..AppConfig::default()
        };
        let mapped = SegmentationPolicy::from_config(&config);
        assert_eq!(mapped.max_segments, 7);
        assert_eq!(
            mapped.min_file_size,
            SegmentationPolicy::default().min_file_size
        );
    }

    #[test]
    fn the_probe_gate_requires_proven_ranges_and_a_known_sizable_body() {
        let policy = policy(1, 512, 4);
        assert!(
            policy
                .plan_for_probe(&probe_facts(RangeSupport::Yes, Some(4_096)))
                .is_some(),
            "proven ranges plus a known size that plans two or more spans"
        );
        assert!(
            policy
                .plan_for_probe(&probe_facts(RangeSupport::No, Some(4_096)))
                .is_none(),
            "an ignored range must never become a plan"
        );
        assert!(
            policy
                .plan_for_probe(&probe_facts(RangeSupport::Unknown, Some(4_096)))
                .is_none(),
            "inconclusive range evidence must never become a plan"
        );
        assert!(
            policy
                .plan_for_probe(&probe_facts(RangeSupport::Yes, None))
                .is_none(),
            "a plan needs a known size to cover"
        );
        assert!(
            policy
                .plan_for_probe(&probe_facts(RangeSupport::Yes, Some(500)))
                .is_none(),
            "a one-span plan is not segmentation"
        );
        let (total, spans) = policy
            .plan_for_probe(&probe_facts(RangeSupport::Yes, Some(4_096)))
            .expect("eligible");
        assert_eq!(total, 4_096);
        assert_exact_cover(&spans, total);
    }

    #[test]
    fn files_below_the_minimum_size_are_never_segmented() {
        let policy = policy(1_024, 512, 4);
        assert!(
            policy.plan(1_023).is_none(),
            "one byte below the threshold stays sequential"
        );
        assert!(
            policy.plan(1_024).is_some(),
            "the threshold itself is eligible"
        );
    }

    #[test]
    fn a_one_segment_plan_is_not_segmentation() {
        let nominal = policy(1, 2_000, 4);
        assert!(
            nominal.plan(1_000).is_none(),
            "a file smaller than one nominal segment streams sequentially"
        );
        assert!(
            policy(1, 500, 4).plan(1_000).is_some(),
            "the same file with a smaller nominal size splits in two"
        );
    }

    #[test]
    fn segment_counts_of_two_three_and_the_maximum_cover_the_object_exactly() {
        let total = 12_288u64;
        let cases = [
            (6_144u64, 2usize), // preferred = 2
            (4_096, 3),         // preferred = 3
            (3_072, 4),         // preferred = 4 = the configured maximum
        ];
        for (segment_size, expected_count) in cases {
            let spans = policy(1, segment_size, 4)
                .plan(total)
                .expect("eligible for segmentation");
            assert_eq!(
                spans.len(),
                expected_count,
                "segment_size {segment_size} must prefer {expected_count} spans"
            );
            assert_exact_cover(&spans, total);
        }
    }

    #[test]
    fn non_divisible_sizes_end_in_a_short_final_segment() {
        let total = 5_000u64;
        let spans = policy(1, 2_048, 4)
            .plan(total)
            .expect("eligible for segmentation");
        assert_eq!(spans.len(), 3, "ceil(5000 / 2048) = 3");
        assert_eq!(spans[0].len(), 2_048);
        assert_eq!(spans[1].len(), 2_048);
        assert_eq!(spans[2].len(), 904, "the remainder lands in the final span");
        assert_exact_cover(&spans, total);
    }

    #[test]
    fn a_plan_never_exceeds_the_configured_maximum() {
        // 100 MiB at ~8 MiB per segment would prefer 13 segments; the
        // clamp must merge them into the configured maximum instead.
        let total = 100 * 1024 * 1024u64;
        let spans = policy(1, 8 * 1024 * 1024, 4)
            .plan(total)
            .expect("eligible for segmentation");
        assert_eq!(spans.len(), 4);
        assert_exact_cover(&spans, total);

        // An awkward total exercises the clamped distribution: the clamped
        // span keeps dividing evenly until the remainder lands in the final
        // short segment.
        let total = 101u64;
        let spans = policy(1, 8, 4).plan(total).expect("eligible");
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].len(), 26);
        assert_eq!(spans[3].len(), 23);
        assert_exact_cover(&spans, total);
    }

    #[test]
    fn every_maximum_stays_within_its_bound_and_covers_the_object() {
        for max_segments in [2u32, 3, 4, 7] {
            for total in [33u64, 1_000, 1_000_000, 1 << 40] {
                let spans = policy(1, 16, max_segments).plan(total).unwrap_or_else(|| {
                    panic!("total {total} with max {max_segments} must segment")
                });
                assert!(
                    spans.len() <= usize::try_from(max_segments).expect("small maximum"),
                    "max {max_segments} produced {} spans for {total}",
                    spans.len()
                );
                assert_exact_cover(&spans, total);
            }
        }
    }

    #[test]
    fn overflow_boundaries_still_cover_the_whole_object() {
        // The largest representable object with a ~4 EiB nominal segment:
        // the final span's extension would overflow u64 and must saturate
        // into an exact end instead.
        let total = u64::MAX;
        let spans = policy(1, 1 << 62, 4)
            .plan(total)
            .expect("the object may be segmented");
        assert_eq!(spans.len(), 4);
        assert_exact_cover(&spans, total);

        // A nominal size that does not divide the preferred count either.
        let spans = policy(1, (1 << 62) + 1, 4)
            .plan(total)
            .expect("the object may be segmented");
        assert!(spans.len() <= 4);
        assert_exact_cover(&spans, total);
    }

    #[test]
    fn degenerate_policies_segment_nothing() {
        assert!(
            policy(1, 0, 4).plan(1_000_000).is_none(),
            "a zero segment size cannot produce spans"
        );
        assert!(
            policy(1, 512, 1).plan(1_000_000).is_none(),
            "a maximum of one means sequential"
        );
        assert!(
            policy(1, 512, 0).plan(1_000_000).is_none(),
            "a maximum of zero means sequential"
        );
        assert!(
            policy(0, 512, 4).plan(0).is_none(),
            "an empty object has nothing to segment"
        );
        assert!(
            policy(0, 1, 4).plan(1).is_none(),
            "a one-byte object cannot have two non-empty ranges"
        );
    }

    #[test]
    fn span_headers_and_lengths_are_exact() {
        let span = SegmentSpan {
            min_byte: 1_024,
            max_byte: 2_047,
        };
        assert_eq!(span.len(), 1_024);
        assert_eq!(span.range_header(), "bytes=1024-2047");
        let single = SegmentSpan {
            min_byte: 0,
            max_byte: 0,
        };
        assert_eq!(single.len(), 1);
        assert_eq!(single.range_header(), "bytes=0-0");
    }
}
