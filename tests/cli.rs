//! CLI end-to-end integration tests (Phases 1 and 11).
//!
//! These tests exercise the compiled executable — the CLI *process*
//! behavior (stdout/stderr/exit codes) is the contract being tested:
//!
//! - the documented exit-code table: `0` every requested download
//!   completed, `1` at least one did not (or the command could not run),
//!   `2` invalid invocation/configuration;
//! - every command wired in Phase 11, driven against the controlled local
//!   mock server (`tests/common/`) — never a live external URL;
//! - hermetic persistence: each invocation gets its own database through
//!   the `SWIFT_FETCH_DATABASE` location override, so tests never touch
//!   the user's real application-data directory.
//!
//! Runtime note: every child runs on a helper thread and reports over a
//! channel (see [`run_in`]) so the test runtime keeps polling the mock
//! server while a download is in flight.

mod common;

use common::fixture::Fixture;
use common::server::{Behavior, MockServer};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// An isolated, auto-removed scratch directory per test: the child's
/// working directory (the default output) and its database home.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "swiftfetch-cli-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir created");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn database(&self) -> PathBuf {
        self.0.join("queue.sqlite3")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs the compiled executable with arguments in `scratch` (as working
/// directory and database home); returns the output.
///
/// The child runs on a helper thread and reports over a channel:
/// awaiting here keeps the test runtime free to poll the mock server
/// tasks the child's transfers depend on — blocking the executor would
/// starve the very server the download talks to.
async fn run_in(scratch: &Scratch, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_swift-fetch"));
    command
        .args(args)
        .current_dir(scratch.path())
        .env("SWIFT_FETCH_DATABASE", scratch.database());
    run_command(command).await
}

/// Runs the executable with one additional environment override (the
/// database location is still forced hermetic).
async fn run_with_env(scratch: &Scratch, args: &[&str], env: (&str, &str)) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_swift-fetch"));
    command
        .args(args)
        .current_dir(scratch.path())
        .env("SWIFT_FETCH_DATABASE", scratch.database())
        .env(env.0, env.1);
    run_command(command).await
}

/// Runs one prepared command to completion on a helper thread.
async fn run_command(mut command: Command) -> Output {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let output = command
            .output()
            .expect("the compiled executable should exist and be runnable");
        let _ = sender.send(output);
    });
    receiver
        .await
        .expect("the runner thread reports the child's output")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// -------------------------------------------------------------------------
// The command surface itself (help/version/parse-level rejections).
// -------------------------------------------------------------------------

#[tokio::test]
async fn help_is_accurate_and_documents_the_exit_codes() {
    let output = run_in(&Scratch::new("help"), &["--help"]).await;

    assert!(
        output.status.success(),
        "--help exits successfully; stdout: {}",
        stdout(&output)
    );
    let text = stdout(&output);
    for fragment in ["download", "status", "resume", "retry"] {
        assert!(
            text.contains(fragment),
            "help must list the {fragment} command"
        );
    }
    assert!(
        text.contains("Exit codes: 0") && text.contains("2 invalid invocation"),
        "help must document the exit-code table: {text}"
    );

    let download_help = run_in(&Scratch::new("help-download"), &["download", "--help"]).await;
    assert!(download_help.status.success());
    let download = stdout(&download_help);
    for fragment in [
        "--url-file",
        "--output",
        "--files",
        "--segments",
        "--retries",
        "--no-extract",
    ] {
        assert!(
            download.contains(fragment),
            "download help must list {fragment}: {download}"
        );
    }
}

#[tokio::test]
async fn version_reports_program_name_and_version() {
    let output = run_in(&Scratch::new("version"), &["--version"]).await;

    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("SwiftFetch"), "stdout: {text}");
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "version output must carry the package version: {text}"
    );
}

#[tokio::test]
async fn missing_subcommand_is_an_invalid_invocation() {
    let output = run_in(&Scratch::new("no-subcommand"), &[]).await;

    assert_eq!(
        output.status.code(),
        Some(2),
        "clap rejects an invocation without a subcommand (exit 2)"
    );
    assert!(stdout(&output).is_empty(), "errors go to stderr");
    assert!(!stderr(&output).is_empty());
}

