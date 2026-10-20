//! Performance and resource validation benchmarks (Phase 13).
//!
//! Long-running, environment-local measurements against the controlled local
//! mock server. Every scenario drives the real engine
//! ([`transfer_persisted`] / [`run_batch`]) through a real SQLite repository
//! and records elapsed time, throughput, request count, process CPU time,
//! and working-set memory; every downloaded fixture is verified byte-for-byte
//! afterwards.
//!
//! These tests are `#[ignore]`d: a full repetition takes seconds to minutes
//! on this machine and the numbers describe this machine's loopback mock
//! only, not a universal speed claim (`DOCS/phases.md` Phase 13 step 8).
//! Repeat them with:
//
//!     cargo test --bin swift-fetch -- --ignored --nocapture
//!
//! Regression assertions (byte-exact correctness, bounded concurrency,
//! bounded memory) run with every repetition. The canonical `cargo test`
//! keeps a cheap, non-ignored harness sanity check.
#![allow(dead_code)] // exercised only by the ignored benchmarks below

use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::app::batch::{run_batch, BatchReport, BatchRequest, DownloadResult};
use crate::download::http::HttpClient;
use crate::download::progress::{Progress, TransferRegistry};
use crate::download::resume::{transfer_persisted, ShutdownSignal};
use crate::download::retry::RetryPolicy;
use crate::download::segmented::SegmentationPolicy;
use crate::error::AppError;
use crate::storage::repository::Repository;

use crate::common::fixture::Fixture;
use crate::common::server::{Behavior, MockServer};

/// unix-seconds clock fed to the repository as record timestamps.
fn now_stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// One instantaneous process-level measurement.
#[derive(Clone, Copy, Debug, Default)]
struct ProcessSample {
    /// Kernel + user CPU time consumed by the process, in 100-nanosecond units.
    cpu_100ns: u64,
    /// Current working-set size in bytes.
    working_set_bytes: u64,
}

/// Samples this process's CPU time and working set.
///
/// `None` when the platform cannot supply it; callers record "not measured"
/// rather than failing. The sample is harness-inclusive: the in-process mock
/// server shares the process with the engine being measured.
#[cfg(windows)]
fn sample_process() -> Option<ProcessSample> {
    unsafe { sample_process_windows() }
}
#[cfg(not(windows))]
fn sample_process() -> Option<ProcessSample> {
    None
}

#[cfg(windows)]
unsafe fn sample_process_windows() -> Option<ProcessSample> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    // `GetCurrentProcess` is a header macro (`(HANDLE)-1`) rather than a
    // function in windows-sys 0.61.2; the current-process pseudo-handle is
    // accepted by both calls below.
    let current_process = -1isize as *mut std::ffi::c_void;
    let mut creation = MaybeUninit::<FILETIME>::zeroed();
    let mut exit = MaybeUninit::<FILETIME>::zeroed();
    let mut kernel = MaybeUninit::<FILETIME>::zeroed();
    let mut user = MaybeUninit::<FILETIME>::zeroed();
    if GetProcessTimes(
        current_process,
        creation.as_mut_ptr(),
        exit.as_mut_ptr(),
        kernel.as_mut_ptr(),
        user.as_mut_ptr(),
    ) == 0
    {
        return None;
    }
    let as_u64 = |ft: FILETIME| (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64;
    let cpu = as_u64(kernel.assume_init()) + as_u64(user.assume_init());
    let working_set = sample_working_set().unwrap_or(0);
    Some(ProcessSample {
        cpu_100ns: cpu,
        working_set_bytes: working_set,
    })
}

