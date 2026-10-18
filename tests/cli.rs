//! CLI integration tests: exercise the compiled executable to verify the
//! Phase 1 contract — accurate help, honest not-implemented dispatch, stable
//! exit codes, and early validation of obviously invalid inputs.
//!
//! These tests intentionally run the real binary instead of linking library
//! internals: the CLI *process* behavior (stdout/stderr/exit codes) is the
//! contract being tested.

use std::process::{Command, Output};

/// Runs the compiled executable with arguments; returns the output.
fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_download-manager"))
        .args(args)
        .output()
        .expect("the compiled executable should exist and be runnable")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_is_accurate_about_what_is_supported() {
    let output = run(&["--help"]);

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
        text.contains("none are wired to a transfer engine"),
        "help must disclose that commands are not functional yet: {text}"
    );
    assert!(
        !text.contains("must not promise"),
        "developer comments must never leak into user help: {text}"
    );

    let download_help = run(&["download", "--help"]);
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

#[test]
fn version_reports_crate_name_and_version() {
    let output = run(&["--version"]);

    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("download-manager"), "stdout: {text}");
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "version output must carry the package version: {text}"
    );
}

#[test]
fn missing_subcommand_is_an_invalid_invocation() {
    let output = run(&[]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "clap rejects an invocation without a subcommand (exit 2)"
    );
    assert!(stdout(&output).is_empty(), "errors go to stderr");
    assert!(!stderr(&output).is_empty());
}

#[test]
fn unknown_subcommand_is_an_invalid_invocation() {
    let output = run(&["frobnicate"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    assert!(!stderr(&output).is_empty());
}

#[test]
fn download_requires_urls_or_a_url_file() {
    let output = run(&["download"]);

    assert_eq!(output.status.code(), Some(2));
    let err = stderr(&output);
    assert!(
        err.contains("URL") && err.contains("--url-file"),
        "message must state both accepted input kinds: {err}"
    );
}

#[test]
fn download_rejects_non_http_schemes_before_dispatch() {
    for url in ["ftp://example.com/f", "example.com/f"] {
        let output = run(&["download", url]);

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

#[test]
fn download_rejects_non_positive_concurrency() {
    let output = run(&["download", "-j", "0", "https://ex.invalid:1/f"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("at least 1"),
        "message must explain the minimum: {}",
        stderr(&output)
    );

    let output = run(&["download", "-s", "0", "https://ex.invalid:1/f"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn download_rejects_empty_output_directory() {
    let output = run(&["download", "-o", "", "https://ex.invalid:1/f"]);

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

#[test]
fn download_with_valid_arguments_does_not_report_success() {
    let output = run(&["download", "https://localhost:9000/fixture.bin"]);

    assert_eq!(
        output.status.code(),
        Some(1),
        "not-implemented must not exit successfully"
    );
    assert!(
        stdout(&output).is_empty(),
        "no success output may appear on stdout; got {:?}",
        stdout(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("not implemented") && err.contains("download"),
        "stderr must explain the honest state: {err}"
    );
}

#[test]
fn queue_commands_do_not_report_success() {
    for command in ["status", "resume", "retry"] {
        let output = run(&[command]);

        assert_eq!(output.status.code(), Some(1), "{command} must exit 1");
        assert!(stdout(&output).is_empty());
        let err = stderr(&output);
        assert!(
            err.contains("not implemented") && err.contains(command),
            "{command} message must be honest: {err}"
        );
    }
}

#[test]
fn valid_options_still_fail_honestly_in_phase_1() {
    let output = run(&[
        "download",
        "--url-file",
        "urls.txt",
        "-o",
        "downloads",
        "-j",
        "2",
        "-s",
        "2",
        "-r",
        "1",
        "--no-extract",
    ]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("not implemented"),
        "documented options parse but the engine is absent: {}",
        stderr(&output)
    );
}
