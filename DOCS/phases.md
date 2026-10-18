# Development Phases — Windows-First CLI Download Manager

## 1. Purpose and Source of Truth

This document defines the controlled development lifecycle for the Windows-first CLI download manager. It is based on:

- `windows-first-cli-download-manager-project.md` — product purpose, desired features, non-goals, and success criteria.
- `architecture.md` — technology stack, module boundaries, download lifecycle, persistence, recovery, Windows behavior, and acceptance criteria.

**If a task or idea is not permitted by those two documents, it is not part of this project unless the project owner explicitly changes the source documents.** This plan decomposes the approved scope into independently testable phases; it must not expand the product scope.

## 2. Global Rules for Every Development Phase

### 2.1 Scope control

- Work only on the phase explicitly assigned.
- Do not begin later phases, even if their implementation appears convenient.
- Do not add a GUI, browser integration, website-specific plugins, video extraction, premium-host support, account management, CAPTCHA handling, cloud services, telemetry, advertising, auto-start behavior, a Windows service, or a background daemon.
- Do not add generalized frameworks or abstractions for hypothetical future requirements.
- Do not change the approved language, core stack, architecture, or feature scope without a documented technical necessity and explicit approval.
- Keep the product CLI-only.

### 2.2 Implementation rules

- Read this file, the project document, and `architecture.md` before starting a phase.
- Inspect the existing repository and tests before editing. Preserve working behavior from completed phases.
- Implement the smallest solution that satisfies the assigned phase and its acceptance criteria.
- Use Rust, Tokio, Reqwest, SQLite, Clap, structured logging, and a maintained ZIP library as specified in `architecture.md`.
- Prefer explicit types, clear error handling, bounded resources, and small modules over clever or opaque code.
- Do not silently ignore errors, use unbounded retries, or mark incomplete files as complete.
- Never trust filenames, HTTP headers, remote content, or ZIP entry paths without validation.
- Do not hard-code credentials, tokens, personal paths, or machine-specific configuration.
- Do not claim a test passed unless it was actually run and its result observed.
- Do not remove, weaken, skip, or rewrite a test merely to make the implementation pass.
- Add or update tests in the same phase as the behavior they verify.

### 2.3 Required phase handoff

At the end of each phase, provide a concise report containing:

1. What was implemented.
2. Which files were added or changed.
3. Which tests and checks were run, with actual results.
4. Any known limitations or unresolved defects.
5. Confirmation that the phase acceptance criteria are met, or a precise list of criteria that remain unmet.

**A phase is not complete while its required tests fail, its acceptance criteria are unmet, or a critical behavior is merely stubbed.** Stop at the phase boundary and wait for the next phase instruction.

### 2.4 Windows and test-environment honesty

Windows is the first target. Use Windows CI or a Windows machine for Windows-specific behavior where available. If development occurs on another operating system, report which checks were run there and which Windows-specific checks remain unverified. Do not present a Linux-only test run as proof of Windows correctness.

---

## 3. Phase Overview

| Phase | Name | Primary outcome |
|---:|---|---|
| 0 | Repository and build foundation | Reproducible Rust project that builds and tests |
| 1 | CLI shell, configuration, errors, and logging | Stable CLI contract and shared application foundations |
| 2 | Controlled HTTP test infrastructure | Repeatable local server and file fixtures for success and failure cases |
| 3 | HTTP client and resource probing | Correct URL inspection and server-capability detection |
| 4 | Sequential download and safe file finalization | One URL downloads correctly without segmentation |
| 5 | SQLite persistence and lifecycle state | Durable queue and explicit download-state transitions |
| 6 | Single-file resume, retry, and restart recovery | Sequential transfers safely resume after interruption |
| 7 | URL batches and bounded concurrent file scheduling | Multiple URLs download concurrently within limits |
| 8 | Segmented downloads | Supported large files download in validated byte ranges |
| 9 | Crash consistency and segmented recovery | Parallel transfers recover without corruption after failures |
| 10 | Safe automatic ZIP extraction | Completed ZIP archives extract safely and independently |
| 11 | User-facing CLI workflows and progress | Required commands, status reporting, and useful exit codes work end to end |
| 12 | Windows resilience and failure hardening | Critical Windows file, shutdown, and resource failure behavior is verified |
| 13 | Performance and resource validation | Repeatable benchmarks validate throughput and bounded resource use |
| 14 | Documentation and release readiness | A focused, testable CLI release with accurate user documentation |

Phases are ordered by dependency. Do not reorder them unless an actual dependency conflict is documented and approved.

---

## Phase 0 — Repository and Build Foundation

### Objective

