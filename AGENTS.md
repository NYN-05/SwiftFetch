# AGENTS.md

Rust CLI download manager (Windows-first, JDownloader-2-inspired). Currently docs-only; no Cargo.toml or source yet.

## Source of truth (read before any work)

- `DOCS/phases.md` — **the plan.** Work only on the explicitly assigned phase. §2 rules bind every phase; stop at the phase boundary and wait.
- `DOCS/architecture.md` — module layout, lifecycle states, persistence model, testing strategy.
- `DOCS/PRD.md` — product scope; anything not listed there (GUI, browser hooks, plugins, telemetry, daemons/services, non-ZIP archives) is out of scope.

Starting a phase requires reading all three, plus inspecting existing repo/tests. No spec exists yet for a section → build smallest thing meeting that phase's exit gate only.

## Package location

Single-crate modular monolith at the **repo root** (user decision): `Cargo.toml` and `src/` live next to `DOCS/`, not in a `swift-fetch/` subfolder. Use the module layout in `DOCS/architecture.md` §5 (cli/, app/, download/, storage/, archive/, config.rs, error.rs, logging.rs).

## Fixed stack — do not swap

Rust + Tokio + Reqwest + SQLite (with schema-version table from first persistence) + Clap + `tracing`/`tracing-subscriber` + thiserror + serde + a maintained Rust ZIP crate. Add each only when the phase needs it. Commit `Cargo.lock`. One `reqwest::Client` reused; never per-request clients.

## Verification commands

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

Check order when landing changes: fmt → clippy → test. Release build + smoke test of the release exe (local fixture) is part of Phase 14's gate.

## Testing rules that are easy to violate

- **Never use live external HTTP URLs** — all network tests go through the controlled local mock server (built in Phase 2: range support, broken/ignored ranges, truncation, delays, changed validators, no Content-Length).
- Don't remove/weaken/skip/rewrite a test to make code pass. Add tests for new behavior in the same phase.
- Only report tests actually run and observed.
- Recovery tests must be idempotent and cover crash-between-file-write-and-DB-commit cases (fs + SQLite are not one atomic transaction).

## Windows honesty rule

Windows is the first-class platform. Windows-specific behavior (reserved filenames, file locks, access-denied, forced-termination recovery) must be validated on Windows; if any check ran elsewhere, explicitly list which Windows checks are unverified. Never present a non-Windows run as Windows proof.

## Reliability invariants (never break these)

- Never mark incomplete/corrupt transfers as complete; finalize via same-volume rename with a no-overwrite policy.
- Never silently overwrite user files (destinations or extraction collisions).
- No unbounded retries, tasks, buffers, or concurrency (global transfer limit caps file×segment concurrency).
- Never append resumed bytes without validating status/range/validators (206 + matching `Content-Range`; changed ETag/Last-Modified ⇒ restart, not append).
- HTTP error bodies are not file content; ZIP extraction never writes outside its destination; preserve the archive when extraction fails.
- Logs must not contain tokens or sensitive URL query params.
- SQLite DB lives in the user's local app-data dir, separate from the download directory; it stores metadata only, never file contents.
