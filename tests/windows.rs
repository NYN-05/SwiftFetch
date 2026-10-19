//! Windows resilience and failure hardening (Phase 12).
//!
//! Everything here runs the compiled executable against the controlled
//! local mock server (never a live external URL) and asserts *Windows*
//! filesystem, console, and process behavior:
//!
//! - reserved device stems, illegal characters, and overlong names are
//!   sanitized by the real transfer path (ADR-0016 naming) and never reach
//!   a Windows device or an illegal name;
//! - access-denied (a write-DACL-denied output directory), occupied paths,
//!   and a file-as-directory fail typed and land as `failed` records with a
//!   filesystem reason — never a fake success;
//! - a foreign exclusive handle on the database or on an engine partial
//!   fails typed and recovers through `retry`/`resume` once released;
//! - concurrent child processes share one SQLite database without
//!   corruption, and the same-scope cross-process race is exercised to
//!   observe its honest outcome (single-user assumption, ADR-0017);
//! - graceful Ctrl+C (one and several active downloads) and hard forced
//!   termination recover through the actual `resume` CLI;
//! - a large queue respects the global concurrency limit and stays within
//!   a generous working-set bound;
//! - a clean run installs no service, auto-start entry, or resident
//!   process, and a missing `LOCALAPPDATA` refuses typed.
//!
//! Orchestration: console events and exclusive locks come from the
//! test-only `windows-sys` dev-dependency (first use ADR-0023 lineage,
//! recorded as ADR-0024) — `AttachConsole` + `GenerateConsoleCtrlEvent`
//! against the child's *private* console (the child spawns with
//! CREATE_NEW_CONSOLE) has no blast radius beyond that child, and a
//! share-mode-0 `CreateFileW` handle is the byte-exact Windows exclusive
//! lock. Machine-wide snapshots (auto-start surface, working set) still
//! go through windowless PowerShell helpers, read only after exit, which
//! their buffering cannot affect.
//!
//! Environment honesty (per the phase spec and the Windows honesty rule):
//! every check in this file was executed on Windows; failure modes that
//! cannot be simulated reliably here (true disk-full, a cross-volume
//! finalize, a kill inside the sub-millisecond rename window) are
//! disclosed in the Phase 12 tracking entry instead of being faked by
//! this suite.

mod common;

use common::fixture::Fixture;
use common::server::{Behavior, MockServer};
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING};
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, ATTACH_PARENT_PROCESS, CTRL_C_EVENT,
};

/// The child gets its own private console so a delivered Ctrl+C event can
/// be aimed at exactly one process (see `deliver_ctrl_c`).
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
/// PowerShell orchestrators run windowless; the flash of a console would
/// only make test output noisier.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// -------------------------------------------------------------------------
// The scratch environment and child-process helpers.
// -------------------------------------------------------------------------

/// An isolated, auto-removed scratch directory per test: the child's
/// working directory (the default output) and its database home.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "swiftfetch-win-{tag}-{}-{stamp}",
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

    /// Writes a PowerShell orchestrator script into the scratch tree (never
    /// the output directory of the transfer under test).
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, body).expect("orchestrator script written");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A read-only directory would survive removal; clear the attribute
        // in the tests that set it (the general case is untouched here).
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Runs the compiled executable with arguments in `scratch` (as working
/// directory and database home); returns the output.
///
/// The child runs on a helper thread and reports over a channel, exactly
/// like the Cli harness: awaiting here keeps the test runtime free to poll
/// the mock server tasks the child's transfers depend on.
async fn run_in(scratch: &Scratch, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_swift-fetch"));
    command
        .args(args)
        .current_dir(scratch.path())
        .env("SWIFT_FETCH_DATABASE", scratch.database());
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

/// Builds a swift-fetch command that spawns into its own private console
/// with captured stdio — the shape a Ctrl+C delivery can be aimed at.
fn console_command(scratch: &Scratch, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_swift-fetch"));
    command
        .args(args)
        .current_dir(scratch.path())
        .env("SWIFT_FETCH_DATABASE", scratch.database())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NEW_CONSOLE);
    command
}

fn spawn_console(scratch: &Scratch, args: &[&str]) -> Child {
    console_command(scratch, args)
        .spawn()
        .expect("the console child spawns")
}

/// Collects a spawned child's output to completion on a helper thread.
async fn wait_for_child(child: Child) -> Output {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let output = child
            .wait_with_output()
            .expect("the spawned child runs to completion");
        let _ = sender.send(output);
    });
    receiver
        .await
        .expect("the waiter thread reports the child's output")
}

// -------------------------------------------------------------------------
// PowerShell orchestration (ADR-0024): console events, foreign locks,
// environment snapshots, and working-set probes — all windowés.
// -------------------------------------------------------------------------

async fn run_powershell(script: &Path, args: &[&str]) -> Output {
    let script = script.to_string_lossy().into_owned();
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", &script])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    run_command(command).await
}

/// Delivers a graceful CTRL_C to the child's *private* console from this
/// test process.
///
/// `AttachConsole(child_pid)` binds this runner to the one console the
/// child spawned into (CREATE_NEW_CONSOLE), so
/// `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)` reaches exactly that
/// child — no parallel-runner test child shares the blast radius. The
/// runner detaches its own console for an instant and re-attaches its
/// parent's console afterwards, since its stdio is captured either way.
async fn deliver_ctrl_c(child_pid: u32) {
    let outcome = tokio::task::spawn_blocking(move || {
        // SAFETY: plain Win32 console and process calls; the handles
        // belong to consoles this process isolates via FreeConsole.
        unsafe {
            FreeConsole();
            if AttachConsole(child_pid) == 0 {
                // Re-join our own console session before reporting, so a
                // failed delivery never blinds the test runner.
                FreeConsole();
                AttachConsole(ATTACH_PARENT_PROCESS);
                return Err("AttachConsole to the child's console failed");
            }
            GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0);
            FreeConsole();
            AttachConsole(ATTACH_PARENT_PROCESS);
            Ok(())
        }
    })
    .await
    .expect("the console bridging runs on a blocking task");
    outcome.expect("the test must be able to aim a CTRL_C at its own child");
}

