# SwiftFetch

SwiftFetch is a lightweight, Windows-first command-line download manager in
Rust. It accepts
one URL or a list of URLs, downloads files efficiently and reliably, resumes
interrupted downloads, and automatically extracts completed ZIP archives.

This project is developed in the controlled phases defined in
[`DOCS/phases.md`](DOCS/phases.md). The current build implements **Phase 11 —
User-Facing CLI Workflows and Progress**: everything through Phase 10
(persistence, transfer engine, resume/retry, batches, segmented downloads,
ZIP extraction) is complete, and all commands are wired end to end.

- Product requirements: [`DOCS/PRD.md`](DOCS/PRD.md)
- Architecture: [`DOCS/architecture.md`](DOCS/architecture.md)
- Running changelog (all code/feature changes): [`changes.md`](changes.md)
- Architecture decision record: [`decision.md`](decision.md)

## CLI

Try `swift-fetch --help` (or `swift-fetch download --help`). Every command is
wired to the transfer engine and the persisted queue:

- `download <URL...> | --url-file <PATH>` — queue and download URLs with an
  output directory (`-o`), a concurrent-file limit (`-j`), a per-file segment
  limit (`-s`), retry attempts (`-r`), and automatic ZIP extraction
  (`--no-extract` to disable); live progress with observed speed and honest
  unknown-size/unknown-ETA wording is rendered while bytes move;
- `status` — a read-only listing of the persisted queue (record state,
  confirmed bytes, and reasons);
- `resume` — collects unfinished records from persistent state and continues
  them;
- `retry` — requeues failed records and runs them again (completed and
  unrelated work is never touched).

Downloads land in the output directory (default: the current one); the
metadata database lives in `%LOCALAPPDATA%\SwiftFetch\swiftfetch.sqlite3` —
never in the download directory — and the `SWIFT_FETCH_DATABASE` environment
variable points it somewhere deliberate (tests, second profiles).

### Exit codes

| Code | What it means |
| ---- | ------------- |
| `0` | every requested download of the invocation completed |
| `1` | at least one did not (failed or interrupted), or the command could not run (unrecoverable application failure) |
| `2` | invalid invocation or configuration |

Logging (enable with `-v`) goes to **stderr**; stdout stays reserved for
results and controlled progress lines, and every logged or displayed URL is
redacted (query string and fragment dropped, passwords masked).

## Target platform

Windows is the first target platform. Development currently runs on Windows;
Windows-specific behavior is verified on Windows.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable) with `cargo`
- The `rustfmt` and `clippy` toolchain components

## Build and test commands

From the repository root, in this order:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Build the executable:

```sh
cargo build
```

Running the development check sequence, building, and testing from a clean
checkout is verified during Phase 0; use `git clone` plus the commands above to
reproduce it.