/// Working-set bytes via the in-box Psapi library (test-only telemetry, no
/// new crate: `Psapi.lib` ships with the Windows SDK the MSVC toolchain
/// already links against).
#[cfg(windows)]
unsafe fn sample_working_set() -> Option<u64> {
    use std::mem::size_of;

    #[link(name = "Psapi")]
    extern "system" {
        fn GetProcessMemoryInfo(
            hprocess: *mut std::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    // `GetCurrentProcess` is a header macro (`(HANDLE)-1`) rather than a
    // function in windows-sys 0.61.2.
    let current_process = -1isize as *mut std::ffi::c_void;

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool: usize,
        quota_paged_pool: usize,
        quota_peak_non_paged_pool: usize,
        quota_non_paged_pool: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    let mut counters = ProcessMemoryCounters {
        cb: size_of::<ProcessMemoryCounters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool: 0,
        quota_paged_pool: 0,
        quota_peak_non_paged_pool: 0,
        quota_non_paged_pool: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
        private_usage: 0,
    };
    let ok = GetProcessMemoryInfo(
        current_process,
        &mut counters,
        size_of::<ProcessMemoryCounters>() as u32,
    );
    if ok == 0 {
        None
    } else {
        Some(counters.working_set_size as u64)
    }
}

/// One completed benchmark run: outcome plus everything the phase asks to record.
#[derive(Debug)]
struct RunResult {
    elapsed: Duration,
    bytes_written: u64,
    throughput_mib_s: f64,
    requests: u64,
    /// Process CPU time consumed during the run, in nanoseconds (0 when not measured).
    cpu_delta_ns: u64,
    mem_before_bytes: u64,
    mem_after_bytes: u64,
    /// Working-set delta in bytes (negative if the allocator released pages).
    mem_delta_bytes: i64,
    memory_measured: bool,
}

fn build_result(
    elapsed: Duration,
    bytes_written: u64,
    requests: u64,
    before: Option<ProcessSample>,
    after: Option<ProcessSample>,
) -> RunResult {
    let (cpu_delta_ns, mem_before, mem_after, mem_delta, memory_measured) = match (before, after) {
        (Some(b), Some(a)) => {
            let cpu = (a.cpu_100ns.saturating_sub(b.cpu_100ns)) * 100;
            let mb = b.working_set_bytes;
            let ma = a.working_set_bytes;
            (cpu, mb, ma, ma as i64 - mb as i64, true)
        }
        _ => (0, 0, 0, 0, false),
    };
    RunResult {
        elapsed,
        bytes_written,
        throughput_mib_s: bytes_written as f64
            / elapsed.as_secs_f64().max(1e-9)
            / (1024.0 * 1024.0),
        requests,
        cpu_delta_ns,
        mem_before_bytes: mem_before,
        mem_after_bytes: mem_after,
        mem_delta_bytes: mem_delta,
        memory_measured,
    }
}

/// Isolated scratch for one scenario: an output directory and one database per run.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "swiftfetch-p13-{tag}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("scratch dir created");
        Self { root: dir }
    }

    /// Output subdirectory for one run (keeps concurrent scenarios independent).
    fn output_dir(&self, run: u32) -> PathBuf {
        let path = self.root.join(format!("run-{run}"));
        let _ = fs::create_dir_all(&path);
        path
    }

    /// Fresh database file for one run.
    fn database(&self, run: u32) -> PathBuf {
        self.root.join(format!("db-{run}.sqlite3"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Verifies the finalized file byte-for-byte without materializing the whole
/// fixture, so the harness's own memory stays bounded for large files.
fn assert_file_matches_fixture(path: &Path, fixture: &Fixture) {
    let mut file = File::open(path).expect("the finalized file must exist");
    let mut buffer = [0u8; 65536];
    let mut position: u64 = 0;
    loop {
        let read = file
            .read(&mut buffer)
            .expect("the finalized file must be readable");
        if read == 0 {
            break;
        }
        for (offset, byte) in buffer[..read].iter().enumerate() {
            let expected = fixture.byte_at(position + offset as u64);
            assert_eq!(
                *byte, expected,
                "finalized file byte {position} differs from the fixture"
            );
        }
        position += read as u64;
    }
    assert_eq!(
        position, fixture.len,
        "finalized file is {position} bytes, the fixture is {} bytes",
        fixture.len
    );
}

/// One transfer through the persisted engine: mount the route, run
/// `transfer_persisted`, verify the final file, and measure.
async fn transfer_run(
    server: &MockServer,
    scratch: &Scratch,
    run: u32,
    path: &str,
    fixture: Fixture,
    policy: &RetryPolicy,
    segmentation: &SegmentationPolicy,
) -> Result<RunResult, AppError> {
    server.mount_route(path, Behavior::default().with_fixture(fixture));
    let url = server.url(path);
    let requests_before = server.requests().len() as u64;
    let output_dir = scratch.output_dir(run);
    let repository = Repository::open(&scratch.database(run))?;
    let client = HttpClient::default_client()?;
    let progress = Progress::shared();
    let (_signal, handle) = ShutdownSignal::channel();

    let before = sample_process();
    let started = Instant::now();
    let download = transfer_persisted(
        &client,
        &repository,
        &url,
        &output_dir,
        progress,
        policy,
        segmentation,
        now_stamp,
        &handle,
        false, // zip_auto_extract: benchmarks measure the transfer, not extraction
    )
    .await?;
    let elapsed = started.elapsed();
    let after = sample_process();
    let requests = server.requests().len() as u64 - requests_before;

    assert_file_matches_fixture(&download.destination, &fixture);

    Ok(build_result(
        elapsed,
        download.bytes_written,
        requests,
        before,
        after,
    ))
}

/// One batch run through the bounded scheduler: every URL is queued first,
/// then `min(max_concurrent_files, urls)` workers drive the persisted engine.
#[allow(clippy::too_many_arguments)] // the run is parameterized, not accidental
async fn batch_run(
    server: &MockServer,
    scratch: &Scratch,
    run: u32,
    request: BatchRequest,
    routes: Vec<(String, String, Fixture)>,
    max_concurrent_files: u32,
    policy: &RetryPolicy,
    segmentation: &SegmentationPolicy,
) -> Result<(BatchReport, RunResult), AppError> {
    for (path, _, fixture) in &routes {
        server.mount_route(path, Behavior::default().with_fixture(*fixture));
    }
    let requests_before = server.requests().len() as u64;
    let output_dir = scratch.output_dir(run);
    let registry = TransferRegistry::shared();
    let (_signal, handle) = ShutdownSignal::channel();

    let before = sample_process();
    let started = Instant::now();
    let repository = Arc::new(Repository::open(&scratch.database(run))?);
    let client = HttpClient::default_client()?;
    let report = run_batch(
        client,
        Arc::clone(&repository),
        request,
        &output_dir,
        max_concurrent_files,
        policy,
        segmentation,
        &registry,
        false, // zip_auto_extract: benchmarks measure the transfer, not extraction
        &handle,
        now_stamp,
    )
    .await?;
    let elapsed = started.elapsed();
    let after = sample_process();
    let requests = server.requests().len() as u64 - requests_before;
    let bytes_written: u64 = report
        .outcomes
        .iter()
        .filter_map(|outcome| match &outcome.result {
            DownloadResult::Completed { bytes_written, .. } => Some(*bytes_written),
            _ => None,
        })
        .sum();

    // Every completed file must match the route it was fetched from.
    for outcome in &report.outcomes {
        if let DownloadResult::Completed { destination, .. } = &outcome.result {
            let name = destination
                .file_name()
                .expect("completed downloads have a file name")
                .to_string_lossy();
            let fixture = routes
                .iter()
                .find(|(_, name_, _)| name_ == &*name)
                .map(|(_, _, fixture)| fixture)
                .expect("every completed URL was mounted");
            assert_file_matches_fixture(destination, fixture);
        }
    }

    Ok((
        report,
        build_result(elapsed, bytes_written, requests, before, after),
    ))
}

/// Header printed once per scenario.
fn scenario_header(label: &str, settings: &str) {
    println!("bench {label}: {settings}");
}

/// One run's result line.
fn run_line(run: u32, result: &RunResult) {
    let cpu_ms = result.cpu_delta_ns / 1_000_000;
    let memory = if result.memory_measured {
        format!(
            "mem_before={} KiB mem_after={} KiB mem_delta={} bytes",
            result.mem_before_bytes / 1024,
            result.mem_after_bytes / 1024,
            result.mem_delta_bytes
        )
    } else {
        "memory not measured on this platform".to_owned()
    };
    println!(
        "  run {run}: bytes={} elapsed={:.3}s throughput={:.1} MiB/s requests={} cpu_ms={} {memory}",
        result.bytes_written,
        result.elapsed.as_secs_f64(),
        result.throughput_mib_s,
        result.requests,
        cpu_ms,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap harness sanity check that runs in every `cargo test`: one
    /// 4 KiB transfer, measured and verified end to end.
    #[tokio::test]
    async fn the_benchmark_harness_exercises_one_transfer_end_to_end() {
        let scratch = Scratch::new("p13-harness-sanity");
        let server = MockServer::start().await;
        let fixture = Fixture::new(1, 4_096);
        let policy = RetryPolicy::default();
        let segmentation = SegmentationPolicy::default();

        let result = transfer_run(
            &server,
            &scratch,
            0,
            "/sanity.bin",
            fixture,
            &policy,
            &segmentation,
        )
        .await
        .expect("the sanity transfer completes");

        assert_eq!(result.bytes_written, 4_096);
        assert_eq!(
            result.requests, 3,
            "probe (HEAD + ranged GET) plus one transfer GET"
        );
        assert!(result.elapsed.as_secs_f64() > 0.0);
        println!(
            "bench sanity: 4 KiB in {:.3}s ({:.1} MiB/s), {} requests",
            result.elapsed.as_secs_f64(),
            result.throughput_mib_s,
            result.requests
        );
    }

    // -----------------------------------------------------------------------
    // Ignored long measurements. Repeat with:
    //   cargo test --bin swift-fetch -- --ignored --nocapture
    // -----------------------------------------------------------------------

    /// Sequential baseline: one 64 MiB file, forced sequential
    /// (`max_segments = 1`), three fresh repositories.
    #[tokio::test]
    #[ignore = "long-running performance measurement; repeat with --ignored --nocapture"]
    async fn sequential_baseline_benchmark() -> Result<(), AppError> {
        let scratch = Scratch::new("p13-seq-baseline");
        let server = MockServer::start().await;
        let fixture = Fixture::new(10, 64 * 1024 * 1024);
        server.mount_route("/baseline.bin", Behavior::default().with_fixture(fixture));

        let policy = RetryPolicy::default();
        let segmentation = SegmentationPolicy {
            max_segments: 1, // one whole-range span: sequential, no range parallelism
            ..SegmentationPolicy::default()
        };

        scenario_header(
            "sequential_baseline",
            "64 MiB, sequential (max_segments=1), one client+repository per repeat, 3 repeats",
        );
        let mut runs = Vec::new();
        for run in 0..3 {
            let result = transfer_run(
                &server,
                &scratch,
                run,
                "/baseline.bin",
                fixture,
                &policy,
                &segmentation,
            )
            .await?;
            runs.push(result);
            run_line(run, &runs[run as usize]);
        }
        for (run, result) in runs.iter().enumerate() {
            assert_eq!(
                result.requests, 3,
                "repeat {run}: probe (HEAD + ranged GET) plus one transfer GET"
            );
            assert_eq!(result.bytes_written, fixture.len);
        }
        let total: u64 = runs.iter().map(|r| r.bytes_written).sum();
        let elapsed: f64 = runs.iter().map(|r| r.elapsed.as_secs_f64()).sum();
        println!(
            "  aggregate: {} bytes in {:.3}s ({:.1} MiB/s)",
            total,
            elapsed,
            total as f64 / elapsed / (1024.0 * 1024.0)
        );
        Ok(())
    }

    /// Segment-count comparison: one 64 MiB file, one fresh repository per
    /// variant, max_segments in {1, 2, 4 (default), 8}.
    #[tokio::test]
    #[ignore = "long-running performance measurement; repeat with --ignored --nocapture"]
    async fn segment_count_comparison_benchmark() -> Result<(), AppError> {
        let scratch = Scratch::new("p13-segment-comparison");
        let server = MockServer::start().await;
        let fixture = Fixture::new(11, 64 * 1024 * 1024);
        server.mount_route("/segments.bin", Behavior::default().with_fixture(fixture));

        let policy = RetryPolicy::default();
        let configs = [
            (
                "sequential (max_segments=1)",
                SegmentationPolicy {
                    max_segments: 1,
                    ..SegmentationPolicy::default()
                },
            ),
            (
                "2 segments",
                SegmentationPolicy {
                    max_segments: 2,
                    ..SegmentationPolicy::default()
                },
            ),
            ("4 segments (default)", SegmentationPolicy::default()),
            (
                "8 segments",
                SegmentationPolicy {
                    max_segments: 8,
                    ..SegmentationPolicy::default()
                },
            ),
        ];

        scenario_header(
            "segment_count_comparison",
            "64 MiB, max_segments in {1, 2, 4 (default), 8}, one fresh repository per variant",
        );
        let mut results = Vec::new();
        for (run, (label, segmentation)) in configs.iter().enumerate() {
            // One probe request pair (HEAD + ranged GET) plus one GET per
            // planned span; a sequential transfer plans one whole-range span.
            let expected_spans = segmentation
                .plan(fixture.len)
                .map(|spans| spans.len())
                .unwrap_or(1);
            let expected_requests = 2 + expected_spans;
            let result = transfer_run(
                &server,
                &scratch,
                run as u32,
                "/segments.bin",
                fixture,
                &policy,
                segmentation,
            )
            .await?;
            results.push((label.to_owned(), result, expected_requests));
            run_line(run as u32, &results.last().unwrap().1);
        }
        for (label, result, expected_requests) in &results {
            assert_eq!(
                result.requests, *expected_requests as u64,
                "{label}: probe plus one GET per planned span"
            );
            assert_eq!(result.bytes_written, fixture.len, "{label}: byte-exact");
        }
        let default_throughput = results[2].1.throughput_mib_s;
        let best = results
            .iter()
            .map(|(_, result, _)| result.throughput_mib_s)
            .fold(0.0f64, f64::max);
        println!(
            "  spread: default 4 segments = {:.1} MiB/s, best on loopback = {:.1} MiB/s \
             (no ranking claimed beyond this machine)",
            default_throughput, best
        );
        Ok(())
    }

    /// Multiple simultaneous files under a global concurrency limit:
    /// 8 files of 16 MiB (below the 32 MiB segmentation minimum, so every
    /// file is sequential and only file-level concurrency is measured),
    /// limits {1, 3 (default), 8}.
    #[tokio::test]
    #[ignore = "long-running performance measurement; repeat with --ignored --nocapture"]
    async fn multiple_file_concurrency_benchmark() -> Result<(), AppError> {
        let scratch = Scratch::new("p13-concurrency");
        let server = MockServer::start().await;
        let fixture = Fixture::new(12, 16 * 1024 * 1024);
        let url_count = 8usize;
        let mut routes = Vec::new();
        let mut urls = Vec::new();
        for i in 0..url_count {
            let path = format!("/file-{i:03}.bin");
            urls.push(server.url(&path));
            routes.push((
                path.clone(),
                path.trim_start_matches('/').to_owned(),
                fixture,
            ));
        }
        let policy = RetryPolicy::default();
        let segmentation = SegmentationPolicy::default();

        scenario_header(
            "multiple_file_concurrency",
            "8 files x 16 MiB, max_concurrent_files in {1, 3 (default), 8}",
        );
        for (run, limit) in [1u32, 3, 8].iter().enumerate() {
            let request = BatchRequest {
                urls: urls.clone(),
                url_file: None,
            };
            let (report, result) = batch_run(
                &server,
                &scratch,
                run as u32,
                request,
                routes.clone(),
                *limit,
                &policy,
                &segmentation,
            )
            .await?;

            assert_eq!(
                report.completed_count(),
                url_count,
                "limit {limit}: every file completes"
            );
            assert_eq!(report.failed_count(), 0, "limit {limit}: no failures");
            assert!(
                report.peak_active_files <= *limit,
                "limit {limit}: active files never exceed the configured limit (was {})",
                report.peak_active_files
            );
            run_line(run as u32, &result);
            println!(
                "    peak_active_files={} peak_active_gets(mock server)={}",
                report.peak_active_files,
                server.peak_active_gets()
            );
        }
        Ok(())
    }

    /// Large file / resource bound: one 128 MiB file with the default
    /// settings; the working-set delta during the transfer must stay below
    /// one full file copy (no whole-file buffering).
    #[tokio::test]
    #[ignore = "long-running performance measurement; repeat with --ignored --nocapture"]
    async fn large_file_resource_bound_benchmark() -> Result<(), AppError> {
        let scratch = Scratch::new("p13-large-file");
        let server = MockServer::start().await;
        let fixture = Fixture::new(13, 128 * 1024 * 1024);
        server.mount_route("/large.bin", Behavior::default().with_fixture(fixture));

        let policy = RetryPolicy::default();
        let segmentation = SegmentationPolicy::default();

        scenario_header(
            "large_file_resource_bound",
            "128 MiB, defaults (3 concurrent files, 4 segments per file, 5 retries), 3 repeats",
        );
        for run in 0..3 {
            let result = transfer_run(
                &server,
                &scratch,
                run,
                "/large.bin",
                fixture,
                &policy,
                &segmentation,
            )
            .await?;
            run_line(run, &result);
            assert_eq!(result.bytes_written, fixture.len, "run {run}: byte-exact");
            if result.memory_measured {
                assert!(
                    result.mem_delta_bytes < fixture.len as i64,
                    "run {run}: the transfer did not residently copy the whole file \
                     (working-set delta {} bytes vs {} byte file)",
                    result.mem_delta_bytes,
                    fixture.len
                );
            }
        }
        Ok(())
    }

    /// Long URL-list stress: 200 URLs x 4 KiB from a list file, limit 4.
    #[tokio::test]
    #[ignore = "long-running performance measurement; repeat with --ignored --nocapture"]
    async fn long_url_list_stress_benchmark() -> Result<(), AppError> {
        let scratch = Scratch::new("p13-url-list");
        let server = MockServer::start().await;
        let fixture = Fixture::new(14, 4 * 1024);
        const URL_COUNT: usize = 200;
        let mut routes = Vec::new();
        let mut list_text = String::new();
        for i in 0..URL_COUNT {
            let path = format!("/list-{i:04}.bin");
            let url = server.url(&path);
            routes.push((
                path.clone(),
                path.trim_start_matches('/').to_owned(),
                fixture,
            ));
            list_text.push_str(&url);
            list_text.push('\n');
        }
        let list_path = scratch.root.join("urls.txt");
        fs::write(&list_path, &list_text).expect("url list written");

        let policy = RetryPolicy::default();
        let segmentation = SegmentationPolicy::default();

        scenario_header(
            "long_url_list_stress",
            "200 URLs x 4 KiB from a URL-list file, max_concurrent_files=4",
        );
        let request = BatchRequest {
            urls: Vec::new(),
            url_file: Some(list_path),
        };
        let (report, result) = batch_run(
            &server,
            &scratch,
            0,
            request,
            routes,
            4,
            &policy,
            &segmentation,
        )
        .await?;

        assert_eq!(
            report.completed_count(),
            URL_COUNT,
            "every URL in the long list completes"
        );
        assert_eq!(report.failed_count(), 0, "no download in the list failed");
        assert!(
            report.peak_active_files <= 4,
            "active files never exceed the configured limit (was {})",
            report.peak_active_files
        );
        run_line(0, &result);
        println!(
            "    peak_active_files={} peak_active_gets(mock server)={}",
            report.peak_active_files,
            server.peak_active_gets()
        );
        if result.memory_measured {
            assert!(
                result.mem_delta_bytes < 100_000_000,
                "memory stays bounded for a long list (delta was {} bytes)",
                result.mem_delta_bytes
            );
        }
        Ok(())
    }
}
