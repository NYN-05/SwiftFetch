//! Progress rendering for the CLI (Phase 11).
//!
//! The command layer reports live transfers at a controlled cadence
//! (architecture §6: "Progress refresh 4 updates per second"); this module
//! owns the pure rendering rules: byte/rate/ETA formatting, the rolling
//! window that turns observed byte deltas into a speed, and the per-line
//! wording that stays honest when a value is unavailable ("unknown size",
//! "remaining time unknown").
//!
//! Rendering is deliberately line-based and terminal-plain: no cursor
//! control, one line per active transfer per refresh sweep, and a sweep
//! suppresses a transfer whose byte count did not change since the last
//! line, so a stalled download never floods stdout ("avoid excessive
//! redraws", phases.md Phase 11).
//!
//! Honesty rules (`DOCS/phases.md` Phase 11):
//! - speed is calculated from observed byte throughput over a rolling
//!   interval — never inferred from configured limits;
//! - an unknown size displays as unknown; percentages and remaining-time
//!   estimates appear only when a size is actually known;
//! - values are bounded and non-negative (an overrun body is capped at
//!   100% — the transfer itself still fails validation, never finalizes).

use std::collections::HashMap;
use std::time::Duration;

use crate::download::progress::TransferKey;
use crate::logging::redact_url;

/// Cadence of the live progress sweep: 4 updates per second
/// (`DOCS/architecture.md` §6 starting value).
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

/// The rolling interval speed is measured over. Two sweet seconds are long
/// enough to smooth out per-second network wobble and short enough to track
/// actual changes; it is an observed-throughput window, not a config knob.
pub(crate) const ROLLING_WINDOW: Duration = Duration::from_secs(2);

/// Human text for a byte magnitude, binary units with one decimal.
///
/// Deterministic for every input (tests lock several); `0` renders `0 B`,
/// and magnitudes beyond the table saturate in exbibytes.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1_024 {
        return format!("{bytes} B");
    }
    // floor(log2(bytes)) selects the magnitude; dividing by 10 selects the
    // unit (0=KiB .. 6=EiB), saturating for u64::MAX.
    let exponent = 63 - u64::from(bytes.leading_zeros());
    let unit = ((exponent / 10) as usize).min(UNITS.len() - 1);
    let value = bytes as f64 / (1u64 << (10 * unit)) as f64;
    format!("{value:.1} {}", UNITS[unit])
}

/// Human text for an observed throughput, same units as [`format_bytes`].
///
/// Non-finite or non-positive speeds render `0 B/s`; the value stays
/// non-negative, and sub-byte rates honestly display as `0 B/s`.
#[must_use]
pub fn format_rate(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return "0 B/s".to_owned();
    }
    format!("{}/s", format_bytes(bytes_per_second as u64))
}

/// Text for a remaining-time estimate; `None` means the caller owns the
/// honest omission (no invented guess).
#[must_use]
pub fn format_eta(remaining: Option<Duration>) -> Option<String> {
    let remaining = remaining?;
    let total = remaining.as_secs();
    if total < 60 {
        return Some(format!("{total} s"));
    }
    let minutes = total / 60;
    let seconds = total % 60;
    if minutes < 60 {
        return Some(format!("{minutes} min {seconds} s"));
    }
    let hours = total / 3_600;
    let minutes = (total % 3_600) / 60;
    Some(format!("{hours} h {minutes} min"))
}

/// Derives a remaining-time estimate from the known expected size and the
/// observed speed. `None` honestly marks every unestimable case: unknown
/// size, no measured speed, a non-positive or non-finite speed, a
/// behind-the-position body, or an unrepresentable duration.
#[must_use]
pub fn estimate_eta(expected: u64, done: u64, speed: Option<f64>) -> Option<Duration> {
    let speed = speed?;
    if !speed.is_finite() || speed <= 0.0 {
        return None;
    }
    let remaining = expected.checked_sub(done)?;
    if remaining == 0 {
        return Some(Duration::from_secs(0));
    }
    let seconds = remaining as f64 / speed;
    Duration::try_from_secs_f64(seconds).ok()
}

