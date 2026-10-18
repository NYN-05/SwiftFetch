# Changelog (`changes.md`)

Running changelog for `download-manager`. It records every code and feature
change, newest first, one section per development phase as defined in
`DOCS/phases.md`.

`DOCS/phases.md`, `DOCS/PRD.md`, and `DOCS/architecture.md` remain the source
of truth for scope; this file records only what was actually built, changed,
and verified. Key technical decisions and their rationale live in the
companion ADR file, [`decision.md`](decision.md).

## Maintenance rules

- **Append-only history.** Add a new entry per phase (or after any interim fix
  between phases). Never rewrite or delete previous entries; superseded
  behavior is corrected by new entries, not by editing old ones.
- Every entry must include: commit hash(es), date, what changed, files added
  or changed, and verification (tests/checks actually run and their observed
  results).
- Product-scope changes are only valid when the project owner changes the
  approved documents; such changes are noted here under the phase in which
  they take effect, never silently.

---

## Phase 0 — Repository and Build Foundation

- **Date:** 2026-10-18
- **Commits:** `83efc56` — "Phase 0: repository and build foundation"
- **Outcome:** Phase 0 exit gate met. The project builds and tests
  reproducibly from a clean checkout; the executable starts and exits
  predictably. **No download behavior is implemented yet.**

### Added

| File | Change |
|---|---|
| `Cargo.toml` | Package `download-manager` 0.1.0, edition 2021; `[dependencies]` intentionally empty (Stack crates deferred to first-use phases) |
| `Cargo.lock` | Dependency lockfile committed for reproducible application builds |
| `src/main.rs` | Entry point: prints one-line banner `download-manager 0.1.0 (Phase 0 foundation build)` to stdout and exits `0`; includes unit test `banner_names_the_application` |
| `src/cli/mod.rs`, `src/app/mod.rs`, `src/download/mod.rs`, `src/storage/mod.rs`, `src/archive/mod.rs` | Intended module directories from `DOCS/architecture.md` §5 created as doc-comment placeholders only — no stub functions, no abstractions; unwired until their phases |
| `tests/entry_point.rs` | Integration smoke test `executable_starts_and_exits_successfully`: runs the built exe via `CARGO_BIN_EXE_download-manager`, asserts exit success and banner on stdout |
| `README.md` | Project purpose, target platform (Windows first), prerequisites, build/test command sequence |
| `.gitignore` | Ignores `/target`; `Cargo.lock` is deliberately not ignored |
| `.gitattributes` | `* text=auto` for consistent line endings on a Windows-first codebase |
| `rustfmt.toml` | `edition = "2021"` so formatting matches the build edition |
| `changes.md`, `decision.md` | This changelog and the ADR record (this entry) |

### Initialized

- Git repository on branch `main` (was previously not a version-controlled
  directory). Initial commit includes the pre-existing `DOCS/` folder,
  unchanged.

### Verification — actual results (all run on Windows)

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings |
| `cargo test` | 2/2 passed |
| `cargo run` | banner printed, exit code 0 |
| Clean clone to temp dir → fmt → clippy → test | all pass |

### Scope-control notes

- No GUI, service, daemon, browser integration, or unrelated feature was
  introduced.
- No stack dependencies (Tokio, Reqwest, Clap, SQLite, ZIP, tracing) were
  added yet — none are "first required" in Phase 0.
- Known limitation: no remote is configured; "tracked" means tracked in local
  git.
