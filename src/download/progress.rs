//! Internal transfer progress reporting (Phase 4).
//!
//! The engine reports byte accounting to this shared counter while it
//! streams; tests read it to prove streaming behavior (for example that no
//! single write ever covered a whole body, and that received-versus-written
//! accounting stays exact). A user-facing display will read the same values
//! in a later phase — `DOCS/phases.md` Phase 4 asks for progress
//! "information internally, without requiring a polished CLI display".
//!
//! Phase 4 wiring note: the values are produced by the not-yet-command-wired
//! engine, so `dead_code` is waived until the CLI consumes progress (the
//! narrow waiver pattern of `AGENTS.md`).

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
    #[must_use]
    pub(crate) fn written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    /// The largest single write handed to the partial file so far.
    #[must_use]
    pub(crate) fn largest_single_write(&self) -> u64 {
        self.largest_single_write.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::Progress;

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
}
