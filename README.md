# SwiftFetch

SwiftFetch is a lightweight, Windows-first command-line download manager in
Rust. It accepts one URL or a list of URLs, downloads files efficiently and
reliably, resumes interrupted downloads, and automatically extracts completed
ZIP archives.

This project is developed in the controlled phases defined in
[`DOCS/phases.md`](DOCS/phases.md). The current build implements **Phase 13 —
Performance and Resource Validation (v0.15.0)**: everything through Phase 10
(persistence, transfer engine, resume/retry, batches, segmented downloads,
ZIP extraction) is complete, all commands are wired end to end, and Phase 13
adds repeatable throughput/resource benchmarks against a sequential baseline.

- Product requirements: [`DOCS/PRD.md`](DOCS/PRD.md)
- Architecture: [`DOCS/architecture.md`](DOCS/architecture.md)
- Running changelog (all code/feature changes): [`changes.md`](changes.md)
- Architecture decision record: [`decision.md`](decision.md)

## CLI

Try `swift-fetch --help` (or `swift-fetch download --help`). Every command is
wired to the transfer engine and the persisted queue.

### `download <URL...> [options]`

Queue and download one URL, several URLs, or URLs from a list file.

| Option | Short | Meaning |
|---|---|---|
| `--url-file <PATH>` | `-f` | Plain-text file with one URL per line (blank lines and `#`-comments are ignored; a UTF-8 BOM on line 1 is tolerated) |
| `--output <DIR>` | `-o` | Output directory for downloaded files [default: current directory] |
| `--files <N>` | `-j` | Maximum concurrent file downloads [default: 3] |
| `--segments <N>` | `-s` | Maximum byte-range segments per file [default: 4] |
| `--retries <N>` | `-r` | Retry attempts for transient failures [default: 5; `0` means one attempt, no retries] |
| `--no-extract` | | Disable automatic ZIP extraction (extraction is enabled by default; the downloaded archive is always preserved) |

At least one URL or a `--url-file` must be provided; inline URLs must start
with `http://` or `https://`.

### `status`

A read-only listing of the persisted queue: record state, confirmed bytes,
size, and failure reason for every download on record.

### `resume`

Collects unfinished downloads from persistent state and continues them
(queued, probing, downloading, paused, retrying, finalizing). Completed and
cancelled records are not resumed; a completed record whose destination file
is missing is reported as a database error and never re-runs.

### `retry`

Requeues only `failed` records and runs them again. Completed, queued, and
unrelated work is never touched. A cancelled record is terminal and cannot be
retried. An empty selection is a successful no-op.

## Configuration

Defaults follow `DOCS/architecture.md` §6 and are provisional starting values
tuned only with benchmark evidence:

| Setting | Default |
|---|---|
| Concurrent file downloads | 3 |
| Segments per file | 4 |
| Minimum file size for segmentation | 32 MiB |
| Nominal segment size | ~8 MiB |
| Retry attempts | 5 |
| Automatic ZIP extraction | enabled |
| Existing-file behavior | never overwrite silently |

Connection timeout is 15 s; each body read is bounded by a 30 s idle budget
(the total transfer duration is unbounded — large downloads are legitimate).

Retries use bounded exponential backoff with full jitter (500 ms base,
doubling per attempt, capped at 8 s). Only transient failures (HTTP 5xx,
timeouts, transport drops) are retried; permanent failures (4xx, validation,
filesystem, database) fail the record at once.

## Data location

Downloads land in the output directory (default: the current one). The
metadata database lives in the user's local application-data directory,
separate from any download directory:

```
%LOCALAPPDATA%\SwiftFetch\swiftfetch.sqlite3
```

It stores queue and recovery metadata only — never downloaded file contents
(no BLOB columns). The `SWIFT_FETCH_DATABASE` environment variable points it
somewhere deliberate (hermetic tests, second profiles); an empty value falls
through to the platform default, which is never guessed at.

## Resume and ZIP extraction

- **Resume:** an interrupted transfer resumes from the trusted offset
  (`min(database-confirmed progress, on-disk partial length)`) only when the
  server re-proves byte-range support and the stored ETag/Last-Modified
  validators still match. If the remote object changed, range support is
  absent, or state is inconsistent, the transfer restarts safely from zero
  rather than appending incompatible bytes. Resume depends on server
  behavior, metadata, and HTTP range support.
- **ZIP extraction:** after a download finalizes, SwiftFetch detects ZIP
  archives by magic bytes (extension is only a hint) and extracts them
  automatically into a dedicated folder beside the archive
  (`payload.zip` → `payload\`). Entry paths are sealed before any write —
  absolute paths, `..` traversal, drive/UNC prefixes, and Windows reserved
  device names are rejected — so nothing can write outside the destination.
  Existing files are never silently overwritten (skip-and-report by default).
  A failing extraction preserves the archive intact and is tracked
  separately from the download, so extraction can be retried without
  redownloading. Use `--no-extract` to disable.

## Exit codes

| Code | What it means |
|---|---|
| `0` | every requested download of the invocation completed |
| `1` | at least one did not (failed or interrupted), or the command could not run (unrecoverable application failure) |
| `2` | invalid invocation or configuration |

Logging (enable with `-v`) goes to **stderr**; stdout stays reserved for
results and controlled progress lines, and every logged or displayed URL is
redacted (query string and fragment dropped, passwords masked).

## Target platform

Windows is the first target platform. Development and verification run on
Windows; Windows-specific behavior is validated on Windows, not assumed from
another OS.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable) with `cargo`
- The `rustfmt` and `clippy` toolchain components

## Build and test commands

From the repository root, run the canonical verification sequence in this
order:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Build the executable:

```sh
cargo build
```

Build a release-mode executable:

```sh
cargo build --release
```

The release artifact is `target/release/swift-fetch.exe`. Smoke-test it
against a local HTTP fixture:

```sh
swift-fetch.exe download <URL> --no-extract
swift-fetch.exe status
```

Running the development check sequence, building, and testing from a clean
checkout is verified during Phase 0; use `git clone` plus the commands above
to reproduce it.

## Known limitations

- Resume and segmentation depend on server behavior (range support,
  validators); a server that does not support resumption is handled safely by
  restarting.
- Graceful Ctrl+C delivery cannot be simulated in this test harness (the
  accepted console event never dispatches to any registered handler under
  this runner's console host chain); the engine's graceful-shutdown contract
  remains unit-locked, and interrupt-recoverability is covered end to end by
  the forced-termination suite.
- True disk-full conditions are not deterministically simulable in CI; the
  discoverable write-failure paths (access-denied, occupied outputs, locked
  partials/databases) are tested.
- Seven pre-existing Phase 9-lineage segmented-recovery tests remain unfixed
  (they fail identically on the v0.14.0 baseline and were deliberately not
  touched by Phase 13/14 per the phase-boundary law); every other test passes.

## Scope

SwiftFetch is CLI-only. The following are explicitly out of scope: GUI,
browser extensions/interception, website-specific plugins, link crawling,
premium-host accounts, CAPTCHA handling, account management, cloud
synchronization, remote control, mobile apps, themes, telemetry, advertising,
bundled offers, background services, and archive formats beyond ZIP. Nothing
in this release installs a Windows service, resident daemon, or auto-start
entry.