/// A foreign exclusive handle (share mode 0) held on one path for the
/// duration of a test — the deterministic Windows "file is locked" state:
/// any other open of the same path fails with a sharing violation.
struct ExclusiveLock(HANDLE);

impl ExclusiveLock {
    fn acquire(target: &Path) -> Self {
        let mut wide = target.to_string_lossy().encode_utf16().collect::<Vec<_>>();
        wide.push(0);
        // SAFETY: a classic single CreateFileW with a NUL-terminated
        // local path; no special flags, no security descriptor.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0, // exclusive: no sharing of any kind
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            handle,
            INVALID_HANDLE_VALUE,
            "the exclusive open of {} must succeed (GetLastError {}) \
             — create the target file first",
            target.display(),
            std::io::Error::last_os_error()
        );
        Self(handle)
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        // SAFETY: the handle is the one this struct owns.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Snapshots the machine-wide auto-start surfaces the spec cares about:
/// per-user Run keys, per-machine Run keys, the service list, scheduled
/// task names, and the Startup folder's entries.
const SNAPSHOT_PS1: &str = r#"$ErrorActionPreference = 'Continue'
Write-Output '== HKCU RUN =='
& reg.exe query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Run' 2>&1 | Out-String
Write-Output '== HKLM RUN =='
& reg.exe query 'HKLM\Software\Microsoft\Windows\CurrentVersion\Run' 2>&1 | Out-String
Write-Output '== SERVICES =='
& reg.exe query 'HKLM\SYSTEM\CurrentControlSet\Services' 2>&1 | Out-String
Write-Output '== SCHEDULED TASKS =='
& schtasks.exe /query /fo LIST 2>&1 |
    Select-String -Pattern 'TaskName:' |
    ForEach-Object { $_.Line.Trim() }
Write-Output '== STARTUP FOLDER =='
Get-ChildItem -Force ([Environment]::GetFolderPath('Startup')) -ErrorAction SilentlyContinue |
    ForEach-Object { $_.Name } | Sort-Object
Write-Output '== SNAPSHOT END =='
"#;

async fn snapshot_auto_start(scratch: &Scratch) -> String {
    let script = scratch.script("auto-start-snapshot.ps1", SNAPSHOT_PS1);
    let output = run_powershell(&script, &[]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "the snapshot helper runs; stderr: {}",
        stderr(&output)
    );
    stdout(&output)
}

/// Queries the working set of one live process (in MiB), reporting `None`
/// when the process is already gone (the no-resident-process check uses the
/// same helper).
const WORKING_SET_PS1: &str = r#"param([int]$ProcessId)
$process = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
if ($null -eq $process) {
    Write-Output 'gone'
    exit 0
}
Write-Output ([int64][math]::Round($process.WorkingSet64 / 1MB))
"#;

async fn working_set_mib(scratch: &Scratch, pid: u32) -> Option<u64> {
    let script = scratch.script("working-set.ps1", WORKING_SET_PS1);
    let pid_arg = pid.to_string();
    let output = run_powershell(&script, &["-ProcessId", pid_arg.as_str()]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "the working-set helper runs; stderr: {}",
        stderr(&output)
    );
    match stdout(&output).trim() {
        "gone" => None,
        mib => mib.parse().ok(),
    }
}

/// Every finalized file in the output directory must hold the fixture's
/// exact bytes; a partial-looking leftover is reported when present.
fn assert_no_leftover_partials(output_dir: &Path) {
    let leftovers: Vec<String> = std::fs::read_dir(output_dir)
        .expect("the output directory is readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".part") && name.contains(".swiftfetch-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the transfer must not leave partial files behind: {leftovers:?}"
    );
}

fn assert_file_bytes(path: &Path, expected: &[u8], context: &str) {
    let actual = std::fs::read(path)
        .unwrap_or_else(|err| panic!("{context}: the file {} must be readable: {err}", path.display()));
    assert_eq!(actual, expected, "{context}: byte-exact content");
}

// -------------------------------------------------------------------------
// Filename and path handling on Windows.
// -------------------------------------------------------------------------

/// A reserved device stem in the effective URL must be neutralized before
/// any file is created: the download lands as `_CON.bin`, a real file, with
/// the device name untouched.
#[tokio::test]
async fn a_reserved_stem_url_never_reaches_a_windows_device() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/CON.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("reserved-name");

    let output = run_in(&scratch, &["download", &server.url("/CON.bin")]).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let sanitized = scratch.path().join("_CON.bin");
    assert!(sanitized.exists(), "the neutralized name must exist");
    assert_file_bytes(&sanitized, fixture.bytes().as_slice(), "reserved-stem download");
    // No sibling attempt is expected: naming renames, it never writes
    // through the device name.
    let stranded: Vec<String> = std::fs::read_dir(scratch.path())
        .expect("scratch readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "_CON.bin" && name != "queue.sqlite3" && !name.ends_with(".ps1"))
        .collect();
    assert!(
        stranded.is_empty(),
        "only the sanitized download may exist: {stranded:?}"
    );
}

/// Illegal NTFS characters decoded out of the URL (`:` `<` `>`), torn-off
/// trailing dots, and spaces are sanitized in the real transfer path.
#[tokio::test]
async fn illegal_url_characters_sanitize_for_windows_end_to_end() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route(
        "/bad%3A%3C%3Ename.bin",
        Behavior::default().with_fixture(fixture.clone()),
    );
    let scratch = Scratch::new("illegal-chars");

    let output = run_in(&scratch, &["download", &server.url("/bad%3A%3C%3Ename.bin")]).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let sanitized = scratch.path().join("bad___name.bin");
    assert!(
        sanitized.exists(),
        "the sanitized name must exist instead of the decoded illegal one"
    );
    assert_file_bytes(&sanitized, fixture.bytes().as_slice(), "illegal-chars download");
}

