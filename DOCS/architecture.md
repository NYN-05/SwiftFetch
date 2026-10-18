# Architecture — Windows-First CLI Download Manager (SwiftFetch)

## 1. Purpose and Architectural Principles

Build a lightweight, Windows-first command-line download manager inspired by the essential workflow of JDownloader 2. It accepts one URL or a list of URLs, downloads files efficiently and reliably, resumes interrupted transfers, and automatically extracts completed ZIP archives.

The initial product is CLI-only. The download engine is the primary component; a graphical interface is out of scope unless introduced later.

Architectural principles:

- **Correctness before peak speed:** A download is successful only when the resulting file is complete and valid.
- **Bounded concurrency:** Parallelism must respect server limits and available network, CPU, memory, and disk resources.
- **Persistent recovery state:** The application must retain enough trustworthy metadata to recover unfinished work after process termination or Windows restart.
- **Small, explicit modules:** Keep one Rust application with clear internal boundaries; avoid microservices and abstractions for hypothetical features.
- **Safe failure behavior:** Never silently overwrite user data, mislabel partial files as complete, or combine bytes from different remote file versions.

## 2. Scope

### Included

- Direct HTTP and HTTPS downloads.
- One URL, multiple URLs in a single invocation, and URL lists loaded from a text file.
- Concurrent downloads of multiple files.
- Segmented downloads of an individual file when byte-range requests are supported and useful.
- Pause/cancel behavior, bounded retries, persistent queue state, and resume after interruption.
- Safe finalization of downloaded files.
- Automatic extraction of completed ZIP archives, with an option to disable extraction.
- CLI commands for starting downloads, inspecting status, resuming unfinished work, and retrying failed work.
- Windows-first file handling, diagnostics, and process shutdown behavior.

### Excluded

- Graphical user interface.
- Browser extensions or browser interception.
- Website-specific plugins, link crawling, video extraction, premium-hosting accounts, CAPTCHA handling, and account management.
- Cloud synchronization, remote control, mobile applications, themes, telemetry, advertising, bundled offers, and unrelated background services.
- A Windows service or resident daemon.
- Archive formats beyond ZIP unless explicitly required later.

## 3. Technology Stack

| Component | Technology | Rationale |
|---|---|---|
| Language | Rust | Native performance, memory safety, and strong concurrency guarantees. |
| Async runtime | Tokio | Efficient scheduling for concurrent network operations, cancellation, and timeouts. |
| HTTP client | Reqwest | Maintained HTTP/HTTPS client, TLS support, redirects, and connection pooling. |
| HTTP implementation | Hyper through the HTTP ecosystem | Reuse established protocol implementations rather than implementing HTTP from scratch. |
| CLI | Clap | Typed arguments, subcommands, validation, and generated help. |
| Persistent metadata | SQLite | Local transactional storage without a separate database server. |
| Database integration | A maintained Rust SQLite crate | Safe, explicit persistence operations and schema management. |
| ZIP extraction | A maintained Rust ZIP library | Archive inspection and extraction without shelling out to an external archiver. |
| Logging | tracing and tracing-subscriber | Structured diagnostics for downloads, segments, retries, and recovery. |
| Error types | thiserror | Typed errors with clear module boundaries. |
| Serialization | serde where needed | Configuration and structured metadata serialization. |
| Testing | Rust unit/integration tests, controlled local HTTP test server, and property-based tests where useful | Verify protocol handling, failure recovery, integrity, and performance. |

Keep dependencies limited to features that the implementation actually uses. Reuse a configured `reqwest::Client` rather than creating a new client for every segment.

## 4. High-Level Architecture

```text
CLI (Clap)
  |
  v
Application / Orchestration
  |-- Queue and scheduler
  |-- Download lifecycle and cancellation
  |-- Recovery coordinator
  |-- Configuration
  |
  +--> HTTP Download Engine
  |      |-- URL probe and response validation
  |      |-- Sequential downloader
  |      |-- Segmented downloader
  |      |-- Retry and timeout policy
  |      |-- Progress aggregation
  |
  +--> Storage Layer
  |      |-- SQLite repository
  |      |-- Recovery metadata
  |      |-- Partial-file and segment writer
  |      |-- Finalization
  |
  +--> ZIP Extraction Engine
         |-- Archive validation
         |-- Safe path handling
         |-- Extraction state and errors
```

The application is a **modular monolith**: one executable with explicit module boundaries. It does not require separate services or processes.

## 5. Module Responsibilities

Suggested source layout:

```text
swift-fetch/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── docs/
│   └── architecture.md
├── src/
│   ├── main.rs
│   ├── cli/
│   │   ├── mod.rs
│   │   └── commands.rs
│   ├── app/
│   │   ├── mod.rs
│   │   ├── manager.rs
│   │   ├── scheduler.rs
│   │   └── recovery.rs
│   ├── download/
│   │   ├── mod.rs
│   │   ├── http.rs
│   │   ├── probe.rs
│   │   ├── sequential.rs
│   │   ├── segmented.rs
│   │   ├── retry.rs
│   │   └── progress.rs
│   ├── storage/
│   │   ├── mod.rs
│   │   ├── repository.rs
│   │   ├── schema.rs
│   │   └── file_writer.rs
│   ├── archive/
│   │   ├── mod.rs
│   │   └── zip_extract.rs
│   ├── config.rs
│   ├── error.rs
│   └── logging.rs
└── tests/
    ├── http_download.rs
    ├── resume_recovery.rs
    ├── segmented_download.rs
    ├── zip_extraction.rs
    └── fixtures/
```

### CLI Layer

Responsibilities:

- Parse and validate commands, URLs, paths, and options.
- Load URL lists from text files.
- Configure output directory, concurrency, and extraction behavior.
- Display concise progress and final results.
- Return meaningful exit codes.
- Delegate actual work to the application layer; never implement HTTP or file-transfer logic directly.

### Application and Orchestration Layer

Responsibilities:

- Own the download queue and lifecycle.
- Schedule files within global concurrency limits.
- Coordinate per-file download strategy and segment workers.
- Coordinate pause, cancellation, retries, recovery, finalization, and extraction.
- Reconcile persistent state during startup.
- Ensure state transitions are explicit and valid.

### HTTP Download Engine

Responsibilities:

- Use shared, configured HTTP clients.
- Follow redirects according to a defined policy.
- Inspect server capabilities and response metadata.
- Choose sequential or segmented transfer.
- Validate HTTP status, content length, byte ranges, and remote validators.
- Stream data using bounded buffers.
- Apply timeouts and bounded retry policies.
- Aggregate progress and transfer metrics.

### Storage Layer

Responsibilities:

- Persist queue, download, segment, and extraction metadata.
- Coordinate database state with partial-file writes.
- Write segments to non-overlapping byte ranges.
- Finalize partial files safely.
- Prevent unintended overwrites.
- Reconcile database records with files during recovery.

### ZIP Extraction Engine

Responsibilities:

- Run only after successful download finalization.
- Validate the archive before or during extraction.
- Prevent path traversal and unsafe writes outside the extraction destination.
- Handle existing files without silently overwriting user data.
- Preserve the downloaded archive if extraction fails.
- Persist extraction status independently from download status.
- Allow extraction to be retried without downloading the archive again.

## 6. Download Scheduling and Parallelism

Use two independently bounded levels of concurrency:

1. **File-level concurrency:** Several different URLs may download simultaneously, subject to a global limit.
2. **Segment-level concurrency:** A supported large file may use multiple HTTP byte-range requests, subject to a per-file limit and the global transfer limit.

The effective number of active transfers must be bounded globally; multiplying file and segment settings must never result in unbounded work.

Starting configuration values for benchmarking, not guaranteed optimal values:

| Setting | Initial value |
|---|---:|
| Concurrent files | 3 |
| Segments per file | 4 maximum |
| Minimum file size for segmentation | 32 MiB |
| Segment size | Approximately 8 MiB |
| Connection timeout | 15 seconds |
| Retry attempts | 5 with bounded backoff |
| Progress refresh | 4 updates per second |
| Automatic ZIP extraction | Enabled |
| Existing-file behavior | Never overwrite silently |

Allow these values to be configured through CLI options or a small configuration file. Tune them only using repeatable benchmarks.

### Transfer Strategy Selection

- Use sequential streaming when the file is small, range support is absent, or segmentation is unlikely to help.
- Use segmented downloading only when the server reliably supports byte ranges and the expected benefit justifies the added complexity.
- If range support is absent or invalid, fall back to a safe sequential transfer.
- Do not assume that additional connections increase throughput; server limits and network conditions may make parallelism slower.

## 7. Download Lifecycle and State Model

Represent lifecycle states explicitly. A minimal model can include:

- `queued`
- `probing`
- `downloading`
- `paused`
- `retrying`
- `finalizing`
- `extracting`
- `completed`
- `failed`
- `cancelled`

State transitions must be validated by the application layer. Download completion and ZIP extraction completion are separate outcomes: a file can be successfully downloaded while extraction fails.

Do not mark a download complete until all expected bytes and segments have been validated, writes have succeeded, and finalization has succeeded.

## 8. Resume, Recovery, and Integrity

### HTTP validation

- Validate `206 Partial Content` responses and `Content-Range` values before accepting segment bytes.
- If a range request receives an unexpected full response, do not write it as a segment.
- Record ETag or Last-Modified validators when available.
- Detect remote changes before resuming when validators permit it.
- If the remote object changed or safe resumption cannot be established, restart rather than risk combining different versions.
- Handle servers with unknown content length or no range support without assuming resume is possible.

### File and metadata consistency

