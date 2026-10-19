# AGENTS.md

SwiftFetch — Windows-first CLI download manager (Rust, JDownloader-2-inspired). Crate/binary `swift-fetch`, one package at the repo root; package version tracks the phase (`0.9.0` = Phase 7). Phases 0–7 have landed: CLI shell, mock-server test infrastructure, shared HTTP client + probing, the sequential transfer engine, SQLite persistence/lifecycle, single-file resume/retry/recovery, and URL batches with bounded concurrent scheduling all exist and are test-locked — but the commands are still unwired and print the honest not-implemented error (exit 1). Live status: the **bottom** of `changes.md` (append-only) plus `git log`/`git status` — verify at session start; status prose (this file, README, comments) goes stale.

## Source of truth (read before any work)

- `DOCS/phases.md` — **the plan.** Work only on the explicitly assigned phase. §2 rules bind every phase; stop at the phase boundary and wait.
- `DOCS/architecture.md` — module layout, lifecycle states, persistence model, testing strategy.
- `DOCS/PRD.md` — product scope; anything not listed there (GUI, browser hooks, plugins, telemetry, daemons/services, non-ZIP archives) is out of scope.
- `decision.md` — ADRs. Engine contracts are test-locked here: timeouts (ADR-0015), naming/finalization (ADR-0016), SQLite/migrations (ADR-0017), lifecycle graph (ADR-0018), resume/recovery (ADR-0019), batch scheduling (ADR-0020). Read the ADR for any area you touch before changing its behavior.

Starting a phase requires reading all three (plus the ADRs for its area), inspecting existing repo/tests. No spec exists yet for a section → build the smallest thing meeting that phase's exit gate only.

## Tracking-file law (the rule agents break most)

- `changes.md` (changelog) and `decision.md` (ADRs) are **append-only at the very bottom** (ADR-0010, adopted after an edit destroyed the Phase 0 heading). Never reword, move, or delete an existing entry — supersede via a new addendum/ADR that references the old ID. Typos in history are disclosed, not fixed.
- Every entry carries: commit hash(es), files changed, and verification **actually run with observed results**.
- Phase workflow (see the phase entries in `changes.md`): commit code + the phase's `changes.md` entry together — the entry cannot know its own hash — then append a post-commit "Phase N verification addendum" with the recorded hash and fresh-clone gate results, and commit that separately.
- Commit style in history: `Phase N: <title> (vX.Y.Z)` for phase work; `docs: …` for tracking updates.
- Naming (ADR-0011): display name **SwiftFetch**; crate/binary/env-var form `swift-fetch`; historical records keep `download-manager` and are mapped via the ADR — don't "fix" them.

## Package location

Single-crate modular monolith at the **repo root** (ADR-0001, confirmed with the owner): `Cargo.toml` and `src/` live next to `DOCS/`, not in a `swift-fetch/` subfolder, and `DOCS/` is intentionally uppercase vs the architecture sketch's `docs/`. Module layout per `DOCS/architecture.md` §5 — populated today: `cli/`; `app/` (`commands_handler`, `batch`); `download/` (`http`, `probe`, `sequential`, `progress`, `resume`, `retry`); `storage/` (`file_writer`, `schema`, `repository`). `src/archive/mod.rs` is an unwired doc-comment placeholder until Phase 10 (never declared in `main.rs`). No `lib.rs` yet — unit tests are inline; a thin lib split would be an ADR, not a quiet change.

## Stack and dependency discipline

Approved stack (architecture §3, not swappable): Rust (edition 2021) + Tokio + Reqwest + SQLite (schema-version table from first persistence) + Clap + `tracing`/`tracing-subscriber` + thiserror + serde + a maintained Rust ZIP crate.

- Each crate is added **only in the phase that first uses it** (ADR-0003); `Cargo.lock` is always committed.
- Deliberate dependency shapes — do not "fix" casually: `reqwest` is `default-features = false` with no compression features (a transparent content transform would invalidate byte-range offsets); product `tokio` features are added with the phase that first needs them (now `fs`, `io-util`, `time`, `sync`, `macros`, `rt`; any further feature follows the same first-use rule, ADR-0003); `rusqlite` uses `bundled` so Windows builds need no system SQLite.
- Feature-unification trap: `clippy --all-targets` and `cargo test` unify dev-dependency features, so a missing product feature can pass the canonical recipe (Phase 6 shipped a production `tokio::select!` that only failed in plain `cargo build`). When touching tokio features or adding production macros/spawn, run plain `cargo build` too.
- Test-only crates (currently hyper/hyper-util/http-body-util; tokio/bytes appear in both sections) belong in `[dev-dependencies]` and must never leak into the product binary.
- One shared, configured `reqwest::Client` — never per-request clients. `HttpClient` is `Clone`; clones share the one pool.
- Narrow `#[allow(dead_code)]` waivers citing the phase that wires the code are the sanctioned bridge for not-yet-called layers (`download/*`, `storage/*`, `app/batch.rs`; the command wiring that consumes them arrives in Phase 11); keep `-D warnings` meaningful instead of deleting contracted code or widening a waiver.