Create a clean, reproducible Rust project foundation for the approved CLI application, without implementing download behavior.

### What needs to be done

- Inspect the repository before making changes.
- Establish the Rust package, source layout, integration-test layout, and documentation locations.
- Add the dependencies needed for the approved stack only when they are first required, or declare the minimal initial set.
- Establish formatting, linting, and test commands.
- Ensure the project builds from a clean checkout.

### How to achieve it

1. Create or verify one Rust package for the modular monolith.
2. Create `src/main.rs` and the intended module directories, but do not create large empty implementations or speculative abstractions.
3. Commit the dependency lockfile for reproducible application builds.
4. Add a minimal README with project purpose, target platform, prerequisites, and build/test commands.
5. Configure a consistent development check using `cargo fmt --check`, `cargo clippy`, and `cargo test`.
6. Ensure the default executable starts and exits predictably; do not implement real download commands in this phase.

### Tests and checks to write/run

- A basic unit or smoke test proving the application crate builds and its entry point is reachable.
- `cargo fmt --check`.
- `cargo clippy --all-targets --all-features -- -D warnings` where compatible with the selected dependencies.
- `cargo test`.
- A clean build from a fresh checkout or clean build directory.

### Required results / exit gate

- The repository has a clear, minimal Rust structure.
- The lockfile is present and tracked.
- Formatting, linting, and tests pass.
- No GUI, service, daemon, browser integration, or unrelated feature is introduced.
- There is a reproducible command sequence for building and testing the project.

---

## Phase 1 — CLI Shell, Configuration, Errors, and Logging

### Objective

Establish the application's command-line contract and common infrastructure before implementing the download engine.

### What needs to be done

- Add Clap-based CLI parsing and help output.
- Define central configuration types for output directory, concurrency settings, retry settings, and ZIP extraction preference.
- Define typed application errors.
- Add structured logging with concise normal output and optional diagnostic detail.
- Establish a small application entry point that validates arguments and dispatches commands.

### How to achieve it

1. Define only the command shapes required by the project: download input, URL-list input, status/listing, resume, retry, and relevant configuration options. Commands may be wired to implementations in later phases.
2. Choose clear option names and stable validation rules. Avoid promising flags that cannot yet work; explicitly mark incomplete commands as unavailable during development or add them when their phase arrives.
3. Separate user-facing errors from internal diagnostic context.
4. Use `tracing`/`tracing-subscriber` for diagnostics. Keep default terminal output concise.
5. Redact authentication information and sensitive URL query parameters from logs.
6. Validate obvious invalid inputs early, such as a non-positive concurrency limit or an invalid output path.

### Tests to write

- CLI tests for help output and invalid arguments.
- Tests for default configuration values and invalid values.
- Tests for error formatting and stable error categories.
- Tests confirming logs do not emit configured secrets or sensitive URL components.
- Tests confirming unsupported or not-yet-implemented commands do not falsely report success.

### Required results / exit gate

- CLI help is accurate about supported behavior.
- Invalid configuration is rejected with an actionable message.
- Shared error and logging behavior exists and is reused by later modules.
- No transfer engine or unrelated feature is implemented prematurely.
- All Phase 0 checks and new tests pass.

---

## Phase 2 — Controlled HTTP Test Infrastructure

### Objective

Create deterministic tests for HTTP behavior and failure recovery before building the real transfer engine.

### What needs to be done

- Build reusable test support using a local HTTP server or a suitable HTTP mocking library.
- Generate deterministic files of known sizes and contents.
- Simulate normal responses, range support, malformed range responses, interruptions, delays, errors, missing lengths, redirects, and changed validators.

### How to achieve it

1. Create reusable test helpers under `tests/` or a test-support module.
2. Provide endpoints or handlers that can:
   - Serve a known file with a correct `Content-Length`.
   - Support `HEAD` where appropriate.
   - Support `Range` and return correct `206 Partial Content` and `Content-Range` values.
   - Ignore a range request and return a normal full response.
   - Return an invalid or mismatched range.
   - End a connection before the expected body is complete.
   - Return configurable status codes, delays, and connection failures.
   - Change ETag or Last-Modified values between requests.
   - Omit `Content-Length` where the protocol permits it.
3. Use deterministic content or hashes so tests can verify final bytes exactly.
4. Make failure scenarios controllable and repeatable; avoid tests that rely on public internet sites or unpredictable external servers.

### Tests to write

- Tests proving each mock-server mode produces the intended status and headers.
- Fixture tests verifying generated files and expected hashes.
- Tests proving interruption and delayed-response scenarios can be reproduced reliably.
- A smoke integration test confirming the application test suite can start and stop the local server without leaked tasks or ports.

