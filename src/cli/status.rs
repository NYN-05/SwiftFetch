//! Status listing for the CLI (Phase 11).
//!
//! `swift-fetch status` reads the persisted queue and renders one line per
//! record with the lifecycle state, the recorded file name, the byte
//! progress that *stored state* actually supports, and the redacted URL.
//! A download's completion and its ZIP extraction's outcome are separate
//! facts (architecture §7): an extraction failure is appended to the
//! completed download's line instead of flipping its state.
//!
//! The stored facts rendered here:
//! - `expected_size` — recorded probe facts (`None` = unknown);
//! - confirmed progress — the newest `min_byte = 0` non-`failed` segment
//!   row's confirmed byte claim (the same definition the resume trust rule
//!   uses; the database is the durable source here, not a live stream);
//! - a failed record's error category;
//! - a failed extraction record for a completed download.
//!
//! Percentages appear only for a known expected size (honest unknowns); a
//! completed download displays its total rather than a recomputed percent.

use crate::cli::progress::format_bytes;
use crate::logging::redact_url;

/// One record's display-ready facts, assembled by the application layer
/// from the repository (plain data; this module does no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusRow {
    pub(crate) id: i64,
    /// Lifecycle state text (the stable kebab-case stored form).
    pub(crate) state: &'static str,
    pub(crate) file_name: String,
    /// The record's URL, redacted at render time.
    pub(crate) url: String,
    /// Expected byte size from recorded probe facts, when known.
    pub(crate) expected_size: Option<u64>,
    /// Confirmed byte progress from stored segment rows, when stored.
    pub(crate) confirmed: Option<u64>,
    /// Stored error category for failed records ("input", "http", …).
    pub(crate) error_category: Option<&'static str>,
    /// Stored error category of the last failed extraction of a completed
    /// download, when it failed.
    pub(crate) extraction_failed: Option<&'static str>,
}

/// Renders the whole listing: a headline with the record count, then one
/// line per record in the given (insertion) order. An empty queue renders
/// a single honest sentence.
#[must_use]
pub fn status_lines(rows: &[StatusRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["SwiftFetch has no downloads on record.".to_owned()];
    }
    let mut lines = vec![format!(
        "SwiftFetch has {} download(s) on record:",
        rows.len()
    )];
    lines.extend(rows.iter().map(status_line));
    lines
}

/// Renders one record.
#[must_use]
fn status_line(row: &StatusRow) -> String {
    let bytes = bytes_fragment(row);
    let mut line = format!(
        "  #{id} {state:<10} {file_name}  {bytes}  {url}",
        id = row.id,
        state = row.state,
        file_name = row.file_name,
        bytes = bytes,
        url = redact_url(&row.url),
    );
    if row.state == "failed" {
        match row.error_category {
            Some(category) => line.push_str(&format!("  (reason: {category})")),
            None => line.push_str("  (reason: none recorded)"),
        }
    }
    if let Some(category) = row.extraction_failed {
        line.push_str(&format!("  (extraction failed: {category})"));
    }
    line
}

