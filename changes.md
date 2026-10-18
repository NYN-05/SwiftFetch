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

---

## Interim change — product rename to SwiftFetch (between Phases 1 and 2)

**Date:** 2026-10-18 · **Version:** 0.2.0 → 0.3.0 · **Nature:** owner-directed
identity change, no feature work (recorded at the bottom per the append-only
rule above and ADR-0010)

### What happened

The project owner named the product **SwiftFetch** and, when asked whether
the rename went beyond documentation text, chose the *full* rename: the brand
also becomes the technical identity. Recorded in full as ADR-0011
(`decision.md`, addendum #2 below-bottom).

### Code changes

| File | Change |
|---|---|
| `Cargo.toml` | Package/binary `download-manager` → `swift-fetch` (kebab-case Rust convention); version 0.2.0 → 0.3.0; description now "SwiftFetch — Windows-first command-line download manager" |
| `Cargo.lock` | Regenerated: package `swift-fetch` 0.3.0; committed per the standing rule |
| `src/cli/commands.rs` | clap `name = "SwiftFetch"` (version/help identity), about and long_about now "SwiftFetch — Windows-first command-line download manager." |
| `src/app/mod.rs` | Unit-test argv[0] placeholders → `"swift-fetch"` (4 sites) |
| `src/cli/mod.rs` | Unit-test argv[0] placeholders → `"swift-fetch"` (2 sites) |
| `tests/cli.rs` | `CARGO_BIN_EXE_download-manager` → `CARGO_BIN_EXE_swift-fetch`; version test renamed to `version_reports_program_name_and_version`, now asserts `SwiftFetch`; assertion substance otherwise unchanged |

### Document changes

| File | Change |
|---|---|
| `README.md` | Title `# SwiftFetch`; intro sentence names the product; usage example `swift-fetch --help` |
| `AGENTS.md` | Layout note now says no `swift-fetch/` subfolder (file stays untracked pending the owner's commit decision; disk content renamed) |
| `DOCS/PRD.md`, `DOCS/architecture.md`, `DOCS/phases.md` | Titles gain a "(SwiftFetch)" suffix |
| `DOCS/architecture.md` §5 | Layout-tree root label `download-manager/` → `swift-fetch/` |
| `changes.md`, `decision.md` | Appended only (this entry; ADR-0011). Historical records keep `download-manager` by design — the append-only rule forbids touching them; ADR-0011 is the official name mapping |

Intentionally unchanged: `DOCS/phases.md` line 7 cites the upstream spec
*filename* `windows-first-cli-download-manager-project.md` (citation, not
branding); generic descriptive prose such as "Windows-first command-line
download manager" (description, not name). The Phase 1 CLI contract —
commands (`download`/`status`/`resume`/`retry`), options, exit codes, help
honesty — is unchanged except for the program name.

### Verification (rename commit `56e7501`)

| Check | Result |
|---|---|
| `cargo fmt --check` | Pass, after applying one mechanical re-wrap (the shorter identifier moved three rustfmt line breaks; no semantic change) |
| `cargo clippy --all-targets --all-features -- -D warnings` | Pass (`swift-fetch v0.3.0`, zero warnings) |
| `cargo test` | Pass: 29 unit + 11 integration = 40/40, including the real-binary test asserting the version output contains `SwiftFetch` |
| Binary smoke | `swift-fetch --version` → `SwiftFetch 0.3.0`; `--help` → branded about text and `Usage:` line |
| Windows honesty | Built, formatted, linted, and tested on Windows MSVC (rustc 1.99.1); every check ran on Windows |

### Scope-control notes

- Owner-directed interim change between phases; it is **not** part of any
  phase's task list, and Phase 2 remains unstarted pending explicit
  instruction.
- No new dependencies; the deferred stack (Tokio, Reqwest, SQLite, ZIP,
  serde, url) is untouched.
- `AGENTS.md` is still untracked; only its identifier reference was renamed
  (its stale "Currently docs-only" status line remains as the owner left it).

---

## Git operations — .gitignore hardening, remote setup, first push (between Phases 1 and 2)

**Date:** 2026-10-18 · **Nature:** owner-directed repository/infrastructure
change; no product code affected

### What happened

1. **`.gitignore` updated.** The existing `/target` and `*.tmp` rules are
   kept verbatim; added backup-file patterns (`*.bak`, `*.rs.bk`), OS noise
   (`Thumbs.db`, `Desktop.ini`, `.DS_Store`), and local-only configuration
   (`.env`, `.vscode/`). Build artifacts and scratch files can no longer be
   staged accidentally. `Cargo.lock` remains intentionally tracked (stack
   rule); it does not match any ignore pattern. Verified before staging:
   no already-tracked file matches any new pattern, so no `git rm --cached`
   was needed.
2. **`AGENTS.md` enters version control.** The owner's explicit
   `git add -A` instruction settles the previously open "commit or not"
   question in favor of tracking. The file is committed exactly as written
   — including its stale "Currently docs-only" status line — under the
   append-only discipline; the agent does not rewrite owner prose.
3. **Remote + first push.** Remote `origin` =
   `https://github.com/NYN-05/SwiftFetch.git`; `main` was pushed upstream
   with `git push -u origin main`, shipping the complete history from
   Phase 0 onward. Recorded as **ADR-0012** (`decision.md`), which
   supersedes the local-only stance of ADR-0002 *via a new appended record*,
   never by editing the old record (ADR-0010 policy).

Honesty note: this entry and ADR-0012 are part of the same single commit
they describe, so they cannot carry that commit's hash in-file; the hash is
recorded in the git log on `origin/main` immediately after the push.



---

## Phase 2 — Controlled HTTP Test Infrastructure

- **Date:** 2026-10-18
- **Version:** `0.3.0` → `0.4.0`
- **Outcome:** Phase 2 exit gate met. Deterministic fixtures, a controlled
  loopback mock server, and reproducible failure scenarios now cover every
  HTTP behavior the transfer-engine phases will face; the suite runs fully
  offline. **No production download code** — `src/download`, `src/storage`,
  `src/archive` remain undeclared placeholder modules.

### Dependencies added (approved stack, test-only)

All under `[dev-dependencies]`, never `[dependencies]`: `tokio 1.53.3`
(rt-multi-thread, macros, net, io-util, time, sync, test-util),
`hyper 1.12.0` (http1, server, client), `hyper-util 0.1.21`
(client-legacy, client, http1, tokio), `http-body-util 0.1.5`,
`bytes 1.12.1`. Test-support crates compile under `tests/` only and are
never linked into the product binary (see ADR-0013). `Cargo.lock` committed.

### Code changes

| File | Change |
|---|---|
| `tests/common/mod.rs` | NEW. Declares the shared test-support modules |
| `tests/common/fixture.rs` | NEW. `pattern_byte(seed, index)` splitmix-style closed form; `Fixture` (seed, len) with `byte_at`, `bytes()`, inclusive/clamped `slice(start, end)`; six inline unit tests; default fixture seed 1, 4096 bytes |
| `tests/common/server.rs` | NEW. `MockServer` binds `127.0.0.1:0` (hyper 1, one accept task, per-connection tasks, watch-channel shutdown, `Drop` aborts the accept loop). Per-route `Behavior`: honored / ignored / broken-headers / broken-body ranges, 416 with `bytes */total`, full-cover → 200, malformed → 200, mid-body truncation, configurable statuses (sentinel `status-<code>` bodies, never fixture bytes), redirects (raw, unfollowed), omitted `Content-Length` (chunked), header + per-body-frame delays, static/changing ETag and Last-Modified (hit-counter driven; `Changing` Last-Modified cycles two precomputed valid IMF-fixdate strings — no chrono dependency), HEAD with exact Content-Length. Request log (method, path incl. query, raw Range), per-route hit counts, `shutdown_wait()`, reusable `http_client()` (hyper-util legacy client) |
| `tests/fixtures.rs` | NEW, 9 tests: golden first-32-bytes array + two FNV-1a checksums (recorded on Windows from the generator; inline 5-line FNV — no crypto crate), byte-by-byte `pattern_byte` agreement, exact slice windows, cross-seed distinctness, panic contract for out-of-range `byte_at` |
| `tests/mock_server.rs` | NEW, 18 tests: one per server mode; smoke start/stop asserting exact 200 + Content-Length + body and that the port refuses connections after `shutdown_wait`; request log arrival order; repeatable truncation across 3 attempts; delay scenario run twice with headers ≥ 90 ms, total ≥ 340 ms, total < 2000 ms; static vs changing validators (ETag three distinct values; Last-Modified cycles back); redirect answered raw with the target route untouched; status sentinels never equal fixture bytes; HEAD probe semantics; unknown routes → 404 sentinel |
| `Cargo.toml` | version `0.4.0`; `[dev-dependencies]` block with stack-discipline comment |

### Behavior contract (test infrastructure)

- Every engine-facing network surface has a local simulation: normal 200
  with exact Content-Length, correct 206 + Content-Range slicing, ignored
  ranges, mismatched Content-Range, right headers with wrong bytes, 416
  with `bytes */total`, malformed ranges falling back to 200, truncation
  that never completes a transfer, bounded repeatable delays, missing
  Content-Length, redirects the client must not auto-follow, validator
  changes between requests, and connection refusal after shutdown.
- Sentinel bodies for non-200 statuses make "an HTTP error body is not
  file content" expressly testable from Phase 3 onward.
- No test contacts a live external URL; loopback ephemeral ports only.

### Defects caught before commit (recorded honestly)

1. **Seed-blind first scramble.** `pattern_byte` v1 XORed the seed into
   bits ≥ 47 (`rotate_left(17)`), so seeds differing only in low bits
   (1 vs 2) produced identical content — the seed-distinctness test would
   have failed. Replaced with a splitmix-style mix (xor seed → multiply →
   rotate → multiply) before any commit; golden values were then recorded
   via a temporary print-only test and converted into strict asserts.
2. **Validators dropped from GET responses.** ETag/Last-Modified were
   attached only to the HEAD builder, so both validator tests failed on
   the first full run; a shared `with_validators` helper now supports
   every response the route returns (HEAD included).
3. **`shutdown_wait` held a `std::sync::MutexGuard` across `.await`**
   (clippy::await_holding_lock under `-D warnings`); the take now happens
   in its own statement so the guard drops before the await.
4. **Truncation test honesty on Windows.** The first truncation test
   assumed the client always sees response headers before the abort; on
   Windows the kernel can discard buffered response data on the abrupt
   close, surfacing the failure at `request()` instead of during
   `.collect()`. The test now accepts either outcome and proves in both
   that the transfer never completes.

### Verification — actual results (all run on Windows)

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings |
| `cargo test` | 79 passed (29 unit + 11 cli + 15 fixtures + 24 mock_server), 0 failed |
| Fresh-clone gate reproduction | see ADR-0008 recipe outcome appended after commit |

### Scope-control notes

- File-level `#![allow(dead_code)]` appears only in `tests/common/*`,
  each with a documented reason: the module is compiled into every
  integration test binary and each binary uses a subset of the helpers.
- Zero production dependencies added; zero `src/` changes this phase.
- Unmounted routes answer the 404 sentinel by design, so route typos in
  later tests can never silently serve fixture bytes.

### Phase 2 verification addendum (post-commit, appended 2026-10-18)

- **Commit:** `a5c0ce8` — "Phase 2: controlled HTTP test infrastructure
  (v0.4.0)" (code + tests + tracking files). The entry above and its hashes
  are part of that commit, so the hash is recorded here and in `git log`,
  not inside the entry itself (same honesty rule as the Phase 1 entry).
- **Fresh-clone gate reproduction** (ADR-0008 recipe: `git clone` of
  `a5c0ce8` to a fresh temp directory on Windows, working tree verified
  clean, recipe run in order):

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings |
| `cargo test` | 79 passed (29 unit + 11 cli + 15 fixtures + 24 mock_server), 0 failed |