### Required results / exit gate

- Network success and failure cases can be simulated locally.
- Tests are repeatable and do not require live external services.
- The test server can assert request methods, range headers, and request counts.
- No production download implementation is added solely to satisfy test infrastructure.

---

## Phase 3 — HTTP Client and Resource Probing

### Objective

Build the shared HTTP client and reliable URL/resource inspection used by sequential and segmented downloads.

### What needs to be done

- Configure a reusable Reqwest client.
- Handle HTTP and HTTPS, redirects, timeouts, and common HTTP errors.
- Inspect available file metadata and range support.
- Capture expected size and remote validators when they are available.
- Avoid assuming all servers correctly implement `HEAD` or support ranges.

### How to achieve it

1. Create the shared HTTP client in the download/network module.
2. Configure explicit connection and request timeouts, a consistent redirect policy, and sensible connection reuse.
3. Implement resource probing using `HEAD` where useful, with a bounded range request or another safe fallback when `HEAD` is unsupported or inconclusive.
4. Treat response data as untrusted: validate status codes, parse lengths safely, and validate range metadata.
5. Record only facts actually supported by responses:
   - Expected size, if known.
   - Range support only after adequate evidence.
   - ETag and Last-Modified validators, if supplied.
   - Effective URL after permitted redirects.
6. Avoid downloading an entire file just to inspect it.
7. Ensure ranged requests use a representation suitable for byte offsets; prevent transparent content transformations from invalidating range calculations.

### Tests to write

- HTTP and HTTPS success tests using the local test setup where practical.
- Redirect tests, redirect-loop or redirect-limit tests, and timeout tests.
- `HEAD` unsupported or inaccurate fallback tests.
- Tests for missing, invalid, or conflicting `Content-Length`.
- Tests for valid and invalid range capability detection.
- Tests for absent or changed ETag/Last-Modified values.
- Tests confirming a range-ignoring server is not misclassified as supporting ranges.

### Required results / exit gate

- All network operations use the configured reusable client.
- Probe results distinguish known, unknown, and unsupported capabilities.
- Invalid or contradictory HTTP metadata is not silently trusted.
- Probe requests have bounded resource usage and timeouts.
- All probe tests pass.

---

## Phase 4 — Sequential Download and Safe File Finalization

### Objective

Implement the simplest correct download path: stream one HTTP/HTTPS resource to disk, then safely finalize it.

### What needs to be done

- Download one URL sequentially.
- Stream response bodies using bounded buffers.
- Select a safe destination filename.
- Write to an identifiable temporary or partial file.
- Validate the transfer and finalize without silently overwriting existing files.
- Report progress information internally, without requiring a polished CLI display.

### How to achieve it

1. Resolve the effective URL and metadata using Phase 3.
2. Determine a filename from trustworthy response metadata or URL information, then sanitize it for Windows filesystem rules.
3. Create the output directory only when permitted, and report filesystem errors clearly.
4. Stream bytes to a partial file rather than buffering the whole resource in memory.
5. Track bytes received and bytes written separately where needed.
6. If an expected content length is known, verify the written byte count before finalization.
7. Ensure HTTP error bodies are not mistaken for downloaded file content.
8. Finalize through a safe same-volume rename or equivalent operation, with a deterministic no-overwrite policy.
9. Preserve useful partial data and diagnostic state on failure; do not create a final completed filename for an incomplete transfer.

### Tests to write

- Small and large sequential download tests verifying byte-for-byte equality.
- Tests for a response with a known content length and one without it.
- Tests for premature EOF and mismatched expected length.
- HTTP error-status tests proving an error body is not reported as a successful file.
- Filename tests for path separators, reserved Windows names, invalid characters, long names, and missing names.
- Existing-file tests proving no silent overwrite occurs.
- Tests for missing output directory, access denied, and simulated write failures where feasible.
- Memory-behavior check confirming the implementation streams data rather than buffering an entire large file.

### Required results / exit gate

- One valid URL produces the correct final file.
- Incomplete or failed transfers never appear as completed files.
- Filename and duplicate-destination policies are deterministic and safe.
- Large transfers use bounded memory.
- The code returns structured errors that later phases can persist and report.
- All sequential download tests pass.

---

## Phase 5 — SQLite Persistence and Lifecycle State

### Objective

Persist download queue metadata and define valid state transitions before adding reliable resume and concurrent scheduling.

### What needs to be done

- Add SQLite integration and schema initialization/migration handling.
- Store downloads, segments, extraction state, and schema version using the architecture's minimal model.
- Define explicit download lifecycle states and validate state transitions.
- Keep database operations separated from HTTP and filesystem implementation details.