#[tokio::test]
async fn unknown_subcommand_is_an_invalid_invocation() {
    let output = run_in(&Scratch::new("unknown-subcommand"), &["frobnicate"]).await;

    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    assert!(!stderr(&output).is_empty());
}

#[tokio::test]
async fn download_requires_urls_or_a_url_file() {
    let output = run_in(&Scratch::new("no-input"), &["download"]).await;

    assert_eq!(output.status.code(), Some(2));
    let err = stderr(&output);
    assert!(
        err.contains("URL") && err.contains("--url-file"),
        "message must state both accepted input kinds: {err}"
    );
}

#[tokio::test]
async fn download_rejects_non_http_schemes_before_dispatch() {
    for url in ["ftp://example.com/f", "example.com/f"] {
        let output = run_in(&Scratch::new("bad-scheme"), &["download", url]).await;

        assert_eq!(
            output.status.code(),
            Some(2),
            "reject {url} as invalid input"
        );
        let err = stderr(&output);
        assert!(
            err.contains("http"),
            "message must name the accepted schemes: {err}"
        );
    }
}

#[tokio::test]
async fn download_rejects_non_positive_concurrency() {
    let output = run_in(
        &Scratch::new("zero-files"),
        &["download", "-j", "0", "https://ex.invalid:1/f"],
    )
    .await;

    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("at least 1"),
        "message must explain the minimum: {}",
        stderr(&output)
    );

    let output = run_in(
        &Scratch::new("zero-segments"),
        &["download", "-s", "0", "https://ex.invalid:1/f"],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
}

#[tokio::test]
async fn download_rejects_empty_output_directory() {
    let output = run_in(
        &Scratch::new("empty-output"),
        &["download", "-o", "", "https://ex.invalid:1/f"],
    )
    .await;

    assert_eq!(
        output.status.code(),
        Some(2),
        "empty output path is invalid early"
    );
    assert!(
        stderr(&output).contains("must not be empty"),
        "{}",
        stderr(&output)
    );
}

// -------------------------------------------------------------------------
// The `download` workflow end to end, against the controlled mock server.
// -------------------------------------------------------------------------

#[tokio::test]
async fn a_single_url_downloads_end_to_end_and_exits_0() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/single.bin", Behavior::default());
    let scratch = Scratch::new("single-url");
    let url = server.url("/single.bin");

    let output = run_in(&scratch, &["download", &url]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "a healthy download exits 0; stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("1 completed, 0 failed, 0 interrupted"),
        "the summary reports every outcome: {text}"
    );

    let destination = scratch.path().join("single.bin");
    assert_eq!(
        std::fs::read(&destination).expect("final file").as_slice(),
        fixture.bytes().as_slice(),
        "the finalized file holds the exact fixture bytes"
    );

    // The persisted queue agrees: the record completed.
    let status = run_in(&scratch, &["status"]).await;
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    let listing = stdout(&status);
    assert!(
        listing.contains("completed") && listing.contains("single.bin"),
        "status shows the completed record: {listing}"
    );
    assert!(
        listing.contains("4.0 KiB total"),
        "a completed record shows its recorded size: {listing}"
    );
}