- Track bytes received, bytes written, and completed segments distinctly.
- Persist segment completion only after the corresponding writes succeed.
- On startup, reconcile the database, partial file, expected size, and segment state.
- Use a recovery protocol that handles crashes between filesystem writes and SQLite transactions; these operations do not form one atomic transaction.
- Keep incomplete files distinguishable from finalized files.
- Make recovery idempotent so repeated restarts do not corrupt data or duplicate completed work.

### Retry policy

- Retry eligible transient network failures with bounded exponential backoff and jitter.
- Do not retry permanent failures indefinitely.
- Apply connection and overall operation timeouts.
- Preserve trustworthy progress across retries.
- Surface permanent errors and actionable diagnostics to the CLI.

## 9. Persistence Model

Use SQLite as the source of truth for queue and recovery metadata, not as a replacement for the file contents.

Minimal tables:

| Table | Key information |
|---|---|
| `downloads` | URL, destination, status, expected size, validators, retry count, timestamps |
| `segments` | Download ID, byte range, state, confirmed progress |
| `extractions` | Archive path, destination, state, error details |
| `schema_version` | Database schema migration version |

Use transactions for state transitions that must be consistent within the database. Keep database operations short and prevent metadata writes from becoming a bottleneck. A controlled database-writer path is sufficient for this single-user application.

Store the database and recovery metadata in the user's local application-data directory, separate from the selected download directory.

## 10. Windows-Specific Behavior

- Start with Rust standard file APIs and positioned writes; introduce lower-level Win32 I/O only if benchmarks demonstrate a measurable need.
- Handle access-denied errors, disk-full conditions, file locks, invalid paths, and duplicate filenames explicitly.
- Use safe temporary or partial files and finalize only after successful validation.
- Keep queue metadata separate from download destinations.
- Handle Ctrl+C by stopping new work, cancelling active transfers, preserving trustworthy state, and exiting with a meaningful status.
- Ensure recovery also works after forced termination or power loss, when graceful shutdown cannot run.
- Run as a normal CLI process. Do not install a Windows service, resident daemon, or auto-start process in the initial version.

## 11. Configuration and CLI Responsibilities

The CLI should support these workflows:

- Download one URL.
- Download multiple URLs in one invocation.
- Load URLs from a text file.
- Choose an output directory.
- Configure global file concurrency and per-file segment limits.
- Inspect queued, active, completed, and failed downloads.
- Resume unfinished downloads.
- Retry failed downloads.
- Enable or disable automatic ZIP extraction.
- Display concise progress by default and detailed diagnostic logs when requested.

Exact command names and syntax belong in CLI documentation rather than being hard-coded into the architecture.

## 12. Error Handling and Observability

Use typed errors at module boundaries. Distinguish at least:

- Input and configuration errors.
- HTTP and TLS failures.
- Unsupported server capabilities.
- Range and content validation errors.
- Retry exhaustion.
- Filesystem and disk-space errors.
- Database and recovery errors.
- ZIP validation and extraction errors.
- Cancellation and shutdown outcomes.

Use `tracing` for structured diagnostics with download IDs, segment IDs, retry attempts, and error categories. Default terminal output should remain concise. Do not include secrets, authentication tokens, or sensitive URL query parameters in logs.

## 13. Testing and Performance Validation

Use a controllable local HTTP test server to simulate:

- Normal sequential downloads.
- Correct and incorrect byte-range responses.
- Servers that ignore range requests.
- Interrupted connections and transient server errors.
- Slow responses and timeouts.
- Unknown content lengths.
- Changed remote validators.
- Duplicate names, disk-full errors, and access failures.
- Process termination and subsequent recovery.
- ZIP path traversal and extraction failures.

Test the engine against a sequential baseline under identical conditions. Measure throughput, CPU, RAM, disk activity, resource bounds, final file correctness, and resume recovery. Use property-based tests where useful for range calculations and state transitions.

## 14. Acceptance Criteria

- One URL, multiple URLs, and text-file URL lists work from the CLI.
- Concurrent files and segmented transfers respect configured global and per-file limits.
- Segmentation is used only when supported and useful.
- Interrupted downloads resume correctly when the server and saved metadata allow it.
- Unsupported or unsafe resumption falls back to a safe restart.
- Network failures, timeouts, disk errors, and invalid responses never produce false completion.
- Process termination and Windows restart leave the queue recoverable.
- Completed ZIP files are extracted safely, and extraction failures preserve the original archive.
- Repeated recovery attempts do not corrupt files or duplicate completed work.
- Performance claims are backed by repeatable benchmarks rather than assumptions.

## 15. Product Principle

Build the smallest reliable tool that performs the essential workflow well:

**Accept URLs → download concurrently where safe → recover interrupted transfers → validate and finalize files → extract ZIP archives → report results.**

Reliability and file integrity take priority over feature count and peak-speed claims.