### How to achieve it

1. Create a storage repository API for inserting, retrieving, updating, and listing download records.
2. Add schema versioning from the first persisted release.
3. Store only necessary fields, such as URL, destination, status, expected size, validators, retry count, timestamps, and error category.
4. Add segment records with byte range, state, and confirmed progress; they may be unused until Phase 8.
5. Add extraction records with archive path, destination, state, and error details; they may be unused until Phase 10.
6. Define allowed transitions, including `queued`, `probing`, `downloading`, `paused`, `retrying`, `finalizing`, `extracting`, `completed`, `failed`, and `cancelled`.
7. Use transactions for database-level changes that must be consistent.
8. Keep operations short and do not hold a database transaction open during network transfers or long file writes.
9. Store the database under the user's local application-data directory, separate from the download directory.

### Tests to write

- Fresh database creation and schema initialization tests.
- Insert/read/update/list tests for download records.
- Tests that invalid lifecycle transitions are rejected.
- Transaction rollback tests.
- Schema-version and migration tests.
- Tests for database path initialization and handling a missing or inaccessible application-data directory.
- Tests showing network/file operations are not performed while a long-lived database transaction is held.

### Required results / exit gate

- Queue metadata persists between program runs.
- Lifecycle state transitions are explicit and validated.
- The schema is versioned and testable.
- Database errors are surfaced as typed errors and are not silently discarded.
- The database is not used to store downloaded file contents.
- All persistence tests pass.

---

## Phase 6 — Single-File Resume, Retry, and Restart Recovery

### Objective

Make sequential downloads recoverable after interruptions, network failures, process exit, and Windows restart, wherever the server and available metadata permit safe resumption.

### What needs to be done

- Persist transfer metadata and reliable progress.
- Resume an eligible partial download using HTTP range requests.
- Validate remote identity and response range before appending.
- Implement bounded retries for transient errors.
- Reconcile persistent state and partial files at startup.

### How to achieve it

1. Associate the partial file with a persistent download record.
2. Persist the expected size, effective URL, remote validators, last trustworthy progress, and current status.
3. Before resume, probe or revalidate the remote object when possible.
4. Use a `Range` request from the correct byte offset and `If-Range` with a saved validator when supported.
5. Accept resumed bytes only when the HTTP status and range metadata match the requested range.
6. If the server does not support safe resumption, validators indicate a changed file, or state is inconsistent, restart the transfer safely instead of appending incompatible content.
7. Implement a bounded retry policy with exponential backoff and jitter for eligible transient errors. Do not retry permanent errors indefinitely.
8. Make Ctrl+C or graceful shutdown stop new work, cancel active I/O as safely as possible, preserve trustworthy metadata, and leave transfers resumable.
9. During startup, examine unfinished records and reconcile their partial files before allowing resume.
10. Do not claim that database metadata and file writes are atomic. Define and test recovery behavior for crashes between a file write and its metadata commit.

### Tests to write

- Interrupt a sequential transfer, restart the application, and verify it resumes from a safe offset when range support exists.
- Verify a server without range support triggers a safe restart from the beginning.
- Verify an ETag or Last-Modified change prevents unsafe append.
- Verify an invalid `206` or mismatched `Content-Range` is rejected.
- Test timeout, connection reset, transient 5xx errors, and retry exhaustion.
- Test graceful Ctrl+C behavior where the harness supports signal testing.
- Simulate a partial file larger or smaller than expected and verify safe reconciliation.
- Verify repeated recovery runs do not corrupt or duplicate a completed file.
- Verify permanent errors are not retried indefinitely.

### Required results / exit gate

- Supported interrupted sequential transfers resume correctly.
- Unsafe resume attempts are rejected or restarted safely.
- Retries are bounded and observable.
- Unfinished records remain recoverable across separate process invocations.
- Final file correctness is verified by comparing bytes or hashes with the original fixture.
- All resume, retry, and recovery tests pass.

---

## Phase 7 — URL Batches and Bounded Concurrent File Scheduling

### Objective

Allow a single invocation to accept multiple URLs and download independent files concurrently within strict resource limits.

### What needs to be done

- Support multiple URLs as command arguments.
- Support URL lists from a plain-text file, one URL per line.
- Add a queue and scheduler for multiple download records.
- Enforce a configurable maximum number of concurrent files.
- Keep per-download failure independent so one bad URL does not corrupt or silently cancel unrelated work.

### How to achieve it