/// The byte-progress fragment, faithful to what stored state claims.
///
/// - completed: the expected size (the finalized file's size) or an honest
///   "size unknown";
/// - everything else: confirmed bytes against the expected size with a
///   percent, or confirmed bytes alone when the size is unknown; a record
///   without stored progress shows `0 B`.
#[must_use]
fn bytes_fragment(row: &StatusRow) -> String {
    let confirmed = row.confirmed.unwrap_or(0);
    match row.state {
        "completed" => row.expected_size.map_or_else(
            || "(size unknown)".to_owned(),
            |expected| format!("{} total", format_bytes(expected)),
        ),
        _ => row.expected_size.map_or_else(
            || format!("{} (size unknown)", format_bytes(confirmed)),
            |expected| {
                let percent = confirmed.min(expected) * 100 / expected.max(1);
                format!(
                    "{} of {} ({percent}%)",
                    format_bytes(confirmed),
                    format_bytes(expected),
                )
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{status_lines, StatusRow};

    fn row(id: i64, state: &'static str) -> StatusRow {
        StatusRow {
            id,
            state,
            file_name: "file.bin".to_owned(),
            url: "http://127.0.0.1:9/file.bin?token=abc".to_owned(),
            expected_size: None,
            confirmed: None,
            error_category: None,
            extraction_failed: None,
        }
    }

    #[test]
    fn an_empty_queue_is_a_single_honest_sentence() {
        let lines = status_lines(&[]);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("no downloads on record"),
            "the empty queue says so plainly: {}",
            lines[0]
        );
    }

    #[test]
    fn the_headline_counts_the_records() {
        let lines = status_lines(&[row(1, "queued"), row(2, "completed")]);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("2 download(s) on record"), "{}", lines[0]);
    }

    #[test]
    fn a_queued_record_without_facts_is_honest() {
        let lines = status_lines(&[row(7, "queued")]);
        let line = &lines[1];
        assert!(line.contains("#7"), "{line}");
        assert!(line.contains("queued    "), "{line}");
        assert!(line.contains("file.bin"), "{line}");
        assert!(line.contains("0 B (size unknown)"), "{line}");
        assert!(!line.contains("token"), "display URLs are redacted: {line}");
        assert!(line.contains("http://127.0.0.1:9/file.bin"), "{line}");
    }

    #[test]
    fn a_paused_record_uses_stored_confirmed_progress() {
        let mut paused = row(12, "paused");
        paused.expected_size = Some(4_096);
        paused.confirmed = Some(2_048);
        let lines = status_lines(&[paused]);
        assert!(
            lines[1].contains("2.0 KiB of 4.0 KiB (50%)"),
            "confirmed/expected/percent render: {}",
            lines[1]
        );
    }

    #[test]
    fn progress_is_bounded_even_for_contradicting_stored_facts() {
        let mut paused = row(13, "paused");
        paused.expected_size = Some(4_096);
        paused.confirmed = Some(8_192);
        let lines = status_lines(&[paused]);
        assert!(
            lines[1].contains("(100%)"),
            "percent is capped although this state can only exist from a bug: {}",
            lines[1]
        );
    }

    #[test]
    fn a_failed_record_names_its_stored_category() {
        let mut failed = row(4, "failed");
        failed.error_category = Some("http");
        let lines = status_lines(&[failed.clone()]);
        assert!(
            lines[1].contains("(reason: http)"),
            "the failure reason is displayed: {}",
            lines[1]
        );

        failed.error_category = None;
        let lines = status_lines(&[failed]);
        assert!(
            lines[1].contains("(reason: none recorded)"),
            "a corrupt record is called out rather than guessed: {}",
            lines[1]
        );
    }

    #[test]
    fn a_completed_record_shows_its_total_not_a_percent() {
        let mut completed = row(5, "completed");
        completed.expected_size = Some(4_096);
        let lines = status_lines(&[completed.clone()]);
        assert!(
            lines[1].contains("4.0 KiB total"),
            "completed shows the real size: {} / {}",
            lines[0],
            lines[1]
        );
        assert!(!lines[1].contains('%'), "{}", lines[0]);

        completed.expected_size = None;
        let lines = status_lines(&[completed]);
        assert!(lines[1].contains("(size unknown)"), "{}", lines[1]);
    }

    #[test]
    fn an_extraction_failure_is_appended_not_substituted() {
        let mut completed = row(6, "completed");
        completed.expected_size = Some(2_048);
        completed.extraction_failed = Some("zip-extraction");
        let lines = status_lines(&[completed]);
        let line = &lines[1];
        assert!(
            line.contains("2.0 KiB total"),
            "the download stays complete: {line}"
        );
        assert!(
            line.contains("(extraction failed: zip-extraction)"),
            "the extraction failure is reported separately: {line}"
        );
    }
}