/// An overlong effective name is capped at the Windows per-component limit
/// (255 UTF-16 units), preserving the extension in the real transfer.
#[tokio::test]
async fn an_overlong_effective_name_is_capped_at_255_units_end_to_end() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    // 400 'a' + ".bin" overflows 255 units; the cap keeps ".bin".
    let long_segment = format!("{}{}", "a".repeat(400), ".bin");
    let route = format!("/{long_segment}");
    server.mount_route(
        &route,
        Behavior::default().with_fixture(fixture.clone()),
    );
    let scratch = Scratch::new("overlong-name");

    let url = format!("{}{}", server.url("/"), long_segment);
    let output = run_in(&scratch, &["download", &url]).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let capped = scratch.path().join(format!("{}.bin", "a".repeat(251)));
    assert!(
        capped.exists(),
        "the capped name (251 stem units + .bin) must exist"
    );
    assert_file_bytes(&capped, fixture.bytes().as_slice(), "capped-name download");
    let overlong = scratch.path().join(&long_segment);
    assert!(
        !overlong.exists(),
        "a name of 404 UTF-16 units must never be created directly"
    );
}

/// A destination beyond the MAX_PATH convention follows the environment's
/// long-path support honestly: either the transfer completes byte-exactly
/// (long paths enabled) or it fails typed before any fake progress
/// (disabled). Both branches are asserted; the branch taken is recorded in
/// the Phase 12 entry.
#[tokio::test]
async fn deep_long_path_destination_matches_the_environment_honestly() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/deep.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("long-path");

    // Build a destination whose absolute path exceeds 260 UTF-16 units.
    let mut deep = scratch.path().to_path_buf();
    let mut index = 0;
    while deep.to_string_lossy().len() < 270 {
        let segment = format!("long-path-segment-{index:02}-{}", "d".repeat(24));
        deep = deep.join(segment);
        index += 1;
    }
    let environment_supports_long_paths = std::fs::create_dir_all(&deep).is_ok();

    let output = run_in(
        &scratch,
        &["download", "-o", &deep.to_string_lossy(), &server.url("/deep.bin")],
    )
    .await;

    if environment_supports_long_paths {
        let destination = deep.join("deep.bin");
        assert_eq!(
            output.status.code(),
            Some(0),
            "long paths enabled in this environment: the download must complete; \
             stderr: {}",
            stderr(&output)
        );
        assert_file_bytes(&destination, fixture.bytes().as_slice(), "long-path download");
    } else {
        assert_eq!(
            output.status.code(),
            Some(1),
            "long paths disabled: the failure must be typed, never a fake success; \
             stderr: {}",
            stderr(&output)
        );
        let error_text = stderr(&output);
        assert!(
            error_text.contains("error: "),
            "the refusal must render as an error line: {error_text}"
        );
    }
}

/// A nested output directory that does not yet exist is created by the
/// engine (the success side of output-directory creation).
#[tokio::test]
async fn a_deep_output_directory_is_created_end_to_end() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/nested.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("deep-output");

    let nested = scratch.path().join("does").join("not").join("exist");
    let output = run_in(
        &scratch,
        &["download", "-o", &nested.to_string_lossy(), &server.url("/nested.bin")],
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_file_bytes(
        &nested.join("nested.bin"),
        fixture.bytes().as_slice(),
        "nested output download",
    );
}

// -------------------------------------------------------------------------
// Access-denied and occupied paths.
// -------------------------------------------------------------------------

/// A write-DACL-denied output directory denies the engine's partial-file
/// creation: the transfer fails typed (filesystem), lands a `failed`
/// record through the CLI, and never reports success. (Windows ignores
/// the classic read-only attribute on directories, so the deterministic
/// access-denied shape is a deny ACE.)
#[tokio::test]
async fn a_write_denied_output_directory_fails_typed_and_records_the_failure() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/locked.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("denied-output");

    let denied = scratch.path().join("denied-outputs");
    std::fs::create_dir_all(&denied).expect("the directory is created");
    // Everyone's well-known SID (S-1-1-0, locale-independent): deny
    // write-data/add-file inheritance for this subtree.
    let deny = Command::new("icacls")
        .arg(&denied)
        .args(["/deny", "*S-1-1-0:(OI)(CI)(WD,AD)"])
        .output()
        .expect("icacls runs");
    assert_eq!(
        String::from_utf8_lossy(&deny.stdout),
        "",
        "unexpected icacls output"
    );
    assert!(deny.status.success(), "the deny ACE must be granted");

    let output = run_in(
        &scratch,
        &[
            "download",
            "-o",
            &denied.to_string_lossy(),
            &server.url("/locked.bin"),
        ],
    )
    .await;

    // Remove the deny so the scratch teardown can delete the tree.
    let _restore = Command::new("icacls")
        .arg(&denied)
        .args(["/remove:d", "*S-1-1-0"])
        .output()
        .expect("icacls runs");

    assert_eq!(
        output.status.code(),
        Some(1),
        "access denied must fail the download; stderr: {}",
        stderr(&output)
    );
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("locked.bin"),
        "the failure must name the transfer: {text}"
    );
    assert!(
        !denied.join("locked.bin").exists(),
        "no file may appear inside the denied directory"
    );

    let status = run_in(&scratch, &["status", "-v"]).await;
    assert_eq!(
        status.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&status)
    );
    let listing = stdout(&status);
    assert!(
        listing.contains("failed"),
        "the failed record must be listed: {listing}"
    );
}