1. Validate and normalize each input URL without inventing site-specific parsing.
2. Ignore blank lines in a URL list; define and test how comments or malformed lines are handled rather than guessing silently.
3. Create persistent queue records for accepted downloads.
4. Schedule only up to the configured file concurrency limit.
5. Use Tokio concurrency primitives or a bounded worker queue; do not spawn one unbounded task per input URL.
6. Reuse the configured HTTP client.
7. Aggregate per-download results while retaining each download's error state.
8. Preserve unfinished work in SQLite so a later invocation can resume it.
9. Keep segment-level concurrency disabled for now or at its existing safe default; segmented transfers are implemented in Phase 8.

### Tests to write

- One URL, several URLs, and a URL-list file test.
- Empty list, blank lines, malformed URL, duplicate URL, and unreadable list-file tests.
- Concurrency-limit test asserting that active file downloads never exceed the configured maximum.
- Mixed-success test with valid and failing URLs in one invocation.
- Test proving one failed download does not falsely mark unrelated downloads as failed or completed.
- Large URL-list test verifying bounded task and memory behavior.
- Restart test verifying unfinished queue entries remain available.

### Required results / exit gate

- The CLI can submit one or many direct HTTP/HTTPS URLs.
- Independent files download concurrently within the configured limit.
- Errors and results are tracked per download.
- Queue work remains bounded and persistent.
- No website-specific crawler, plugin system, or browser functionality is introduced.
- All batch and scheduling tests pass.

---

## Phase 8 — Segmented Downloads

### Objective

Add parallel byte-range downloads for eligible files while preserving correctness and using bounded resources.

### What needs to be done

- Implement segment planning from a known file size.
- Run segment requests concurrently within per-file and global limits.
- Validate every segment response.
- Write each segment to its exact byte offset through a safe positioned-write or dedicated-writer strategy.
- Fall back to sequential downloading when segmentation is unsupported or unsafe.

### How to achieve it

1. Enable segmentation only when expected size is known and range support has been adequately validated.
2. Apply the configured minimum file size and segment count/size limits.
3. Calculate non-overlapping inclusive byte ranges with careful overflow checks; ensure the ranges cover the intended object exactly once.
4. Request each range and require a valid `206 Partial Content` with an exact, matching `Content-Range`.
5. Ensure the response representation does not change byte offsets, including handling compression consistently.
6. Use bounded streaming buffers; never load all segments into memory.
7. Prevent concurrent workers from racing on a shared mutable file cursor. Use platform-correct positioned writes or a dedicated file-writer mechanism that preserves non-overlapping range semantics.
8. Keep one failed segment from being mistaken for a successful whole-file transfer.
9. Use safe fallback if range requests are ignored, malformed, or unsupported. Discard or safely reconcile any partial segmented output before starting sequential fallback.
10. Track per-segment state, but full crash-consistent segment recovery is finalized in Phase 9.

### Tests to write

- Download a deterministic file using several ranges and compare the final bytes/hash to the source.
- Test segment counts of 1, 2, 3, and the configured maximum.
- Test files smaller than the segmentation threshold.
- Test non-divisible file sizes and final short segments.
- Test invalid range arithmetic and integer overflow boundaries.
- Test wrong status codes, mismatched `Content-Range`, wrong body lengths, and ignored range requests.
- Test range requests against an unknown-size resource and verify segmentation is not attempted.
- Test bounded per-file and global active-transfer counts.
- Test a segment failure never yields a false completed download.

### Required results / exit gate

- Segmented downloads produce byte-identical results for eligible test files.
- Each worker writes only to its assigned range.
- Segment and overall concurrency remain bounded.
- Unsupported or invalid range behavior falls back safely.
- Corrupted or incomplete segmented output is never finalized.
- All segment correctness and concurrency tests pass.

---

## Phase 9 — Crash Consistency and Segmented Recovery

### Objective

Extend safe resume and recovery to segmented transfers and make filesystem/database inconsistency recoverable.

### What needs to be done

- Persist segment state and validated completion.
- Recover unfinished segmented downloads after process termination or system restart.
- Define the consistency protocol across file writes and SQLite metadata updates.
- Recover from partial, stale, or inconsistent segment data without corrupting output.

### How to achieve it

1. Persist each segment's range, state, and confirmed byte count.
2. Mark a segment complete only after its expected bytes have been written successfully; use the architecture's chosen durability policy consistently.
3. During recovery, verify the partial file, expected total size, saved segment plan, remote validators, and database state.
4. Re-fetch only segments that are safely known to be incomplete or inconsistent.
5. If the remote object changed, metadata is untrustworthy, or the partial file cannot be reconciled safely, reset the partial transfer and restart.
6. Make recovery idempotent: running startup recovery more than once must not damage valid progress.
7. Add test-only failure injection points around critical transitions when needed to reproduce crashes deterministically; do not expose test hooks as user-facing product features.
8. Handle a crash after file write but before database commit, and a crash after database update but before durable file data, according to the chosen correctness policy.
9. Keep finalization separate from segment scheduling and prevent simultaneous finalization of the same download.

