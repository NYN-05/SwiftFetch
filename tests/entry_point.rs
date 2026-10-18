//! Phase 0 smoke test: the built executable starts, reports itself, and exits
//! with a success status.

use std::process::Command;

#[test]
fn executable_starts_and_exits_successfully() {
    let output = Command::new(env!("CARGO_BIN_EXE_download-manager"))
        .output()
        .expect("the compiled executable should exist and be runnable");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "executable exited with {output:?}; stdout: {stdout:?}"
    );
    assert!(
        stdout.contains("download-manager"),
        "stdout should identify the application, got: {stdout:?}"
    );
}
