# AGENTS.md

SwiftFetch — Windows-first CLI download manager (Rust, JDownloader-2-inspired). Crate/binary `swift-fetch`, one package at the repo root. Phase 0 (build foundation) and Phase 1 (CLI shell, config, errors, logging) are complete; commands dispatch honest not-implemented errors until their phase. Live status: the **bottom** of `changes.md` (append-only) plus `git log`/`git status` — verify at session start; status lines in prose go stale.

## Source of truth (read before any work)

- `DOCS/phases.md` — **the plan.** Work only on the explicitly assigned phase. §2 rules bind every phase; stop at the phase boundary and wait.
- `DOCS/architecture.md` — module layout, lifecycle states, persistence model, testing strategy.
- `DOCS/PRD.md` — product scope; anything not listed there (GUI, browser hooks, plugins, telemetry, daemons/services, non-ZIP archives) is out of scope.

Starting a phase requires reading all three, plus inspecting existing repo/tests. No spec exists yet for a section → build smallest thing meeting that phase's exit gate only.

## Tracking-file law (the rule agents break most)

- `changes.md` (changelog) and `decision.md` (ADRs) are **append-only at the very bottom** (ADR-0010, adopted after an edit destroyed the Phase 0 heading). Never reword, move, or delete an existing entry — supersede via a new addendum/ADR that references the old ID. Typos in history are disclosed, not fixed.
- Every entry carries: commit hash(es), files changed, and verification **actually run with observed results**.
- Naming (ADR-0011): display name **SwiftFetch**; crate/binary/env-var form `swift-fetch`; historical records keep `download-manager` and are mapped via the ADR — don't "fix" them.

## Package location

Single-crate modular monolith at the **repo root** (ADR-0001, confirmed with the owner): `Cargo.toml` and `src/` live next to `DOCS/`, not in a `swift-fetch/` subfolder, and `DOCS/` is intentionally uppercase vs the architecture sketch's `docs/`. Use the module layout in `DOCS/architecture.md` §5 (cli/, app/, download/, storage/, archive/, config.rs, error.rs, logging.rs). No `lib.rs` yet — unit tests are inline; a thin lib split would be an ADR, not a quiet change.

## Stack and dependency discipline

Approved stack (architecture §3, not swappable): Rust (edition 2021) + Tokio + Reqwest + SQLite (schema-version table from first persistence) + Clap + `tracing`/`tracing-subscriber` + thiserror + serde + a maintained Rust ZIP crate.

- Each crate is added **only in the phase that first uses it** (ADR-0003); `Cargo.lock` is always committed.
- Test-only crates (currently tokio/hyper/hyper-util/http-body-util/bytes) belong in `[dev-dependencies]` and must never leak into the product binary.
- One shared, configured `reqwest::Client` — never per-request clients.
- Narrow `#[allow(dead_code)]` waivers citing their future phase are the sanctioned bridge for not-yet-wired taxonomy/helpers (`src/error.rs`, `src/logging.rs`, `tests/common/`); keep `-D warnings` meaningful instead of deleting contracted code.

## Verification (canonical recipe, ADR-0008 — run in this order)

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

- Once per phase handoff, run the full sequence in a **fresh clone** (temp dir), not just the working tree.
- Record results (actually observed) + commit hash in the phase's `changes.md` entry; report only what you ran.
- Release build + smoke test of the release exe against a local fixture is a Phase 14 gate.

## CLI contract (Phase 1, ADR-0009 — locked by tests)

- Commands `download`/`status`/`resume`/`retry` + global `-v/--verbose`. Exit codes: `0` success, `1` operation cannot proceed (incl. not-implemented), `2` invalid invocation/config (Clap parse + input rejection both land here).
- An unwired command prints `error: the <command> command is not implemented yet; it will be available when its phase arrives` on **stderr**, exits `1`, leaves stdout empty — never fake success.
- Logging is tracing → **stderr**; stdout stays reserved for results/progress. Every logged URL goes through `logging::redact_url` (drops query + fragment wholesale, masks userinfo passwords; log the returned string, never the raw URL).
- Integration tests spawn the real binary via `CARGO_BIN_EXE_swift-fetch` + `std::process::Command`; help-text defaults mirror `AppConfig::default` (single source; a test locks the mapping).

## Testing rules that are easy to violate

- **Never use live external HTTP URLs** — all network tests go through the controlled local mock server in `tests/common/` (Phase 2: honored/broken/ignored ranges, truncation, delays, changed validators, omitted Content-Length) with deterministic `Fixture` bytes.
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