#[tokio::test]
async fn inline_urls_and_a_list_file_download_together() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    let scratch = Scratch::new("list-file");
    let names = ["listed-a.bin", "listed-b.bin"];
    for name in names {
        server.mount_route(&format!("/{name}"), Behavior::default());
    }
    let inline_url = server.url("/inline.bin");
    server.mount_route("/inline.bin", Behavior::default());
    let list = scratch.path().join("urls.txt");
    // A comment and a blank line are skipped; the listed URLs queue.
    std::fs::write(
        &list,
        format!(
            "# the list\n\n{}\n{}\n",
            server.url("/listed-a.bin"),
            server.url("/listed-b.bin")
        ),
    )
    .expect("list written");

    let list_text = list.display().to_string();
    let output = run_in(
        &scratch,
        &["download", &inline_url, "--url-file", &list_text, "-j", "3"],
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "three accepted URLs exit 0; stderr: {}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("3 completed, 0 failed, 0 interrupted"),
        "the summary counts the accepted URLs: {}",
        stdout(&output)
    );
    for name in names {
        assert_eq!(
            std::fs::read(scratch.path().join(name))
                .expect("final file")
                .as_slice(),
            fixture.bytes().as_slice(),
            "{name} holds the fixture bytes"
        );
    }
    assert_eq!(
        std::fs::read(scratch.path().join("inline.bin"))
            .expect("final file")
            .as_slice(),
        fixture.bytes().as_slice()
    );
}

#[tokio::test]
async fn a_failed_download_exits_1_and_records_the_failure() {
    let server = MockServer::start().await;
    server.mount_route("/missing.bin", Behavior::default().with_status(404));
    let scratch = Scratch::new("failed-download");
    let url = format!("{}?token=secret", server.url("/missing.bin"));

    let output = run_in(&scratch, &["download", "-r", "0", &url]).await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "a failed download is not a success; stdout: {}",
        stdout(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("failed") && text.contains("missing.bin"),
        "the failed outcome is reported on the results surface: {text}"
    );
    assert!(text.contains("0 completed, 1 failed"), "summary: {text}");
    assert!(
        !text.contains("secret"),
        "result lines are redacted: {text}"
    );
    assert!(
        !scratch.path().join("missing.bin").exists(),
        "a failed download never finalizes a file"
    );

    // The persisted record carries the failure, and status is honest
    // about it (readable output across lifecycle states).
    let status = run_in(&scratch, &["status"]).await;
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    let listing = stdout(&status);
    assert!(
        listing.contains("failed") && listing.contains("(reason: http)"),
        "the failed record names its stored reason: {listing}"
    );
    assert!(
        !listing.contains("secret"),
        "status display URLs are redacted: {listing}"
    );
}

#[tokio::test]
async fn a_mixed_batch_reports_each_outcome_and_exits_1() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/mixed-good.bin", Behavior::default());
    server.mount_route("/mixed-bad.bin", Behavior::default().with_status(404));
    let scratch = Scratch::new("mixed-batch");
    let good = server.url("/mixed-good.bin");
    let bad = server.url("/mixed-bad.bin");

    let output = run_in(&scratch, &["download", "-r", "0", &good, &bad]).await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "partial failure exits 1, not 0 and not 2; stdout: {}",
        stdout(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("1 completed, 1 failed, 0 interrupted"),
        "the summary counts both outcomes: {text}"
    );
    assert_eq!(
        std::fs::read(scratch.path().join("mixed-good.bin"))
            .expect("the good file finalized")
            .as_slice(),
        fixture.bytes().as_slice()
    );
    assert!(
        !scratch.path().join("mixed-bad.bin").exists(),
        "the failed download never finalizes"
    );
}

