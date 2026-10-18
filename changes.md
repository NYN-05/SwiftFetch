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

## Phase 1 — CLI Shell, Configuration, Errors, and Logging

- **Date:** 2026-10-18
- **Version:** `0.1.0` → `0.2.0`
- **Outcome:** Phase 1 exit gate met. The Clap-based command surface, typed
  errors, configuration model, and logging are in place; every command
  dispatches an honest not-implemented error. **No transfer engine.**

### Dependencies added (approved stack, first use)

`clap 4.6.7` (derive), `thiserror 2.0.21`, `tracing 0.1.44`,
`tracing-subscriber 0.3.23`; transitive crates recorded in the committed
`Cargo.lock`. `serde` remains deferred (no config file is read in Phase 1).

### Code changes

| File | Change |
|---|---|
| `src/main.rs` | Rewritten as thin entry point: parse CLI → install logging → `app::run` → map typed errors to exit codes (0/1/2). Phase 0 banner removed (supersession recorded in ADR-0009) |
| `src/cli/commands.rs` | NEW. Clap contract: `download` (URLs, `--url-file`, `--output`, `--files`, `--segments`, `--retries`, `--no-extract`), `status`, `resume`, `retry`, global `--verbose`. Help text discloses that no command is wired to the transfer engine yet |
| `src/cli/mod.rs` | Rewritten from placeholder: option→config mapping, early input validation (URL scheme, missing input), unit tests |
| `src/app/mod.rs` | Rewritten from placeholder: `run()` dispatches commands |
| `src/app/commands_handler.rs` | NEW: per-command handlers; Phase 1 contains only the not-implemented guards |
| `src/config.rs` | NEW: `AppConfig` (output dir, concurrency 3, segments 4, retries 5, ZIP extraction enabled) with validation: `--files/--segments` must be ≥ 1; empty/NUL/non-Unicode output paths rejected; `retries = 0` is valid (no retries) |
| `src/error.rs` | NEW: `ErrorCategory` taxonomy from architecture §12 with stable kebab-case ids and exit codes (input → 2, all else → 1); `AppError` keeps user-facing message separate from internal diagnostic source |
| `src/logging.rs` | NEW: tracing subscriber on stderr (concise INFO; `--verbose` → DEBUG); `redact_url` strips query+fragment entirely and masks userinfo passwords — the rule every later module must apply when logging URLs |
| `tests/cli.rs` | NEW integration tests (11) |
| `tests/entry_point.rs` | REMOVED — superseded, not weakened: it pinned the Phase 0 banner contract that this phase deliberately replaces; `--help`/`--version` tests now cover the new contract (ADR-0005 superseded by ADR-0009) |

### Behavior contract

- Exit codes: `0` success (not reachable in Phase 1), `1` requested
  operation cannot proceed (incl. not-implemented), `2` invalid
  invocation/configuration (Clap parse and config validation both land here).
- Not-implemented commands print `error: the <command> command is not
  implemented yet; it will be available when its phase arrives` on stderr,
  never report success, and leave stdout empty.

### Defect caught before commit

The redaction test suite exposed a bug pre-commit: a bare username
(`user@host`) was masked as if it contained a password. Fixed: password-only
userinfo (empty username) alone is masked.

### Verification — actual results (all run on Windows)

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings |
| `cargo test` | 40 passed (29 unit + 11 integration), 0 failed |

### Scope-control notes

- URL-list file *content* is not read yet (engine phases); `--url-file`
  presence alone satisfies the input requirement.
- URL validation is scheme-level only; deep URL parsing arrives with the
  HTTP layers (Phases 3/7).
- Narrow `allow(dead_code)` waivers mark taxonomy/error helpers that only
  tests exercise today; each cites the phase that makes them load-bearing
  (see `src/error.rs`, `src/logging.rs`).
- `AGENTS.md` (owner-added governance notes) remains uncommitted; its
  "docs-only" status line is stale relative to the repo.


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

---

## Tracking addendum — appended 2026-10-18 (owner-directed, append-only)

**Placement convention (owner decision):** from now on, *all* updates to this
file are added at the very bottom — strictly append-only. Nothing above this
addendum is ever reworded, moved, or deleted, including entries that predate
this rule. This supersedes the "newest first" placement note in the
Maintenance rules section above, which stays as-written for the historical
record.

### Integrity disclosure #1 — the lost Phase 0 heading

The incident: during the Phase 1 entry insertion (commit `9903795`), the
section heading `## Phase 0 — Repository and Build Foundation` was
accidentally destroyed by the edit that inserted the Phase 1 section. The
heading is the *only* content that was lost.

Everything else survived completely intact: the complete Phase 0 record —
Date, Commits (`83efc56`), Outcome, the full "Added" file table, the
"Initialized" section, the verification table, and the scope-control notes —
is present, unchanged, directly below the Phase 1 section in this file (from
the line beginning `- **Date:** 2026-10-18` / `**Commits:** 83efc56` on).

The heading belonged directly above that surviving record. Per the strict
append-only rule it is restored here (for the record) rather than re-inserted
mid-file:

```
## Phase 0 — Repository and Build Foundation
```

Readers: treat the orphaned block between the Phase 1 section and this
addendum as the Phase 0 record under this restored heading.

### Integrity disclosure #2 — an in-place mutation before this rule

One historical record was reworded in place *before* the append-only rule was
established, disclosed here so no change to history remains silent:

- Phase 0 "Added" table, `Cargo.toml` row: the parenthetical
  "(stack CABs deferred to first-use phases)" was typo-corrected in place to
  "(Stack crates deferred to first-use phases)" during the post-Phase 0
  documentation commit. Meaning unchanged; capitalization drifted
  mid-sentence as part of that fix ("Stack" vs "stack"). Left as-is per this
  rule.

### Phase 1 tracking addendum — missing metadata

The Phase 1 entry above was written without its commit line, violating the
maintenance rule that every entry carries commit hashes. Recorded here,
append-only:

- **Commits (Phase 1):**
  - `9903795` — "Phase 1: CLI shell, configuration, errors, and logging"
    (15 files changed, 1819 insertions(+), 64 deletions(-); includes
    `Cargo.lock` with the Phase 1 dependency set)
  - `826e7f9` — "docs: add changes.md changelog and decision.md ADR; link
    both from README" (the documentation commit that introduced this file
    and `decision.md`, between Phases 0 and 1)
- **Repository state at Phase 1 close:** branch `main`; `Cargo.lock` tracked;
  canonical verification re-run on a fresh clone per ADR-0008
  (fmt → clippy → test; 29 unit + 11 integration, all passed on Windows);
  `AGENTS.md` present but intentionally untracked pending owner instruction.

These addendum entries are part of the Phase 1 tracking record and must be
read together with it.