### Tests to write

- Recover a partially completed segmented download and verify completed segments are not unnecessarily redownloaded when safely verifiable.
- Simulate crashes at each critical file-write/database-update boundary.
- Test database records claiming a segment is complete when the file region is missing or inconsistent.
- Test a partial file that is truncated, oversized, or has an incompatible segment layout.
- Test remote validator changes during recovery.
- Run recovery multiple times and compare final output hash with the source.
- Test concurrent attempts to recover/finalize the same record are prevented or safely serialized.
- Test completed downloads are not re-downloaded after restart.

### Required results / exit gate

- Segmented downloads resume safely after process or system interruption when conditions permit.
- Recovery never trusts metadata blindly when it contradicts file state.
- Repeated recovery is idempotent.
- No crash scenario tested can cause silent false completion or mixed-version output.
- Full recovery suite passes.

---

## Phase 10 — Safe Automatic ZIP Extraction

### Objective

Automatically extract downloaded ZIP archives only after successful download finalization, without allowing unsafe archive paths or silently overwriting files.

### What needs to be done

- Detect ZIP archives according to a documented, predictable policy.
- Extract to a predictable destination, such as a dedicated folder alongside the archive.
- Allow automatic extraction to be disabled.
- Track extraction separately from download completion.
- Preserve the original ZIP if extraction fails.
- Support retrying extraction without redownloading the archive.

### How to achieve it

1. Use a maintained Rust ZIP library.
2. Treat filename extension as a detection hint, not proof that archive content is valid.
3. Run extraction only after the download is finalized and validated.
4. Normalize and validate each entry path before writing. Reject absolute paths, parent traversal, unsafe drive/UNC prefixes, and any resolved path that escapes the extraction root.
5. Do not overwrite existing files silently. Apply a documented collision policy and report conflicts clearly.
6. Bound extraction resource use where practical, including handling suspicious entry sizes and extraction failures.
7. Persist extraction state separately, using states such as queued/extracting/completed/failed as needed.
8. Preserve the downloaded archive on extraction failure and make extraction retry independent of download retry.
9. Do not delete the archive automatically unless the source documents explicitly authorize that behavior; the initial behavior should preserve it.

### Tests to write

- Valid ZIP extraction with nested paths and byte-for-byte extracted content verification.
- Corrupt and truncated ZIP tests.
- Archive with absolute paths, `..` traversal, drive prefixes, and paths escaping the destination.
- Existing destination file/directory collision tests.
- Test that no extraction starts before download finalization.
- Test that extraction failure preserves the archive and records a clear error.
- Test that `--no-extract` or its chosen equivalent skips extraction.
- Test extraction retry does not trigger a new download.
- Test repeated extraction attempts do not overwrite existing user data silently.

### Required results / exit gate

- A successfully downloaded ZIP extracts automatically by default.
- Unsafe archive entries cannot write outside the intended destination.
- Extraction failure is reported separately from download success.
- The original archive is preserved.
- Extraction can be disabled and retried independently.
- All ZIP extraction security and correctness tests pass.

---

## Phase 11 — User-Facing CLI Workflows and Progress

### Objective

Complete the CLI interface so users can operate the implemented engine and inspect current or persisted download states.

### What needs to be done

- Wire the supported commands to the completed application services.
- Provide concise per-download and overall summaries.
- Report progress, speed, bytes, and estimated remaining time when meaningful.
- Provide status/listing, resume, and retry workflows.
- Return useful exit codes and actionable diagnostics.

### How to achieve it

1. Finalize the command syntax consistently with the project and architecture documents.
2. Support one URL, several URLs, and a URL-list file.
3. Support output directory, global concurrent file limit, per-file segment limit, and extraction enable/disable settings.
4. Add a status/listing command showing queued, active, paused/interrupted, completed, and failed records as supported by stored state.
5. Add resume behavior that selects eligible unfinished records; do not pretend to resume a record when metadata or the remote server makes it unsafe.
6. Add retry behavior for eligible failed records without resetting unrelated completed work.
7. Calculate speed from observed byte throughput using a rolling interval or equivalent stable method; do not infer it from configured bandwidth.
8. Avoid excessive redraws or event/log output. Keep progress updates controlled and ensure logs remain useful.
9. Document exit codes for complete success, partial batch failure, invalid invocation, and unrecoverable application failure.