#[tokio::test]
async fn retry_requeues_only_the_failed_record_and_completes_it() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/retry-good.bin", Behavior::default());
    // Three failing answers cover the first invocation's probe phase
    // (HEAD and the evidence request, whatever order the engine sends
    // them in); from the retry invocation on, the route is healthy.
    server.mount_route(
        "/retry-broken.bin",
        Behavior::default().with_fail_first(3, 503),
    );
    let scratch = Scratch::new("retry");
    let good = server.url("/retry-good.bin");
    let broken = server.url("/retry-broken.bin");

    // Invocation 1: the good URL completes, the broken one fails (budget 0).
    let first = run_in(&scratch, &["download", "-r", "0", &good, &broken]).await;
    assert_eq!(
        first.status.code(),
        Some(1),
        "the 503 download fails with no retries; stdout: {}",
        stdout(&first)
    );
    assert!(
        stdout(&first).contains("1 completed, 1 failed"),
        "summary: {}",
        stdout(&first)
    );
    let good_hits = server.route_hits("/retry-good.bin");

    // Invocation 2: `retry` requeues exactly the failed record.
    let second = run_in(&scratch, &["retry"]).await;
    assert_eq!(
        second.status.code(),
        Some(0),
        "the requeued download completes; stderr: {}",
        stderr(&second)
    );
    let text = stdout(&second);
    assert!(
        text.contains("1 completed, 0 failed, 0 interrupted of 1"),
        "retry drives exactly its selected record: {text}"
    );
    assert_eq!(
        server.route_hits("/retry-good.bin"),
        good_hits,
        "retry never re-fetches unrelated completed work"
    );
    assert_eq!(
        std::fs::read(scratch.path().join("retry-broken.bin"))
            .expect("the retried file finalized")
            .as_slice(),
        fixture.bytes().as_slice()
    );

    // A second retry has nothing to do: no failed records remain.
    let third = run_in(&scratch, &["retry"]).await;
    assert_eq!(
        third.status.code(),
        Some(0),
        "an empty retry selection is a successful no-op: {}",
        stdout(&third)
    );
    assert!(
        stdout(&third).contains("nothing to retry"),
        "retry selects only failed records: {}",
        stdout(&third)
    );
}

#[tokio::test]
async fn resume_and_status_are_honest_when_there_is_nothing_to_do() {
    let server = MockServer::start().await;
    server.mount_route("/finished.bin", Behavior::default());
    let scratch = Scratch::new("resume-empty");
    let url = server.url("/finished.bin");

    let download = run_in(&scratch, &["download", &url]).await;
    assert_eq!(download.status.code(), Some(0), "{}", stderr(&download));
    let hits = server.route_hits("/finished.bin");

    // A completed record is not resumable and not retryable: both commands
    // select nothing, exit 0, and never touch the network again.
    for command in ["resume", "retry"] {
        let output = run_in(&scratch, &[command]).await;
        assert_eq!(
            output.status.code(),
            Some(0),
            "{command} with an empty selection succeeds; stderr: {}",
            stderr(&output)
        );
        let text = stdout(&output);
        assert!(
            text.contains(&format!("nothing to {command}")),
            "{command} says so when nothing is eligible: {text}"
        );
    }
    assert_eq!(
        server.route_hits("/finished.bin"),
        hits,
        "neither command re-fetched the completed download"
    );

    // An untouched database says so plainly.
    let empty = Scratch::new("status-empty");
    let status = run_in(&empty, &["status"]).await;
    assert_eq!(status.status.code(), Some(0));
    assert!(
        stdout(&status).contains("no downloads on record"),
        "the empty queue is honest: {}",
        stdout(&status)
    );
}

