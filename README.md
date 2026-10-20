# SwiftFetch

Windows-first command-line download manager in Rust. SwiftFetch accepts one URL or a list of URLs, downloads files efficiently and reliably, resumes interrupted downloads, and automatically extracts completed ZIP archives.

Built with correctness and file integrity as the top priorities — never silently overwriting user data, never mislabeling partial files as complete, and never combining bytes from different remote file versions.

[![Rust](https://img.shields.io/badge/rust-1.99+-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-windows-blue.svg)](https://www.microsoft.com/windows)

---

## Table of Contents

- [Features](#features)
- [Quick Start](#quick-start)
- [Installation](#installation)
- [Usage](#usage)
- [Configuration](#configuration)
- [Resume &amp; Recovery](#resume--recovery)
- [ZIP Extraction](#zip-extraction)
- [Exit Codes](#exit-codes)
- [Logging](#logging)
- [Data Location](#data-location)
- [Security](#security)
- [Performance](#performance)
- [Testing](#testing)
- [Windows Support](#windows-support)
- [Scope &amp; Limitations](#scope--limitations)
- [Contributing](#contributing)
- [License](#license)

---

## Features

| Feature                                   | Description                                                                                                            |
| ----------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| **Single & multiple URL downloads** | Download one URL, several URLs in one invocation, or URLs from a plain-text list file                                  |
| **Bounded concurrency**             | Configurable maximum concurrent file downloads (default: 3) — never one task per URL                                  |
| **Segmented downloads**             | Parallel byte-range downloads for large files (default: 32 MiB minimum, 4 segments max)                                |
| **Resume & retry**                  | Resume interrupted downloads with validator revalidation; bounded exponential backoff with jitter (default: 5 retries) |
| **Automatic ZIP extraction**        | Extracts completed ZIP archives into a dedicated sibling folder (default: enabled)                                     |
| **Safe finalization**               | Writes to engine-owned partial files, finalizes via same-volume rename, never overwrites existing files                |
| **Persistent queue**                | SQLite-backed queue in `%LOCALAPPDATA%\SwiftFetch\swiftfetch.sqlite3` — survives process exit and restart           |
| **Windows-first**                   | Validated Windows filename handling (reserved stems, illegal characters, 255 UTF-16 unit cap)                          |
| **CLI-only**                        | No GUI, no browser integration, no background services, no telemetry                                                   |

---

## Quick Start

```bash
# Clone the repository
git clone https://github.com/NYN-05/SwiftFetch.git
cd SwiftFetch

# Build in release mode
https://fuckingfast.co/8cw9qshukcmz#CoD_MW4_--_fitgirl-repacks.site_--_.part30.rar

# Download a single file
.\target\release\swift-fetch.exe download https://example.com/file.zip

# Download multiple URLs concurrently
.\target\release\swift-fetch.exe download https://example.com/a.zip https://example.com/b.zip -j 3

# Download from a URL list file
.\target\release\swift-fetch.exe download --url-file urls.txt

# Check download status
.\target\release\swift-fetch.exe status

# Resume interrupted downloads
.\target\release\swift-fetch.exe resume

# Retry failed downloads
.\target\release\swift-fetch.exe retry

# Disable automatic ZIP extraction
.\target\release\swift-fetch.exe download --no-extract https://example.com/archive.zip
```

---

## Installation

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable) with `cargo`
- The `rustfmt` and `clippy` toolchain components

### Build from Source

```bash
# Development build
cargo build

# Release build (recommended for production use)
cargo build --release
```

The release artifact is `target/release/swift-fetch.exe` on Windows.

---

## Usage

### `download`

Download one URL, several URLs, or URLs from a list file.

```
swift-fetch download <URL...> [options]
```

| Option                | Short  | Default | Description                                                                                                   |
| --------------------- | ------ | ------- | ------------------------------------------------------------------------------------------------------------- |
| `--url-file <PATH>` | `-f` | —      | Plain-text file with one URL per line (blank lines and `#`-comments ignored; UTF-8 BOM on line 1 tolerated) |
| `--output <DIR>`    | `-o` | `.`   | Output directory for downloaded files                                                                         |
| `--files <N>`       | `-j` | `3`   | Maximum concurrent file downloads                                                                             |
| `--segments <N>`    | `-s` | `4`   | Maximum byte-range segments per file                                                                          |
| `--retries <N>`     | `-r` | `5`   | Retry attempts for transient failures (`0` = no retries)                                                    |
| `--no-extract`      |        | enabled | Disable automatic ZIP extraction                                                                              |

At least one URL or a `--url-file` must be provided. Inline URLs must start with `http://` or `https://`.

**Examples:**

```powershell
# Single download
swift-fetch.exe download https://example.com/file.zip

# Multiple URLs with custom output directory
swift-fetch.exe download https://example.com/a.zip https://example.com/b.zip -o C:\Downloads -j 5

# Download from list file with custom concurrency
swift-fetch.exe download --url-file urls.txt -j 4 --segments 2

# Download without automatic ZIP extraction
swift-fetch.exe download https://example.com/archive.zip --no-extract
```

### `status`

A read-only listing of the persisted queue showing record state, confirmed bytes, size, and failure reason for every download on record.

```
swift-fetch.exe status
```

### `resume`

Collects unfinished downloads from persistent state and continues them. Only recoverable states are resumed (queued, probing, downloading, paused, retrying, finalizing). Completed records are not resumed; cancelled records are terminal.

```
swift-fetch.exe resume
```

### `retry`

Requeues only `failed` records and runs them again. Completed, queued, and unrelated work is never touched. A cancelled record cannot be retried.

```
swift-fetch.exe retry
```

---

## Configuration

Defaults follow `DOCS/architecture.md` §6 and are provisional starting values tuned only with benchmark evidence:

| Setting                            | Default             | Description                               |
| ---------------------------------- | ------------------- | ----------------------------------------- |
| Concurrent file downloads          | `3`               | Maximum files downloading simultaneously  |
| Segments per file                  | `4`               | Maximum byte-range segments per file      |
| Minimum file size for segmentation | `32 MiB`          | Files below this size stream sequentially |
| Nominal segment size               | `~8 MiB`          | Target size per segment                   |
| Retry attempts                     | `5`               | Bounded retries for transient failures    |
| Automatic ZIP extraction           | `enabled`         | Extract completed ZIP archives by default |
| Existing-file behavior             | `never overwrite` | Collision policy appends `(n)` suffixes |

Connection timeout is **15 s**; each body read is bounded by a **30 s** idle budget (total transfer duration is unbounded — large downloads are legitimate).

Retries use bounded exponential backoff with full jitter (500 ms base, doubling per attempt, capped at 8 s). Only transient failures (HTTP 5xx, timeouts, transport drops) are retried; permanent failures (4xx, validation, filesystem, database) fail the record at once.

---

## Resume & Recovery

### Resume

An interrupted transfer resumes from the trusted offset — `min(database-confirmed progress, on-disk partial length)` — only when:

- The server re-proves byte-range support, **and**
- The stored ETag/Last-Modified validators still match.

If the remote object changed, range support is absent, or state is inconsistent, the transfer restarts safely from zero rather than appending incompatible bytes.

Resume depends on server behavior, metadata, and HTTP range support. A server that does not support resumption is handled safely by restarting from zero.

### Recovery

SwiftFetch persists queue and recovery metadata in SQLite, separate from download directories. On startup, unfinished records are reconciled against partial files on disk before any network work:

- Crashes between file write and database commit are handled safely in both directions
- Recovery is idempotent — repeated restarts do not corrupt data or duplicate completed work
- The database is never used to store downloaded file contents (no BLOB columns)

---

## ZIP Extraction

After a download finalizes, SwiftFetch detects ZIP archives by **magic bytes** (extension is only a hint) and extracts them automatically into a dedicated folder beside the archive:

```
payload.zip  →  payload\
```

### Safety guarantees

- **Path traversal prevention:** Entry paths are sealed before any write — absolute paths, `..` traversal, drive/UNC prefixes, and Windows reserved device names (CON, PRN, AUX, NUL, COM1–9, LPT1–9) are all rejected
- **No silent overwrites:** Existing files are skipped and reported by default
- **Archive preservation:** The original ZIP is preserved byte-for-byte if extraction fails
- **Independent retry:** Extraction can be retried without redownloading the archive
- **Separate tracking:** Extraction state is tracked independently from download state — a failed extraction never flips a completed download to failed

Use `--no-extract` to disable automatic extraction.

---

## Exit Codes

| Code  | Meaning                                                                    |
| ----- | -------------------------------------------------------------------------- |
| `0` | Every requested download of the invocation completed                       |
| `1` | At least one did not (failed or interrupted), or the command could not run |
| `2` | Invalid invocation or configuration                                        |

Logging (enable with `-v`) goes to **stderr**; stdout stays reserved for results and controlled progress lines. Every logged or displayed URL is redacted (query string and fragment dropped, passwords masked).

---

## Data Location

Downloads land in the output directory (default: the current directory). The metadata database lives in the user's local application-data directory, separate from any download directory:

```
%LOCALAPPDATA%\SwiftFetch\swiftfetch.sqlite3
```

It stores queue and recovery metadata only — never downloaded file contents. The `SWIFT_FETCH_DATABASE` environment variable points it somewhere deliberate (hermetic tests, second profiles); an empty value falls through to the platform default, which is never guessed at.

---

## Security

- **URL redaction:** Query strings and fragments are stripped from logs; passwords in userinfo are masked (`user:[REDACTED]@host`)
- **No content transformation:** Requests carry `Accept-Encoding: identity`; no compression features enabled, preventing byte-range offset invalidation
- **ZIP path sealing:** Entry names are validated lexically and through filesystem resolution — nothing can write outside the extraction destination
- **No silent overwrites:** Existing user files are never replaced; collision policy appends `(n)` suffixes

---

## Performance

SwiftFetch measures performance against a sequential baseline using repeatable benchmarks:

- **Sequential baseline:** Forced-sequential downloads measured against segmented downloads
- **Segment-count comparison:** 1, 2, 4 (default), and 8 segments compared
- **Multiple-file concurrency:** File concurrency limits (1, 3, 8) measured
- **Large file resource bound:** Working-set memory stays below one full file copy
- **Long URL-list stress:** 200 URLs from a list file measured for bounded memory and concurrency

Results describe the measured environment only — no universal speed claims are made. Default concurrency settings are retained as conservative provisional defaults.

Run benchmarks with:

```bash
cargo test --bin swift-fetch -- --ignored --nocapture
```

---

## Testing

From the repository root, run the canonical verification sequence in this order:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Build the executable:

```bash
cargo build
```

Build a release-mode executable:

```bash
cargo build --release
```

The release artifact is `target/release/swift-fetch.exe`. Smoke-test it against a local HTTP fixture:

```powershell
.\target\release\swift-fetch.exe download <URL> --no-extract
.\target\release\swift-fetch.exe status
```

Running the development check sequence, building, and testing from a clean checkout is verified during Phase 0; use `git clone` plus the commands above to reproduce it.

---

## Windows Support

Windows is the first target platform. Development and verification run on Windows; Windows-specific behavior is validated on Windows, not assumed from another OS.

Validated Windows behaviors include:

- Reserved device stems (CON, PRN, AUX, NUL, COM1–9, LPT1–9) sanitized
- Illegal filename characters replaced
- 255 UTF-16 unit per-component path cap with extension preservation
- Long path handling (honest branch based on environment support)
- Access-denied via deny ACE (read-only attribute ignored on directories)
- Locked files and databases detected and handled typed
- Cross-process concurrent database access via WAL + busy timeout
- Forced termination and recovery through the real CLI
- Clean install: no Windows service, resident daemon, or auto-start entry

**Limitations not deterministically simulable in CI:** true disk-full conditions, cross-volume finalization, sub-millisecond kill-inside-rename windows, graceful Ctrl+C console event delivery.

---

## Scope & Limitations

SwiftFetch is CLI-only. The following are explicitly out of scope:

- Graphical user interface
- Browser extensions or browser download interception
- Website-specific plugins, link crawling, streaming-video extraction
- Premium-hosting accounts, CAPTCHA handling, account management
- Remote control, cloud synchronization, mobile applications
- Themes, telemetry, advertising, bundled offers, background services
- Archive formats beyond ZIP

Nothing in this release installs a Windows service, resident daemon, or auto-start entry.

**Known limitations:**

- Resume and segmentation depend on server behavior (range support, validators); a server that does not support resumption is handled safely by restarting
- Graceful Ctrl+C delivery cannot be simulated in this test harness (the accepted console event never dispatches to any registered handler under this runner's console host chain)
- True disk-full conditions are not deterministically simulable in CI; discoverable write-failure paths (access-denied, occupied outputs, locked partials/databases) are tested

---

## Contributing

SwiftFetch is developed in controlled phases defined in [`DOCS/phases.md`](DOCS/phases.md). Each phase has explicit acceptance criteria and must be verified before the next begins.

- Product requirements: [`DOCS/PRD.md`](DOCS/PRD.md)
- Architecture: [`DOCS/architecture.md`](DOCS/architecture.md)
- Running changelog: [`changes.md`](changes.md)
- Architecture decision record: [`decision.md`](decision.md)

Contributions should follow the established phase workflow, maintain `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings` compliance, and include tests for new behavior in the same phase.

---

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE) for details.

---

*Built with correctness and file integrity as the top priorities — reliability takes priority over feature count and peak-speed claims.*