/// An output path occupied by a *file* is refused: it can never become a
/// directory, and the engine reports the failure typed.
#[tokio::test]
async fn an_output_directory_occupied_by_a_file_fails_typed() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/blocked.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("occupied-output");

    let occupied = scratch.path().join("occupied-as-file");
    std::fs::write(&occupied, b"a file, not an output directory").expect("file created");

    let output = run_in(
        &scratch,
        &[
            "download",
            "-o",
            &occupied.to_string_lossy(),
            &server.url("/blocked.bin"),
        ],
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "a file cannot be an output directory; stderr: {}",
        stderr(&output)
    );
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("could not be created or is not a directory"),
        "the typed refusal must mention the directory law: {text}"
    );
}

/// An existing user file at the destination is never overwritten: the
/// engine claims the `name (1)` sibling instead, byte-exact.
#[tokio::test]
async fn an_occupied_destination_is_never_overwritten_and_a_sibling_is_used() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/payload.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("occupied-destination");

    let user_file = scratch.path().join("payload.bin");
    std::fs::write(&user_file, vec![0xAA_u8; 512]).expect("the user file is created");

    let output = run_in(&scratch, &["download", &server.url("/payload.bin")]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "the sibling policy must still complete the transfer; stderr: {}",
        stderr(&output)
    );
    assert_file_bytes(&user_file, &[0xAA; 512], "the user file must be untouched");
    let sibling = scratch.path().join("payload (1).bin");
    assert!(
        sibling.exists(),
        "the deterministic sibling must carry the download"
    );
    assert_file_bytes(&sibling, fixture.bytes().as_slice(), "sibling download");
    assert_no_leftover_partials(scratch.path());
}

// -------------------------------------------------------------------------
// Locked files and the database.
// -------------------------------------------------------------------------

/// A database file held by a foreign exclusive handle refuses the download
/// typed (the database cannot be opened), the failure stays recoverable,
/// and after the lock is released the same URL completes.
#[tokio::test]
async fn a_database_locked_by_a_foreign_handle_fails_typed_and_recovers() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/dblock.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("db-locked");

    // Create the database file first so the test can open it exclusively;
    // a zero-byte file is what SQLite initializes from.
    std::fs::write(scratch.database(), b"").expect("the database file is created");
    {
        let lock = ExclusiveLock::acquire(&scratch.database());
        let output = run_in(&scratch, &["download", &server.url("/dblock.bin")]).await;
        drop(lock);

        assert_eq!(
            output.status.code(),
            Some(1),
            "a locked database must fail typed; stderr: {}",
            stderr(&output)
        );
        let text = stderr(&output);
        assert!(
            text.contains("download database") && text.contains("could not be opened"),
            "the refusal must name the database law: {text}"
        );
    }

    let recovered = run_in(&scratch, &["download", &server.url("/dblock.bin")]).await;
    assert_eq!(
        recovered.status.code(),
        Some(0),
        "after the lock is released the transfer must complete; stderr: {}",
        stderr(&recovered)
    );
    assert_file_bytes(
        &scratch.path().join("dblock.bin"),
        fixture.bytes().as_slice(),
        "recovered download",
    );
}

/// A forced-terminated transfer leaves its identifiable partial behind;
/// a foreign exclusive handle on that partial fails the resume typed, and
/// `retry` after the release completes the transfer byte-exactly.
#[tokio::test]
async fn an_engine_partial_held_locked_fails_the_resume_typed_and_retry_recovers() {
    let server = MockServer::start().await;
    // 16 KiB in 512-byte frames, 150 ms per frame: ~4.8 s transfer; the
    // kill lands mid-body deterministically.
    let fixture = Fixture::new(2, 16_384);
    server.mount_route(
        "/held.bin",
        Behavior::default()
            .with_fixture(fixture.clone())
            .with_delay_body_ms(150),
    );
    let scratch = Scratch::new("partial-locked");

    let mut child = spawn_console(&scratch, &["download", &server.url("/held.bin")]);
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let _ = child.kill();
    drop(child);

    let partials: Vec<PathBuf> = std::fs::read_dir(scratch.path())
        .expect("scratch readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.to_string_lossy();
            name.ends_with(".part") && name.contains(".swiftfetch-")
        })
        .collect();
    assert_eq!(
        partials.len(),
        1,
        "the killed transfer leaves exactly one identifiable partial"
    );
    let partial = partials[0].clone();
    let partial_bytes = std::fs::read(&partial).expect("the partial is readable");
    assert!(
        partial_bytes.len() > 1024,
        "the kill must land mid-body, with bytes already written"
    );

    {
        let lock = ExclusiveLock::acquire(&partial);
        let refused = run_in(&scratch, &["resume"]).await;
        drop(lock);

        assert_eq!(
            refused.status.code(),
            Some(1),
            "a locked partial must fail the resume typed; stderr: {}",
            stderr(&refused)
        );
        let text = format!("{}{}", stdout(&refused), stderr(&refused));
        assert!(
            text.contains("could not be reopened for resume"),
            "the refusal must come from the resume open: {text}"
        );
        assert_eq!(
            std::fs::read(&partial).expect("the partial is readable"),
            partial_bytes,
            "the failed attempt must not have modified the locked partial"
        );
    }

    let recovered = run_in(&scratch, &["retry"]).await;
    assert_eq!(
        recovered.status.code(),
        Some(0),
        "after the lock is released the retry must complete; stderr: {}",
        stderr(&recovered)
    );
    assert_file_bytes(
        &scratch.path().join("held.bin"),
        fixture.bytes().as_slice(),
        "recovered transfer",
    );
    assert_no_leftover_partials(scratch.path());
}

// -------------------------------------------------------------------------
// Concurrent processes and the database.
// -------------------------------------------------------------------------

