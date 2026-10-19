//! Internal transfer progress reporting (Phase 4).
//!
//! The engine reports byte accounting to this shared counter while it
//! streams; tests read it to prove streaming behavior (for example that no
//! single write ever covered a whole body, and that received-versus-written
//! accounting stays exact). Phase 11's CLI progress display reads the same
//! values live over the transfer registry below.
//!
//! `largest_single_write` keeps a narrow waiver: it is a diagnostic metric
//! observed by the streaming tests (`AGENTS.md` narrow-waiver pattern); no
//! production caller emerged, and the file-level waiver from Phase 4 came
//! off when the CLI started consuming `received`/`written`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Wait-free byte counters of one transfer.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    bytes_received: AtomicU64,
    bytes_written: AtomicU64,
    largest_single_write: AtomicU64,
}

impl Progress {
    /// A counter set shared between the engine and its observers.
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records that `bytes` arrived from the network.
    pub(crate) fn record_received(&self, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Records that `bytes` completed a write to the partial file.
    pub(crate) fn record_written(&self, bytes: u64) {
        self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
        self.largest_single_write
            .fetch_max(bytes, Ordering::Relaxed);
    }

    /// Bytes received from the network so far.
    #[must_use]
    pub(crate) fn received(&self) -> u64 {
        self.bytes_received.load(Ordering::Relaxed)
    }

    /// Bytes written to the partial file so far.
    ///
    /// Diagnostic metric of the streaming tests (received-versus-written
    /// accounting); the CLI consumes `received`, so the narrow waiver
    /// names its observers instead of widening to the whole file.
    #[allow(dead_code)] // streaming tests observe it; no production caller yet
    #[must_use]
    pub(crate) fn written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    /// The largest single write handed to the partial file so far.
    ///
    /// Diagnostic metric of the streaming tests (bounded-buffer evidence);
    /// no production caller yet — the narrow Phase 4 waiver stays.
    #[allow(dead_code)] // streaming tests observe it; produced by record_written
    #[must_use]
    pub(crate) fn largest_single_write(&self) -> u64 {
        self.largest_single_write.load(Ordering::Relaxed)
    }

    /// Continues one transfer at `offset` reflected bytes (Phase 6 resume).
    ///
    /// The byte counters start where the transfer left off so final
    /// accounting covers the *whole* transfer and the diagnostics of a
    /// resumed, interrupted session stay truthful. The largest-single-write
    /// metric is deliberately *not* seeded: it is a fact about this
    /// invocation's own writes, and the files already on disk wrote them
    /// in earlier sessions.
    pub(crate) fn continue_from(&self, offset: u64) {
        self.bytes_received.store(offset, Ordering::Relaxed);
        self.bytes_written.store(offset, Ordering::Relaxed);
    }
}

/// Identifies one live transfer: the normalized URL and the output
/// directory it runs against (the engine's own `(url, output_dir)` scope,
/// so the same URL in two directories displays separately and a completed
/// one is never shadowed by a rerun).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TransferKey {
    pub(crate) url: String,
    pub(crate) directory: PathBuf,
}

impl TransferKey {
    /// Builds a key from the work item's scope pieces.
    pub(crate) fn new(url: impl Into<String>, directory: PathBuf) -> Self {
        Self {
            url: url.into(),
            directory,
        }
    }
}

/// The live-transfer registry the command layer reads for progress display.
///
/// Workers publish their `Arc<Progress>` when a transfer begins and remove
/// it when the attempt ends (any outcome), so a snapshot always describes
/// exactly the transfers whose bytes are currently arriving. Entries are
/// bounded by the scheduler's own concurrency limit — publishing is just a
/// map insert performed by the bounded pool's workers, never by input URLs.
///
/// The mutex is `std::sync`: these are short non-async critical sections
/// (map insert/remove/cloning handles under a lock that is never held
/// across an await).
#[derive(Debug, Default)]
pub(crate) struct TransferRegistry {
    entries: Mutex<HashMap<TransferKey, Arc<Progress>>>,
}

impl TransferRegistry {
    /// A fresh empty registry, ready to be shared (`Arc`) by command and
    /// scheduler.
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers one live transfer, publishing its progress handle.
    pub(crate) fn publish(&self, key: TransferKey, progress: Arc<Progress>) {
        self.entries
            .lock()
            .expect("progress registry lock")
            .insert(key, progress);
    }

    /// Removes one live transfer (called whatever the outcome was).
    pub(crate) fn remove(&self, key: &TransferKey) {
        self.entries
            .lock()
            .expect("progress registry lock")
            .remove(key);
    }

    /// Snapshot of the live transfers at this instant.
    pub(crate) fn snapshot(&self) -> Vec<(TransferKey, Arc<Progress>)> {
        self.entries
            .lock()
            .expect("progress registry lock")
            .iter()
            .map(|(key, progress)| (key.clone(), Arc::clone(progress)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Progress, TransferKey, TransferRegistry};
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn counters_accumulate_and_the_maximum_only_grows() {
        let progress = Progress::shared();
        progress.record_received(512);
        progress.record_received(4_096);
        progress.record_written(512);
        assert_eq!(progress.received(), 4_608);
        assert_eq!(progress.written(), 512);
        assert_eq!(progress.largest_single_write(), 512);
        progress.record_written(4_096);
        assert_eq!(progress.largest_single_write(), 4_096);
        progress.record_written(8);
        assert_eq!(
            progress.largest_single_write(),
            4_096,
            "a smaller later write never lowers the maximum"
        );
    }

    #[test]
    fn a_continued_transfer_counts_its_offset_without_inflating_this_invocation() {
        let progress = Progress::shared();
        progress.continue_from(4_096);
        assert_eq!(progress.received(), 4_096);
        assert_eq!(progress.written(), 4_096);
        assert_eq!(
            progress.largest_single_write(),
            0,
            "the resumed prefix was written by a previous session, not this one"
        );

        progress.record_written(512);
        progress.record_received(512);
        assert_eq!(progress.written(), 4_608);
        assert_eq!(progress.received(), 4_608);
        assert_eq!(progress.largest_single_write(), 512);
    }

    #[test]
    fn the_registry_reflects_publish_remove_and_snapshot() {
        let registry = TransferRegistry::shared();
        let one = TransferKey::new("https://host/one.bin", PathBuf::from("out"));
        let two = TransferKey::new("https://host/two.bin", PathBuf::from("out"));

        let progress_one = Progress::shared();
        let progress_two = Progress::shared();
        registry.publish(one.clone(), Arc::clone(&progress_one));
        registry.publish(two.clone(), Arc::clone(&progress_two));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 2, "both live transfers are visible");

        progress_one.record_received(256);
        let lookup = |registry: &TransferRegistry, key: &TransferKey| {
            registry
                .snapshot()
                .into_iter()
                .find(|(entry, _)| entry == key)
                .expect("key present")
                .1
        };
        assert_eq!(lookup(&registry, &one).received(), 256);

        registry.remove(&one);
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0, two, "only two.bin remains live");

        registry.remove(&two);
        assert!(registry.snapshot().is_empty(), "removals are exact");
    }
}