### Tests to write

- CLI integration tests for every supported command and option.
- Tests for readable status output across lifecycle states.
- Tests verifying progress values are bounded, non-negative, and consistent with bytes transferred.
- Tests for unknown size or unknown ETA, ensuring the CLI handles unavailable values honestly.
- Tests for resume/retry selecting only eligible records.
- Tests for exit codes in success, partial-failure, invalid-input, and fatal-error cases.
- Tests proving CLI behavior delegates to application services rather than reimplementing transfer logic.

### Required results / exit gate

- All intended user workflows are available from the CLI.
- Status, progress, and errors are clear and truthful.
- Resume and retry operate from persistent state.
- No graphical interface or service process has been added.
- All CLI end-to-end tests pass.

---

## Phase 12 — Windows Resilience and Failure Hardening

### Objective

Verify Windows-specific behavior and harden the application against expected operating-system, filesystem, and shutdown failures.

### What needs to be done

- Verify filename and path handling on Windows.
- Verify access-denied, locked-file, disk-full, long-path, and duplicate-destination behavior.
- Verify graceful interruption and recovery after forced process termination.
- Verify bounded resource usage under a large queue.
- Confirm no service, auto-start item, or background daemon is installed.

### How to achieve it

1. Run the test suite on a supported Windows environment.
2. Test output directory creation and failure cases using temporary directories and controlled permissions where possible.
3. Simulate disk-write failure where a deterministic test mechanism is available.
4. Test Ctrl+C during a transfer, during finalization where safely testable, and while multiple downloads are active.
5. Force-terminate a test process during a transfer, relaunch it, and verify recovery through the actual CLI.
6. Inspect process/task cleanup to ensure cancellation does not leak unbounded tasks or leave resources open.
7. Test application-data database path handling and concurrent database access assumptions.
8. Verify file moves/finalization behavior across same-volume and cross-volume situations; never assume an atomic rename across different volumes.
9. Clearly document any failure mode that cannot be simulated reliably in automated CI.

### Tests to write

- Windows integration tests for invalid/reserved filenames and path semantics.
- Access-denied and file-lock handling tests.
- Disk-full or simulated write-failure recovery tests.
- Ctrl+C tests with one and multiple active downloads.
- Forced-termination-and-relaunch test.
- Repeated startup/shutdown test verifying no duplicate work or corrupt state.
- Large queue test confirming global concurrency limits and reasonable memory use.
- A clean-install or clean-run test confirming the application creates no unrequested services or startup entries.

### Required results / exit gate

- Windows-specific behavior is tested on Windows, not assumed from another OS.
- Filesystem failures return useful errors and do not create false completion.
- The application remains recoverable after tested interruptions.
- No unrequested service, daemon, startup task, or resident process is installed.
- Any CI/environment limitations are reported accurately.

---

## Phase 13 — Performance and Resource Validation

### Objective

Measure practical throughput and resource use, then make only evidence-based performance changes that preserve correctness.

### What needs to be done

- Benchmark sequential downloading against supported segmented downloading.
- Benchmark multiple simultaneous files under a global concurrency limit.
- Measure CPU, memory, disk activity, request count, and throughput.
- Confirm resource usage remains bounded for large files and long URL lists.
- Tune defaults only when repeatable evidence justifies the change.

### How to achieve it

1. Use local controlled servers and known test files; optionally use a stable controlled remote environment for additional network realism.
2. Compare sequential versus 2, 4, 8, and other safe segment counts only when the server supports ranges.
3. Compare one active file with several concurrent files.
4. Test small files, large files, high latency, slow servers, and servers that throttle or limit connections.
5. Record environment details, server behavior, file size, concurrency settings, duration, throughput, CPU, RAM, and result hash.
6. Repeat measurements enough times to avoid basing a decision on a single run.
7. Keep correctness and recovery tests enabled while changing performance-sensitive code.
8. Do not claim a universal speed multiplier; report results only for the measured environment.
9. Do not adopt custom Win32 I/O, unsafe code, lock-free designs, or new dependencies unless a profile proves the current approach is a meaningful bottleneck and the change is justified.

### Tests to write/run

- Repeatable sequential baseline benchmark.
- Segment-count comparison benchmark.
- Multiple-file concurrency benchmark.
- Large file/resource-bound test.
- Long URL-list stress test.
- Post-benchmark hash/size verification for every downloaded fixture.
- Regression suite covering resume, segmentation, recovery, and ZIP extraction after any optimization.

### Required results / exit gate