## Verification (canonical recipe, ADR-0008 — run in this order)

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

- `cargo test` takes minutes (network-bound tests use real socket delays against the local mock) — allow a generous timeout; a 2-minute kill is not a failure.
- Once per phase handoff, run the full sequence in a **fresh clone** (temp dir), not just the working tree.
- Record results (actually observed) + commit hash in the phase's `changes.md` entry; report only what you ran.
- Release build + smoke test of the release exe against a local fixture is a Phase 14 gate.

## CLI contract (Phase 1, ADR-0009 — locked by tests)

- Commands `download`/`status`/`resume`/`retry` + global `-v/--verbose`. Exit codes: `0` success, `1` operation cannot proceed (incl. not-implemented), `2` invalid invocation/config (Clap parse + input rejection both land here).
- An unwired command prints `error: the <command> command is not implemented yet; it will be available when its phase arrives` on **stderr**, exits `1`, leaves stdout empty — never fake success.
- Logging is tracing → **stderr**; stdout stays reserved for results/progress. Every logged URL goes through `logging::redact_url` (drops query + fragment wholesale, masks userinfo passwords; log the returned string, never the raw URL).
- Integration tests spawn the real binary via `CARGO_BIN_EXE_swift-fetch` + `std::process::Command`; help-text defaults mirror `AppConfig::default` (single source; a test locks the mapping).

## Testing rules that are easy to violate

- **Never use live external HTTP URLs** — all network tests go through the controlled local mock server in `tests/common/` (honored/broken/ignored ranges, truncation, delays, changed validators, omitted Content-Length, plus Phase 6 countdowns `with_fail_first`/`with_truncate_first`) with deterministic `Fixture` bytes. Integration targets: `cli`, `fixtures`, `mock_server` (`cargo test --test <name>`).
- Mock gotchas (cost real time in Phase 6): truncation ends in a clean mid-body EOF — a stream error poisons the pooled connection for the next request; the probe's evidence GET consumes a `truncate_first` slot, so budget `n + probes` slots for `n` truncated transfer GETs.
- `tests/common/` is also compiled **into the binary's unit tests** via `#[cfg(test)] #[path = "../tests/common/mod.rs"]` in `src/main.rs` (ADR-0014); keep it compiling for both, and expect shared fixture tests to be counted in several targets.
- Don't remove/weaken/skip/rewrite a test to make code pass. Add tests for new behavior in the same phase.
- Only report tests actually run and observed.
- Recovery tests must be idempotent and cover crash-between-file-write-and-DB-commit cases (fs + SQLite are not one atomic transaction).

## Windows honesty rule

Windows is the first-class platform. Windows-specific behavior (reserved filenames, file locks, access-denied, forced-termination recovery) must be validated on Windows; if any check ran elsewhere, explicitly list which Windows checks are unverified. Never present a non-Windows run as Windows proof.

## Reliability invariants (never break these)

- Never mark incomplete/corrupt transfers as complete; finalize via same-volume rename with a no-overwrite policy (ADR-0016: sanitized effective-URL names, deterministic `name (n)` siblings, engine-owned `*.swiftfetch-<pid>-<seq>.part` partials).
- Never silently overwrite user files (destinations or extraction collisions).
- No unbounded retries, tasks, buffers, or concurrency (global transfer limit caps file×segment concurrency; batch workers are `min(max_concurrent_files, URLs)` over bounded channels — never one task per URL).
- Every URL gets a persistent record before its first network byte, and one `(url, output dir)` scope owns exactly one unfinished record — later invocations adopt or reschedule, never duplicate (ADR-0020).
- Never append resumed bytes without validating status/range/validators (206 + matching `Content-Range`; changed ETag/Last-Modified ⇒ restart, not append).
- HTTP error bodies are not file content; ZIP extraction never writes outside its destination; preserve the archive when extraction fails.
- Logs must not contain tokens or sensitive URL query params.
- SQLite DB lives in the user's local app-data dir (`%LOCALAPPDATA%\SwiftFetch\swiftfetch.sqlite3`), separate from the download directory; metadata only, never file contents (no BLOB columns, test-enforced).
- Download states move only along the ADR-0018 tested graph; `failed` requires an error category and other states reject one; a downloaded file never flips to `failed` because extraction failed.