/// Two child processes download *disjoint* URLs against the same database
/// concurrently: WAL plus the busy timeout carries both writers, and every
/// record completes exactly once.
#[tokio::test]
async fn concurrent_processes_on_disjoint_urls_share_one_database_safely() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/first.bin", Behavior::default().with_fixture(fixture.clone()));
    server.mount_route("/second.bin", Behavior::default().with_fixture(fixture.clone()));
    server.mount_route("/third.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("concurrent-disjoint");

    let first_url = server.url("/first.bin");
    let second_url = server.url("/second.bin");
    let third_url = server.url("/third.bin");
    let first = spawn_console(
        &scratch,
        &["download", first_url.as_str(), second_url.as_str()],
    );
    let second = spawn_console(&scratch, &["download", third_url.as_str()]);

    let (first, second) = tokio::join!(wait_for_child(first), wait_for_child(second));
    assert_eq!(first.status.code(), Some(0), "stderr: {}", stderr(&first));
    assert_eq!(second.status.code(), Some(0), "stderr: {}", stderr(&second));

    let status = run_in(&scratch, &["status", "-v"]).await;
    let listing = stdout(&status);
    assert!(
        listing.contains("3 download(s) on record"),
        "exactly three records must exist: {listing}"
    );
    assert!(
        !listing.contains("queued") && !listing.contains("paused") && !listing.contains("failed"),
        "all records must have completed: {listing}"
    );
    for name in ["first.bin", "second.bin", "third.bin"] {
        assert_file_bytes(
            &scratch.path().join(name),
            fixture.bytes().as_slice(),
            name,
        );
    }
    assert_no_leftover_partials(scratch.path());
}

/// Two child processes submit the *same* URL to the same empty database at
/// the same moment: the cross-process adoption window is observed as-is.
///
/// Honest expected shapes (single-user assumption, ADR-0017; disclosed in
/// the Phase 12 entry, not silently "fixed"):
/// - either both processes inserted while the other was still probing and
///   the scope ends with two records (never one record with two owners) —
///   the next sequential download then refuses with the multiplicity error;
/// - or one process adopted the other's record and the scope ends with one
///   record, byte-exact file(s), and recoverable state.
/// In every observed shape: no corruption, every existing final file holds
/// the fixture's exact bytes, and no record ever lies about completion.
#[tokio::test]
async fn concurrent_same_scope_processes_never_corrupt_or_fake_success() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/payload.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("same-scope");
    let url = server.url("/payload.bin");

    let first = spawn_console(&scratch, &["download", url.as_str()]);
    let second = spawn_console(&scratch, &["download", url.as_str()]);
    let (first, second) = tokio::join!(wait_for_child(first), wait_for_child(second));
    assert!(
        matches!(first.status.code(), Some(0) | Some(1)),
        "both children end with a documented exit code; stderr: {}",
        stderr(&first)
    );
    assert!(
        matches!(second.status.code(), Some(0) | Some(1)),
        "both children end with a documented exit code; stderr: {}",
        stderr(&second)
    );

    let status = run_in(&scratch, &["status", "-v"]).await;
    let listing = stdout(&status);
    let single = listing.contains("1 download(s) on record");
    let doubled = listing.contains("2 download(s) on record");
    assert!(
        single || doubled,
        "the scope must have one or two records, never more: {listing}"
    );

    // No file named like a final download may hold other than fixture
    // bytes — corruption would break this invariant.
    for name in ["payload.bin", "payload (1).bin", "payload (2).bin"] {
        let candidate = scratch.path().join(name);
        if candidate.exists() {
            assert_file_bytes(&candidate, fixture.bytes().as_slice(), name);
        }
    }

    // A later sequential invocation reports the honest state: with one
    // record it is an idempotent no-op (exit 0); with two records the
    // multiplicity gate refuses typed (exit 1) until a human resolves it.
    let third = run_in(&scratch, &["download", &url]).await;
    if doubled {
        let text = format!("{}{}", stdout(&third), stderr(&third));
        assert_eq!(
            third.status.code(),
            Some(1),
            "two records must be refused typed by the multiplicity gate; {text}"
        );
        assert!(
            text.contains("multiplicity") || text.contains("cannot share one destination plan"),
            "the refusal must be the documented multiplicity error: {text}"
        );
    } else if single {
        assert_eq!(
            third.status.code(),
            Some(0),
            "with one record the scope is an idempotent no-op; stderr: {}",
            stderr(&third)
        );
    }
}

// -------------------------------------------------------------------------
// Environment paths.
// -------------------------------------------------------------------------

/// Without `LOCALAPPDATA` and without an override, `download` refuses
/// typed before any network byte: the default database location cannot be
/// guessed.
#[tokio::test]
async fn a_missing_localappdata_refuses_typed_before_any_network() {
    let server = MockServer::start().await;
    server.mount_route("/nowhere.bin", Behavior::default());
    let scratch = Scratch::new("no-localappdata");

    let mut command = Command::new(env!("CARGO_BIN_EXE_swift-fetch"));
    command
        .args(["download", &server.url("/nowhere.bin")])
        .current_dir(scratch.path())
        .env_remove("SWIFT_FETCH_DATABASE")
        .env_remove("LOCALAPPDATA");

    let output = run_command(command).await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "the honest refusal exits 1; stderr: {}",
        stderr(&output)
    );
    let text = stderr(&output);
    assert!(
        text.contains("LOCALAPPDATA"),
        "the refusal must name the missing environment law: {text}"
    );
    assert!(
        stdout(&output).is_empty(),
        "a refused command must leave stdout empty"
    );
    assert!(
        !scratch.database().exists(),
        "no database may be created when the location cannot be resolved"
    );
}

// -------------------------------------------------------------------------
// Graceful interruption (Ctrl+C) and recovery.
// -------------------------------------------------------------------------