#[tokio::test]
async fn slow_transfers_render_bounded_redacted_progress_lines() {
    let server = MockServer::start().await;
    // 8 KiB in 512-byte frames, 150 ms per frame: a ~2.4 s transfer, so the
    // 250 ms progress sweep samples it repeatedly while bytes are moving.
    let fixture = Fixture::new(2, 8_192);
    server.mount_route(
        "/slow.bin",
        Behavior::default()
            .with_fixture(fixture)
            .with_delay_body_ms(150),
    );
    let scratch = Scratch::new("progress");
    // A query string must never survive onto the display surface.
    let url = format!("{}?token=secret", server.url("/slow.bin"));

    let output = run_in(&scratch, &["download", "-s", "1", &url]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "the slow download still completes; stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    let progress_lines: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("downloading "))
        .collect();
    assert!(
        !progress_lines.is_empty(),
        "a multi-second transfer renders live progress lines: {text}"
    );
    for line in &progress_lines {
        assert!(
            !line.contains("secret"),
            "progress lines are redacted: {line}"
        );
        assert!(
            line.contains("of 8.0 KiB (") && line.contains('%'),
            "a known size renders with a percent: {line}"
        );
        assert!(
            !line.contains("101%"),
            "the percent never exceeds 100: {line}"
        );
    }
    let final_line = text
        .lines()
        .last()
        .expect("the summary line is always printed");
    assert!(
        final_line.contains("1 completed, 0 failed, 0 interrupted"),
        "the summary follows the progress lines: {text}"
    );
    assert_eq!(
        std::fs::read(scratch.path().join("slow.bin"))
            .expect("final file")
            .as_slice(),
        fixture.bytes().as_slice()
    );
}

#[tokio::test]
async fn no_extract_keeps_the_archive_unextracted() {
    let server = MockServer::start().await;
    // A real ZIP payload (one small entry), served raw.
    let payload = build_zip_payload();
    server.mount_route(
        "/payload.zip",
        Behavior::default().with_raw_body(payload.clone()),
    );
    let scratch = Scratch::new("no-extract");
    let url = server.url("/payload.zip");

    let output = run_in(&scratch, &["download", "--no-extract", &url]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "the archive downloads and finalizes; stderr: {}",
        stderr(&output)
    );
    let downloaded =
        std::fs::read(scratch.path().join("payload.zip")).expect("the archive was preserved");
    assert_eq!(
        downloaded, payload,
        "--no-extract keeps the archive byte-identical in place"
    );
    assert!(
        !scratch.path().join("payload").exists() && !scratch.path().join("inside.txt").exists(),
        "--no-extract extracts nothing"
    );
}

#[tokio::test]
async fn automatic_extraction_is_the_default_for_archives() {
    let server = MockServer::start().await;
    let payload = build_zip_payload();
    server.mount_route("/payload.zip", Behavior::default().with_raw_body(payload));
    let scratch = Scratch::new("auto-extract");
    let url = server.url("/payload.zip");

    let output = run_in(&scratch, &["download", &url]).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let extracted = scratch.path().join("payload").join("inside.txt");
    assert!(
        extracted.exists(),
        "the default run extracts the archive next to it"
    );
    assert_eq!(
        std::fs::read(&extracted).expect("entry content"),
        b"extracted content".as_slice(),
        "the extracted entry holds its content"
    );
    assert!(
        scratch.path().join("payload.zip").exists(),
        "the archive itself is preserved after extraction"
    );
}

/// Builds a one-entry DEFLATE ZIP archive in memory (the same shape the
/// Phase 10 engine tests use).
fn build_zip_payload() -> Vec<u8> {
    use std::io::Write as _;

    // The scratch binding must outlive the write: the temporary's own Drop
    // would remove the directory the archive is about to be written to.
    let scratch = Scratch::new("zip-build");
    let archive_path = scratch.path().join("payload.zip");
    {
        let file = std::fs::File::create(&archive_path).expect("archive created");
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("inside.txt", options)
            .expect("entry started");
        zip.write_all(b"extracted content").expect("entry written");
        zip.finish().expect("archive finished");
    }
    let payload = std::fs::read(&archive_path).expect("archive read");
    std::fs::remove_file(&archive_path).expect("build scratch removed");
    drop(scratch);
    payload
}

// -------------------------------------------------------------------------
// Unrecoverable application failure: a database home that cannot exist.
// -------------------------------------------------------------------------

#[tokio::test]
async fn an_unusable_database_location_exits_1_with_an_actionable_error() {
    let scratch = Scratch::new("blocked-db");
    // A file stands where the database's directory would need to be; the
    // repository must refuse in a typed, actionable way (Windows: the
    // create fails because a file occupies the path).
    let blocked = scratch.path().join("blocked.txt");
    std::fs::write(&blocked, "a file, not a directory").expect("the blocking file is created");
    let database = blocked.join("queue.sqlite3");

    let output = run_with_env(
        &scratch,
        &["status"],
        ("SWIFT_FETCH_DATABASE", database.to_str().unwrap()),
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "a fatal environment failure is not an invalid invocation"
    );
    assert!(
        stdout(&output).is_empty(),
        "no fake results: {}",
        stdout(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("error:") && err.contains("database"),
        "the error names the database problem: {err}"
    );
}