/// The rolling byte-throughput window: speed from observed bytes only.
///
/// Samples older than the window are dropped on every sample, then the
/// speed is the byte delta between the window's oldest and newest sample
/// divided by the real elapsed time between them. Fewer than two samples,
/// a zero elapsed time, or a non-finite result produce `None` — never a
/// division by zero and never an inferred value.
#[derive(Debug, Default)]
pub struct ThroughputWindow {
    span: Duration,
    samples: Vec<(std::time::Instant, u64)>,
}

impl ThroughputWindow {
    /// A window over the requested span.
    #[must_use]
    pub const fn new(span: Duration) -> Self {
        Self {
            span,
            samples: Vec::new(),
        }
    }

    /// Records a byte position at a timestamp and returns the window's
    /// speed (bytes per second) when it is measurable.
    ///
    /// Byte positions come from the engine's monotonic atomic counter, so
    /// samples only move forward; a zero-width elapsed time or a lone
    /// sample yields `None` instead of a fabricated speed.
    pub fn sample(&mut self, at: std::time::Instant, bytes: u64) -> Option<f64> {
        self.samples.push((at, bytes));
        if let Some(horizon) = at.checked_sub(self.span) {
            self.samples.retain(|(time, _)| *time >= horizon);
        } else {
            // Instant's clock cannot represent the horizon (process-start
            // edge); nothing older than this sample is meaningful.
            let last = self.samples.pop();
            self.samples = last.into_iter().collect();
        }
        self.current_speed()
    }

    /// The current speed from the retained samples.
    fn current_speed(&self) -> Option<f64> {
        if self.samples.len() < 2 {
            return None;
        }
        let first = self.samples.first()?;
        let last = self.samples.last()?;
        let elapsed = last.0.checked_duration_since(first.0)?;
        if elapsed.is_zero() {
            return None;
        }
        let bytes = last.1.saturating_sub(first.1);
        let speed = bytes as f64 / elapsed.as_secs_f64();
        if !speed.is_finite() {
            return None;
        }
        Some(speed)
    }
}

/// One line of live transfer progress; every unavailable value is stated as
/// unavailable rather than guessed.
///
/// - with a known size: `downloading URL 1.5 KiB of 4.0 KiB (37%) at
///   3.4 KiB/s, 1 s left`;
/// - without a measured speed yet: the speed part becomes an explicit
///   "speed not yet measured" and the remaining time is omitted;
/// - unknown size: `downloading URL 1.5 KiB at 3.4 KiB/s (size unknown)`.
///
/// The URL is redacted through [`redact_url`] (query/fragment dropped) —
/// display lines are log-surface.
#[must_use]
pub fn progress_line(
    url: &str,
    done: u64,
    expected_size: Option<u64>,
    speed: Option<f64>,
    eta: Option<Duration>,
) -> String {
    let url = redact_url(url);
    let bytes = format_bytes(done);
    let speed_text = speed.map_or_else(|| "speed not yet measured".to_owned(), format_rate);
    match expected_size {
        Some(expected) => {
            let percent = done.min(expected) * 100 / expected.max(1);
            let eta_text =
                format_eta(eta).map_or_else(String::new, |eta_text| format!(", {eta_text} left"));
            format!(
                "downloading {url} {bytes} of {} ({percent}%) at {speed_text}{eta_text}",
                format_bytes(expected)
            )
        }
        None => {
            if speed.is_some() {
                format!("downloading {url} {bytes} at {speed_text} (size unknown)")
            } else {
                format!("downloading {url} {bytes} (size unknown, speed not yet measured)")
            }
        }
    }
}

/// One sweep input entry: the transfer's key, observed bytes, the recorded
/// expected size, the observed speed, and a remaining-time estimate.
type SweepEntry = (TransferKey, u64, Option<u64>, Option<f64>, Option<Duration>);

