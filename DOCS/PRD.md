# Windows-First CLI Download Manager (SwiftFetch)

## Core Idea

Build a lightweight, Windows-first command-line download manager inspired by the core workflow of JDownloader 2: accept one URL or a list of URLs, download the files efficiently and reliably, resume interrupted downloads, and automatically extract completed ZIP archives.

The first version is **CLI-only**. A graphical interface is explicitly out of scope until the download engine is reliable and proven through tests. Keep the project focused on essential download functionality, with no plugin ecosystem, account manager, browser integration, themes, telemetry, or other nonessential features.

Reference: [JDownloader 2](https://jdownloader.org/jdownloader2) and its [official archive-extraction documentation](https://support.jdownloader.org/en/knowledgebase/category/extraction).

## Primary Goals

- Deliver fast, parallel downloads while respecting server limits and available network bandwidth.
- Support downloading one URL or many URLs in a single command.
- Resume partially completed downloads after manual interruption, network failure, process termination, or system restart when the server supports it.
- Recover download state safely without corrupting or silently mislabeling partial files.
- Automatically extract a successfully downloaded ZIP archive when extraction is enabled.
- Optimize for Windows first, including reliable file handling and clear CLI behavior.
- Keep the project small, testable, and focused on the download engine.

## Essential Features

### 1. CLI Input and Download Queue

- Accept a single direct HTTP or HTTPS URL.
- Accept multiple URLs in one invocation.
- Read URL lists from a plain-text file, with one URL per line.
- Allow the user to specify an output directory.
- Show clear per-download status and a concise overall summary.
- Support a configurable limit for simultaneous file downloads.
- Preserve a queue across restarts so unfinished downloads can be resumed.

### 2. Fast, Parallel Downloading

- Download multiple independent files concurrently.
- Support segmented downloads for a single file when the server reliably supports HTTP byte-range requests.
- Use bounded, configurable concurrency rather than creating unlimited connections or tasks.
- Detect when range requests are unsupported and safely fall back to a sequential download.
- Reuse HTTP clients and connections where appropriate.
- Report progress, downloaded bytes, transfer speed, and estimated remaining time when the required information is available.
- Prefer stable, measured throughput over aggressive connection counts that may reduce performance.

### 3. Resume and Recovery

- Provide commands to inspect unfinished downloads and resume them.
- Persist enough metadata to recover after the CLI process exits or Windows restarts.
- Retry transient network failures using bounded backoff.
- Handle timeouts, dropped connections, server errors, and interrupted writes without losing track of verified progress.
- Validate range responses and ensure segments write only to their assigned byte ranges.
- Use ETag or Last-Modified validators when available to detect a changed remote file before resuming.
- If safe resumption is impossible, restart that download rather than risk combining bytes from different file versions.
- Keep incomplete files distinguishable from completed files.
- Never report a download as complete until all required bytes have been written and finalization succeeds.
- Make interruption and recovery behavior idempotent, so repeated restarts do not corrupt the file or queue state.

### 4. Automatic ZIP Extraction

- After a download completes successfully, detect ZIP archives and extract them automatically.
- Provide an option to disable automatic extraction.
- Extract into a predictable destination, such as a folder beside the archive.
- Do not extract an archive that is incomplete or has failed validation.
- Report extraction failures clearly and preserve the original archive.
- Prevent archive path traversal and unsafe file writes.
- Handle existing destination files without silently overwriting user data.
- Treat extraction as a separate state from downloading, so a completed download can be retried for extraction without downloading the archive again.

### 5. Windows-First File Handling

- Use user-selected output directories and safe filenames.
- Handle duplicate names without accidental overwrites.
- Handle access-denied, disk-full, path, and file-lock errors explicitly.
- Write to partial or temporary files and finalize only after successful completion.
- Keep temporary data and persistent queue metadata in a predictable application data location.
- Exit cleanly on user cancellation and recover safely after an unexpected shutdown.

## Minimal Command-Line Workflow

The exact command syntax can be decided during implementation, but the CLI must support these workflows:

- Download one URL.
- Download several URLs in one invocation.
- Download URLs from a text file.
- Set the output directory.
- Configure maximum concurrent file downloads and per-file segments.
- Inspect queued, active, completed, and failed downloads.
- Resume unfinished downloads.
- Retry failed downloads.
- Enable or disable automatic ZIP extraction.

## Core Technical Direction

- **Target platform:** Windows first.
- **Language:** Rust.
- **Asynchronous runtime and networking:** Tokio and Reqwest.
- **Persistent queue and recovery metadata:** SQLite.
- **Archive support:** ZIP extraction through a maintained Rust library.
- **Interface:** CLI only for the initial release; no GUI framework or frontend.

Keep the engine modular internally, but do not build abstractions for hypothetical future features. The download engine owns scheduling, HTTP behavior, retries, segment coordination, file writing, state persistence, and recovery. The CLI is a thin layer for commands, progress output, and exit codes.

## Reliability and Performance Acceptance Criteria

- A single URL and a list of URLs can be downloaded from the CLI.
- Multiple files can download concurrently within configured limits.
- Segmented downloading is used only when server behavior makes it safe and useful.
- A supported interrupted download resumes without restarting from zero.
- A server that does not support resumption is handled safely.
- Process termination and Windows restart do not cause completed files to be mistaken for partial files or vice versa.
- Network failures, timeouts, disk-full conditions, and access errors produce clear status and recoverable state where possible.
- ZIP extraction starts only after a successful download, does not write outside the intended destination, and preserves the archive if extraction fails.
- Repeated recovery attempts do not corrupt files or duplicate completed work.
- Performance is measured against a sequential baseline using repeatable tests, including throughput, resource use, and recovery correctness.

## Explicitly Out of Scope for the First Version

- Graphical user interface.
- Browser extensions or browser download interception.
- Website-specific plugins, link crawling, and streaming-video extraction.
- Premium-hosting accounts, CAPTCHA handling, and account management.
- Remote control, cloud synchronization, and mobile applications.
- Themes, telemetry, advertising, bundled offers, and unrelated background services.
- Advanced archive formats beyond the initially supported ZIP workflow unless explicitly added later.

## Product Principle

Build the smallest reliable tool that performs the essential workflow well: **accept URLs → download in parallel where safe → recover interrupted transfers → verify completion → extract ZIP archives → report the final result**. Reliability and file integrity take priority over feature count and peak speed claims.