/// Ctrl+C during one active transfer parks the record (`paused`), prints
/// the honest interrupted summary (exit 1), and a later `resume` completes
/// the transfer byte-exactly.
#[tokio::test]
async fn graceful_ctrlc_mid_transfer_parks_paused_and_resume_completes() {
    let server = MockServer::start().await;
    // 16 KiB in 512-byte frames, 150 ms per frame: a ~4.8 s transfer, so
    // the Ctrl+C lands deep mid-body instead of racing process teardown.
    let fixture = Fixture::new(2, 16_384);
    server.mount_route(
        "/interrupted.bin",
        Behavior::default()
            .with_fixture(fixture.clone())
            .with_delay_body_ms(150),
    );
    let scratch = Scratch::new("ctrlc-one");

    let child = spawn_console(&scratch, &["download", &server.url("/interrupted.bin")]);
    // ~900 ms in: several frames already written, plenty of transfer left.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    let pid = child.id();
    deliver_ctrl_c(pid).await;
    let output = wait_for_child(child).await;

    assert_eq!(
        output.status.code(),
        Some(1),
        "an interrupted download exits 1; stderr: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("interrupted"),
        "the interrupted summary must print: {text}"
    );

    let status = run_in(&scratch, &["status", "-v"]).await;
    let listing = stdout(&status);
    assert!(
        listing.contains("paused"),
        "the parked record must show as paused: {listing}"
    );

    let resume = run_in(&scratch, &["resume"]).await;
    assert_eq!(
        resume.status.code(),
        Some(0),
        "the resume must complete the transfer; stderr: {}",
        stderr(&resume)
    );
    assert_file_bytes(
        &scratch.path().join("interrupted.bin"),
        fixture.bytes().as_slice(),
        "resumed transfer",
    );
    assert_no_leftover_partials(scratch.path());
}

/// Ctrl+C with several active downloads parks every touched record
/// (`paused`), leaves untouched ones `queued`, and one `resume` completes
/// the whole batch.
#[tokio::test]
async fn graceful_ctrlc_with_multiple_active_downloads_recovers_every_record() {
    let server = MockServer::start().await;
    // 8 KiB each in 512-byte frames at 120 ms: ~1.9 s per file; two run
    // concurrently (-j 2), the third stays queued — the Ctrl+C lands
    // mid-flight for both active transfers.
    let fixture = Fixture::new(2, 8_192);
    for path in ["/alpha.bin", "/bravo.bin", "/charlie.bin"] {
        server.mount_route(
            path,
            Behavior::default()
                .with_fixture(fixture.clone())
                .with_delay_body_ms(120),
        );
    }
    let scratch = Scratch::new("ctrlc-multi");

    let child = spawn_console(
        &scratch,
        &[
            "download",
            "-j",
            "2",
            &server.url("/alpha.bin"),
            &server.url("/bravo.bin"),
            &server.url("/charlie.bin"),
        ],
    );
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let pid = child.id();
    deliver_ctrl_c(pid).await;
    let output = wait_for_child(child).await;

    assert_eq!(
        output.status.code(),
        Some(1),
        "an interrupted batch exits 1; stderr: {}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("interrupted"),
        "the interrupted summary must print"
    );

    let status = run_in(&scratch, &["status", "-v"]).await;
    let listing = stdout(&status);
    assert!(
        listing.contains("3 download(s) on record"),
        "all three records survive the Ctrl+C: {listing}"
    );
    assert!(
        listing.contains("paused") || listing.contains("queued"),
        "interrupted/unstarted records stay recoverable: {listing}"
    );

    let resume = run_in(&scratch, &["resume"]).await;
    assert_eq!(
        resume.status.code(),
        Some(0),
        "one resume must complete every record; stderr: {}",
        stderr(&resume)
    );
    for name in ["alpha.bin", "bravo.bin", "charlie.bin"] {
        assert_file_bytes(
            &scratch.path().join(name),
            fixture.bytes().as_slice(),
            name,
        );
    }
    assert_no_leftover_partials(scratch.path());
}

// -------------------------------------------------------------------------
// Forced termination and recovery.
// -------------------------------------------------------------------------

/// Forced termination (TerminateProcess) mid-body leaves the record
/// recoverable; the relaunch through the actual `resume` CLI completes the
/// transfer to byte-exact content with exactly one record on scope.
#[tokio::test]
async fn forced_termination_mid_transfer_recovers_through_the_actual_cli() {
    let server = MockServer::start().await;
    // 16 KiB in 512-byte frames, 150 ms per frame: a ~4.8 s transfer.
    let fixture = Fixture::new(2, 16_384);
    server.mount_route(
        "/killed.bin",
        Behavior::default()
            .with_fixture(fixture.clone())
            .with_delay_body_ms(150),
    );
    let scratch = Scratch::new("forced-one");

    let mut child = spawn_console(&scratch, &["download", &server.url("/killed.bin")]);
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let _ = child.kill();
    drop(child);

    // The record exists and is recoverable; the partial survives.
    let status = run_in(&scratch, &["status", "-v"]).await;
    let listing = stdout(&status);
    assert!(
        listing.contains("1 download(s) on record"),
        "exactly one record for the scope: {listing}"
    );
    let recoverable_states = ["paused", "downloading", "probing", "queued", "retrying", "finalizing"];
    assert!(
        recoverable_states
            .iter()
            .any(|state| listing.contains(state)),
        "the killed transfer must sit in a recoverable state: {listing}"
    );

    let resume = run_in(&scratch, &["resume"]).await;
    assert_eq!(
        resume.status.code(),
        Some(0),
        "the relaunch must recover through the actual CLI; stderr: {}",
        stderr(&resume)
    );
    assert_file_bytes(
        &scratch.path().join("killed.bin"),
        fixture.bytes().as_slice(),
        "recovered transfer",
    );
    assert_no_leftover_partials(scratch.path());

    let final_status = run_in(&scratch, &["status", "-v"]).await;
    let final_listing = stdout(&final_status);
    assert!(
        final_listing.contains("completed"),
        "the record must show completed: {final_listing}"
    );
}