/// One sweep of live-transfer lines, in registry-snapshot order.
///
/// `entries` are the assembled [`SweepEntry`]s the async driver produces
/// from a registry snapshot, the database, and the rolling windows. Keys
/// whose bytes did not change since the last sweep render nothing (stalled
/// lines are not re-printed). The caller prunes `seen` entries whose
/// transfers left the registry. Returns the lines in the given order.
#[must_use]
pub fn sweep_lines(seen: &mut HashMap<TransferKey, u64>, entries: Vec<SweepEntry>) -> Vec<String> {
    let mut lines = Vec::new();
    for (key, bytes, expected, speed, eta) in entries {
        if seen.get(&key) == Some(&bytes) {
            continue; // unchanged since the last sweep: no redraw
        }
        let line = progress_line(&key.url, bytes, expected, speed, eta);
        seen.insert(key, bytes);
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{
        estimate_eta, format_bytes, format_eta, format_rate, progress_line, sweep_lines,
        ThroughputWindow, REFRESH_INTERVAL, ROLLING_WINDOW,
    };
    use crate::download::progress::TransferKey;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    #[test]
    fn byte_formatting_is_deterministic_and_bounded() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(1_023), "1023 B");
        assert_eq!(format_bytes(1_024), "1.0 KiB");
        assert_eq!(format_bytes(4_096), "4.0 KiB");
        assert_eq!(format_bytes(1_572_864), "1.5 MiB");
        assert_eq!(format_bytes(5_368_709_120), "5.0 GiB");
        assert_eq!(
            format_bytes(u64::MAX),
            "16.0 EiB",
            "the largest magnitude saturates without panicking"
        );
        // Non-decreasing words for increasing magnitudes sanity.
        assert!(format_bytes(2_048).contains("KiB"));
        assert!(format_bytes(2_097_152).contains("MiB"));
    }

    #[test]
    fn rate_formatting_is_non_negative() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(-12.0), "0 B/s");
        assert_eq!(format_rate(f64::NAN), "0 B/s", "non-finite is not a rate");
        assert_eq!(format_rate(1_024.0), "1.0 KiB/s");
        assert_eq!(format_rate(3_500.0), "3.4 KiB/s");
        assert_eq!(format_rate(300.0), "300 B/s");
    }

    #[test]
    fn eta_formatting_is_honest_and_readably_scaled() {
        assert_eq!(format_eta(None), None, "a None estimate is honest");
        assert_eq!(
            format_eta(Some(Duration::from_secs(0))),
            Some("0 s".to_owned())
        );
        assert_eq!(
            format_eta(Some(Duration::from_secs(9))),
            Some("9 s".to_owned())
        );
        assert_eq!(
            format_eta(Some(Duration::from_secs(75))),
            Some("1 min 15 s".to_owned())
        );
        assert_eq!(
            format_eta(Some(Duration::from_secs(7_200))),
            Some("2 h 0 min".to_owned())
        );
    }

    #[test]
    fn eta_estimation_uses_only_known_size_and_observed_speed() {
        // 4 KiB total, 1 KiB done at 1024 B/s -> exactly 3 s remain.
        let eta = estimate_eta(4_096, 1_024, Some(1_024.0)).expect("measurable");
        assert_eq!(eta.as_secs_f64().round() as u64, 3);

        for speed in [None, Some(0.0), Some(-5.0), Some(f64::NAN)] {
            assert!(
                estimate_eta(4_096, 1_024, speed).is_none(),
                "no measurable speed means no estimate: {speed:?}"
            );
        }
        assert!(estimate_eta(4_096, 4_096, Some(1_024.0)).is_some());
        // A body claiming more progress than the whole size never estimates.
        assert!(estimate_eta(4_096, 8_192, Some(1_024.0)).is_none());
    }

    #[test]
    fn progress_lines_are_bounded_non_negative_and_honest() {
        let url = "http://127.0.0.1:9/file.bin?token=abc";
        let line = progress_line(
            url,
            1_536,
            Some(4_096),
            Some(3_500.0),
            Some(Duration::from_secs(1)),
        );
        assert!(!line.contains("token"), "display URLs are redacted: {line}");
        assert!(line.contains("1.5 KiB of 4.0 KiB"), "{line}");
        assert!(line.contains("(37%)"), "{line}");
        assert!(line.contains("3.4 KiB/s"), "{line}");
        assert!(line.contains("1 s left"), "{line}");

        // Percent is bounded: an overrun body never displays over 100%.
        let line = progress_line(url, 8_192, Some(4_096), Some(3_500.0), None);
        assert!(line.contains("(100%)"), "bounded: {line}");
        assert!(
            !line.contains("left"),
            "no remaining estimate for an overrun: {line}"
        );

        // No speed measured yet: the line says so instead of guessing.
        let line = progress_line(url, 1_536, Some(4_096), None, None);
        assert!(line.contains("speed not yet measured"), "{line}");
        assert!(!line.contains("left"), "{line}");

        // Unknown size: no percent, no estimate, honest wording.
        let line = progress_line(url, 1_536, None, Some(3_500.0), None);
        assert!(line.contains("(size unknown)"), "{line}");
        assert!(!line.contains("%"), "{line}");
        let line = progress_line(url, 0, None, None, None);
        assert!(
            line.contains("0 B (size unknown, speed not yet measured)"),
            "{line}"
        );
    }

    #[test]
    fn a_rolling_window_computes_speed_from_observed_bytes() {
        let start = Instant::now();
        let mut window = ThroughputWindow::new(ROLLING_WINDOW);

        assert!(
            window.sample(start, 0).is_none(),
            "one sample is not a speed"
        );
        let speed = window
            .sample(start + Duration::from_millis(1_000), 1_000)
            .expect("two samples 1 s apart");
        assert_eq!(speed as u64, 1_000, "1000 bytes in 1 s");

        let speed = window
            .sample(start + Duration::from_millis(2_000), 3_000)
            .expect("the window is boundary-inclusive: nothing is dropped yet");
        // The 2 s window still holds the t=0 sample (elapsed == span), so
        // the speed is the true window average: 3000 bytes over 2 s.
        assert!(
            (speed - 1_500.0).abs() < 0.001,
            "3000 bytes over the full 2 s window: {speed}"
        );
    }

    #[test]
    fn a_window_prunes_stale_samples_and_refuses_short_spans() {
        let start = Instant::now();
        let mut window = ThroughputWindow::new(REFRESH_INTERVAL);

        window.sample(start, 0);
        assert!(window.sample(start, 0).is_none(), "zero elapsed, no speed");
        // Everything is older than the tiny window once a new sample stands
        // alone again.
        assert!(
            window
                .sample(start + Duration::from_millis(2_000), 10)
                .is_none(),
            "a pruned window needs to rebuild before a speed exists"
        );
        let speed = window.sample(start + Duration::from_millis(2_050), 60);
        // The window retains both fresh samples: 50 bytes over 50 ms.
        let expected = 50_f64 / 0.05;
        match speed {
            Some(actual) => assert!((actual - expected).abs() < 1.0, "got {actual}"),
            None => panic!("two fresh samples must produce a speed"),
        }
    }

    #[test]
    fn a_sweep_suppresses_unchanged_transfers_only() {
        let key = TransferKey::new("http://host/one.bin", PathBuf::from("out"));
        let mut seen: HashMap<TransferKey, u64> = HashMap::new();

        let entries = vec![(key.clone(), 512, Some(4_096), None, None)];
        let lines = sweep_lines(&mut seen, entries);
        assert_eq!(lines.len(), 1, "first view always renders: {lines:?}");

        let entries = vec![(key.clone(), 512, Some(4_096), None, None)];
        let lines = sweep_lines(&mut seen, entries);
        assert!(
            lines.is_empty(),
            "an unchanged byte count is not re-printed: {lines:?}"
        );

        let entries = vec![(key.clone(), 1_024, Some(4_096), Some(512.0), None)];
        let lines = sweep_lines(&mut seen, entries);
        assert_eq!(lines.len(), 1, "new bytes render: {lines:?}");
        assert!(lines[0].contains("1.0 KiB"), "{}", lines[0]);
    }
}