- Benchmarks can be repeated with documented commands and settings.
- Throughput, CPU, RAM, and file correctness are recorded.
- Default concurrency settings are justified by evidence or explicitly retained as conservative provisional defaults.
- No performance change weakens file integrity or recovery.
- No unsupported performance claims are added to documentation.

---

## Phase 14 — Documentation and Release Readiness

### Objective

Deliver a focused CLI application that another developer can build, test, operate, and troubleshoot using accurate documentation.

### What needs to be done

- Complete user-facing README and command reference.
- Document configuration, data location, retry behavior, resume limitations, ZIP extraction, and known limitations.
- Document build and test commands for Windows.
- Verify clean build, installation/run instructions, and release artifact behavior.
- Review project scope for unrequested features.

### How to achieve it

1. Document only commands and flags that exist and have passing tests.
2. Explain that resume and segmentation depend on server behavior, metadata, and HTTP range support.
3. Explain ZIP extraction destination and collision policy.
4. Explain where queue metadata is stored and how interrupted downloads are resumed.
5. Include troubleshooting guidance based on actual error categories.
6. Run all required formatting, linting, unit, integration, recovery, security, and Windows tests.
7. Build a release-mode executable on Windows and test it from a clean environment.
8. Review dependencies and configuration for unused features, unnecessary services, telemetry, or bundled components.
9. Confirm project documents are internally consistent; change the approved source documents only with explicit project-owner approval.

### Tests and checks to write/run

- Full test suite on Windows.
- `cargo fmt --check`.
- `cargo clippy --all-targets --all-features -- -D warnings`, adjusted only for documented, necessary toolchain constraints.
- Release build.
- Smoke test of the release executable using a local HTTP fixture.
- CLI help/command documentation consistency check.
- Dependency and packaging review.
- Final scope review against the project document and `architecture.md`.

### Required results / exit gate

- A developer can build and test the project from documented instructions.
- All implemented CLI commands and important behavior are documented accurately.
- Windows release artifact works in a clean test environment.
- Tests for essential download, resume, recovery, and extraction behavior pass.
- No GUI, browser integration, plugin ecosystem, service, telemetry, or unrelated feature has slipped into the release.
- Remaining limitations are explicit and evidence-based.

---

## 4. Cross-Phase Definition of Done

Every phase must satisfy all applicable items below before it can be marked complete:

- **Scope:** The changes implement only the assigned phase and approved product requirements.
- **Correctness:** New behavior has tests for success and relevant failure conditions.
- **Regression:** Existing tests still pass.
- **Error handling:** Errors are surfaced and state remains truthful.
- **Resource bounds:** Network tasks, retries, buffers, queues, and concurrency remain bounded where applicable.
- **Windows:** Windows-specific claims are validated on Windows, or clearly marked unverified.
- **Documentation:** User-facing behavior and configuration are documented when introduced.
- **Handoff:** The implementation report lists files changed, tests actually run, results, limitations, and acceptance status.

## 5. Final Product Acceptance Checklist

The project is considered complete only when all applicable items below have passing evidence:

- [ ] Accept one direct HTTP/HTTPS URL.
- [ ] Accept multiple URLs and a URL-list text file.
- [ ] Download multiple files concurrently within configured limits.
- [ ] Use segmented downloading only when safe and useful.
- [ ] Fall back safely when range support is absent or invalid.
- [ ] Show honest progress, throughput, and ETA when calculable.
- [ ] Resume supported interrupted downloads after a subsequent run.
- [ ] Retry transient failures with bounded backoff.
- [ ] Recover state after graceful interruption, forced termination, and tested restart scenarios.
- [ ] Detect or safely handle changed remote files during resume.
- [ ] Never finalize an incomplete, mismatched, or corrupted file.
- [ ] Avoid silent overwrites and unsafe Windows filenames/paths.
- [ ] Automatically extract valid ZIP archives by default.
- [ ] Allow ZIP extraction to be disabled.
- [ ] Prevent ZIP path traversal and unsafe extraction writes.
- [ ] Preserve the archive if extraction fails and allow extraction retry independently.
- [ ] Bound concurrency, memory, retries, and background tasks.
- [ ] Pass the full test suite on Windows.
- [ ] Provide accurate build, usage, configuration, and troubleshooting documentation.
- [ ] Remain CLI-only without unrelated features or persistent background services.

## 6. Final Product Boundary

The approved workflow is:

**Accept URLs → queue downloads → download concurrently where safe → recover interrupted transfers → validate and finalize files → extract ZIP archives → report results.**

Anything outside that workflow is out of scope unless the project owner explicitly updates the approved requirements.