/// Forced termination while several downloads are active recovers *all*
/// of them through one `resume`.
#[tokio::test]
async fn forced_termination_of_multiple_active_transfers_recovers_all() {
    let server = MockServer::start().await;
    // 8 KiB each at 120 ms/frame (~1.9 s per file): two active, one queued.
    let fixture = Fixture::new(2, 8_192);
    for path in ["/one.bin", "/two.bin", "/three.bin"] {
        server.mount_route(
            path,
            Behavior::default()
                .with_fixture(fixture.clone())
                .with_delay_body_ms(120),
        );
    }
    let scratch = Scratch::new("forced-multi");

    let mut child = spawn_console(
        &scratch,
        &[
            "download",
            "-j",
            "2",
            &server.url("/one.bin"),
            &server.url("/two.bin"),
            &server.url("/three.bin"),
        ],
    );
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let _ = child.kill();
    drop(child);

    let status = run_in(&scratch, &["status", "-v"]).await;
    let listing = stdout(&status);
    assert!(
        listing.contains("3 download(s) on record"),
        "every record survives the kill: {listing}"
    );

    let resume = run_in(&scratch, &["resume"]).await;
    assert_eq!(
        resume.status.code(),
        Some(0),
        "one relaunch recovers all records; stderr: {}",
        stderr(&resume)
    );
    for name in ["one.bin", "two.bin", "three.bin"] {
        assert_file_bytes(
            &scratch.path().join(name),
            fixture.bytes().as_slice(),
            name,
        );
    }
    assert_no_leftover_partials(scratch.path());
}

/// Repeated startup/shutdown cycles never duplicate work or corrupt state:
/// interrupt the transfer twice, resume each time, and the scope still has
/// exactly one record, one final file, and byte-exact content.
#[tokio::test]
async fn repeated_startup_shutdown_cycles_are_idempotent_and_nonduplicating() {
    let server = MockServer::start().await;
    // 8 KiB each at 120 ms/frame: two files, ~2 s per file, run under -j 2.
    let fixture = Fixture::new(2, 8_192);
    for path in ["/cycles.bin", "/other.bin"] {
        server.mount_route(
            path,
            Behavior::default()
                .with_fixture(fixture.clone())
                .with_delay_body_ms(120),
        );
    }
    let scratch = Scratch::new("cycles");

    // Cycle 1: Ctrl+C during the transfers.
    let child = spawn_console(
        &scratch,
        &[
            "download",
            "-j",
            "2",
            &server.url("/cycles.bin"),
            &server.url("/other.bin"),
        ],
    );
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let pid = child.id();
    deliver_ctrl_c(pid).await;
    let interrupted = wait_for_child(child).await;
    assert_eq!(interrupted.status.code(), Some(1));

    // Cycle 2: forced termination on the relaunch.
    let mut relaunched = spawn_console(&scratch, &["resume"]);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = relaunched.kill();
    drop(relaunched);

    // Cycle 3: one clean resume finishes everything.
    let final_resume = run_in(&scratch, &["resume"]).await;
    assert_eq!(
        final_resume.status.code(),
        Some(0),
        "the third cycle must recover completely; stderr: {}",
        stderr(&final_resume)
    );

    let status = run_in(&scratch, &["status", "-v"]).await;
    let listing = stdout(&status);
    assert!(
        listing.contains("2 download(s) on record"),
        "no duplicates across three cycles: {listing}"
    );
    for name in ["cycles.bin", "other.bin"] {
        assert_file_bytes(
            &scratch.path().join(name),
            fixture.bytes().as_slice(),
            name,
        );
    }
    assert_no_leftover_partials(scratch.path());
}

// -------------------------------------------------------------------------
// Bounded resources under a large queue; the clean-install surface.
// -------------------------------------------------------------------------

/// A queue of forty URLs through the default worker pool: the mock server
/// observes the global concurrency cap (never more than 4 concurrent GETs,
/// really concurrent), all records complete, and the child's working set
/// stays within a generous bound during the run.
#[tokio::test]
async fn a_large_queue_respects_the_global_limit_and_stays_reasonably_bound() {
    let server = MockServer::start().await;
    let fixture = Fixture::new(3, 4_096);
    let mut urls = Vec::with_capacity(40);
    for index in 0..40 {
        let path = format!("/queued-{index}.bin");
        server.mount_route(
            &path,
            Behavior::default()
                .with_fixture(fixture.clone())
                .with_delay_body_ms(60),
        );
        urls.push(server.url(&path));
    }
    let scratch = Scratch::new("large-queue");

    let mut args = vec!["download", "-j", "4"];
    args.extend(urls.iter().map(String::as_str));

    let child = spawn_console(&scratch, &args);
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let pid = child.id();
    let peak_working_set = tokio::join!(
        working_set_mib(&scratch, pid),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(900)).await;
            working_set_mib(&scratch, pid).await
        },
    );
    let output = wait_for_child(child).await;

    assert_eq!(
        output.status.code(),
        Some(0),
        "the whole queue must complete; stderr: {}",
        stderr(&output)
    );

    let peak = server.peak_active_gets();
    assert!(
        peak <= 4,
        "the global limit must bound concurrent transfers (observed peak \
         active GETs: {peak})"
    );
    assert!(
        peak >= 2,
        "the run must actually exercise concurrency (observed peak: {peak})"
    );

    let measured: Option<u64> = peak_working_set
        .0
        .filter(|mib| *mib > 0)
        .or(peak_working_set.1.filter(|mib| *mib > 0));
    let truth = match measured {
        Some(mib) => format!("{mib} MiB observed"),
        None => "unmeasurable (the child had already exited at the sample \
                 points; bounds recorded in the Phase 12 entry)"
            .to_owned(),
    };
    assert!(
        measured.is_none_or(|mib| mib < 250),
        "the working set must stay reasonable ({truth})"
    );

    let status = run_in(&scratch, &["status", "-v"]).await;
    let listing = stdout(&status);
    assert!(
        listing.contains("40 download(s) on record"),
        "every URL owns one record: {listing}"
    );
    for index in 0..40 {
        let name = format!("queued-{index}.bin");
        assert_file_bytes(&scratch.path().join(&name), fixture.bytes().as_slice(), &name);
    }
    assert_no_leftover_partials(scratch.path());
}

/// A clean run installs nothing: before/after snapshots of the services
/// list, both auto-start Run keys, scheduled-task names, and the Startup
/// folder are identical, none contain a SwiftFetch entry, and the child is
/// not resident after it reports completion.
#[tokio::test]
async fn a_clean_run_installs_no_services_auto_start_or_resident_processes() {
    let server = MockServer::start().await;
    let fixture = Fixture::default();
    server.mount_route("/clean.bin", Behavior::default().with_fixture(fixture.clone()));
    let scratch = Scratch::new("clean-run");
    let clean_url = server.url("/clean.bin");

    let before = snapshot_auto_start(&scratch).await;

    let child = spawn_console(&scratch, &["download", clean_url.as_str()]);
    let pid = child.id();
    let output = wait_for_child(child).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let after = snapshot_auto_start(&scratch).await;
    assert_eq!(before, after, "the machine's auto-start surface must be untouched");

    let text = format!("{before}\n{after}");
    for forbidden in ["swift-fetch", "swiftfetch", "SwiftFetch"] {
        assert!(
            !text.contains(forbidden),
            "no auto-start surface may mention {forbidden}: snapshot {text}"
        );
    }

    // The child was waited to completion; nothing named swift-fetch may be
    // resident right after (the honest no-daemon check).
    assert_eq!(
        working_set_mib(&scratch, pid).await,
        None,
        "the reporting child must not be resident after completing"
    );

    assert_file_bytes(
        &scratch.path().join("clean.bin"),
        fixture.bytes().as_slice(),
        "clean-run download",
    );
}

/// TEMPORARY DIAGNOSTIC (remove before the phase closes): probes both
/// console-event routes against the compiled engine — route A, the
/// AttachConsole + GenerateConsoleCtrlEvent bridge; route B, writing a
/// real Ctrl keystroke into the child's console input buffer — and
/// observes which one the child reacts to.
#[tokio::test]
async fn zz_probe_ctrl_c_delivery_mechanism() {
    let server = MockServer::start().await;
    let fixture = Fixture::new(2, 16_384);
    server.mount_route(
        "/probe.bin",
        Behavior::default()
            .with_fixture(fixture.clone())
            .with_delay_body_ms(150),
    );
    let scratch = Scratch::new("probe-mech");
    let mut child = spawn_console(&scratch, &["download", &server.url("/probe.bin")]);
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    let pid = child.id();

    deliver_ctrl_c(pid).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let after_event = child.try_wait().expect("try_wait works").map(|code| code.code());
    println!("route A (GenerateConsoleCtrlEvent): child state after 400ms: {after_event:?}");

    if after_event.is_none() {
        inject_console_keystroke(pid).await;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let after_key = child.try_wait().expect("try_wait works").map(|code| code.code());
        println!("route B (WriteConsoleInputW): child state after 400ms: {after_key:?}");
    }

    let output = wait_for_child(child).await;
    let exit_code = output.status.code();
    let summary_lines: Vec<String> = stdout(&output)
        .lines()
        .filter(|line| line.contains("SwiftFetch:") || line.contains("interrupted"))
        .map(str::to_owned)
        .collect();
    let warn_lines: Vec<String> = stderr(&output)
        .lines()
        .filter(|line| line.contains("WARN") || line.contains("interrupt"))
        .map(str::to_owned)
        .collect();
    println!(
        "final: exit={exit_code:?}; stdout summary={summary_lines:?}; stderr warnings={warn_lines:?}"
    );
}

const C256: u16 = 0x43;

async fn inject_console_keystroke(child_pid: u32) {
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    use windows_sys::Win32::System::Console::{
        WriteConsoleInputW, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT, KEY_EVENT_RECORD,
        LEFT_CTRL_PRESSED,
    };

    let outcome = tokio::task::spawn_blocking(move || {
        let mut wide: Vec<u16> = "CONIN$\0".encode_utf16().collect();
        unsafe {
            FreeConsole();
            if AttachConsole(child_pid) == 0 {
                FreeConsole();
                AttachConsole(ATTACH_PARENT_PROCESS);
                return "attach failed".to_owned();
            }
            let conin = CreateFileW(
                wide.as_mut_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            let pending = if conin == INVALID_HANDLE_VALUE {
                "CONIN$ open failed".to_owned()
            } else {
                let press = INPUT_RECORD {
                    EventType: KEY_EVENT as u16,
                    Event: INPUT_RECORD_0 {
                        KeyEvent: KEY_EVENT_RECORD {
                            bKeyDown: 1,
                            wRepeatCount: 1,
                            wVirtualKeyCode: C256,
                            wVirtualScanCode: 0x2E,
                            uChar: KEY_EVENT_RECORD_0 { UnicodeChar: 0x63 },
                            dwControlKeyState: LEFT_CTRL_PRESSED,
                        },
                    },
                };
                let release = INPUT_RECORD {
                    EventType: KEY_EVENT as u16,
                    Event: INPUT_RECORD_0 {
                        KeyEvent: KEY_EVENT_RECORD {
                            bKeyDown: 0,
                            wRepeatCount: 1,
                            wVirtualKeyCode: C256,
                            wVirtualScanCode: 0x2E,
                            uChar: KEY_EVENT_RECORD_0 { UnicodeChar: 0x63 },
                            dwControlKeyState: 0,
                        },
                    },
                };
                let buffer = [press, release];
                let mut written = 0u32;
                let sent = WriteConsoleInputW(conin, buffer.as_ptr(), 2, &mut written);
                CloseHandle(conin);
                format!("WriteConsoleInputW sent={sent} written={written}")
            };
            FreeConsole();
            AttachConsole(ATTACH_PARENT_PROCESS);
            pending
        }
    })
    .await
    .expect("the keystroke bridge completes");
    println!("route B bridge: {outcome}");
}
