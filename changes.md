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

- **Commit:** `a5c0ce8` - "Phase 2: controlled HTTP test infrastructure
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

---

## Phase 3 - HTTP Client and Resource Probing

- **Date:** 2026-10-18
- **Version:** `0.4.0` → `0.5.0`
- **Outcome:** Phase 3 exit gate met (working tree). A shared, configured
  `reqwest::Client` and the resource prober are implemented and network-
  tested against the Phase 2 mock server; URLs are validated at the scheme
  level with typed `Input` errors; probe facts come only from response
  headers and contradictory metadata is never silently trusted. Every CLI
  command still dispatches an honest not-implemented error (wiring is
  Phase 11). No storage, no SQLite, no ZIP work.

### Dependencies added (approved stack, first use)

`reqwest 0.12.28` (resolved from `0.12`) in `[dependencies]` with
`default-features = false`, `features = ["native-tls", "http2"]`:
native-tls maps to Windows SChannel on the first-class platform and no
compression feature is enabled. Verified empirically with `cargo tree -i`:
`flate2`, `brotli`, and `zstd` match **no packages**, so no transparent
content transform can shift byte offsets. Every request additionally sends
`Accept-Encoding: identity`. Transitive crates are recorded in the committed
`Cargo.lock`. Test-only crates were extended, not moved: the binary's unit
tests now also compile `tests/common/` under `cfg(test)` (ADR-0014), so the
Phase 2 dev-dependencies are shared, never leaked into the product build.

### Code changes

| File | Change |
|---|---|
| `Cargo.toml` | version `0.5.0`; first `[dependencies]` entry `reqwest` with a first-use phase comment (ADR-0003) |
| `Cargo.lock` | reqwest `0.12.28` + transitive crates; committed per the standing rule |
| `src/download/mod.rs` | Phase 3 module declared: `http` (shared client) + `probe` (prober); doc comment states the prober contract and points at ADR-0014 for test reuse |
| `src/download/http.rs` | NEW. `HttpClient` wraps one configured `reqwest::Client` (connect timeout 15 s and provisional overall timeout 30 s per architecture §6, redirect limit 10, `Accept-Encoding: identity` on every request). `send()` issues HEAD/GET (+ optional Range) and maps failure to typed `AppError`s; `http_error()` keeps a raw URL, when reqwest embeds one, only in the hidden diagnostic source while the user-facing message carries `logging::redact_url` output. Two inline unit tests lock the timeout defaults |
| `src/download/probe.rs` | NEW. `probe(client, url)` (≤ 2 bounded requests, bodies never read): strict URL validation (`http`/`https` only, otherwise `Input`/exit 2); primary `HEAD`; 405/501 fall back to one `bytes=0-0` ranged GET as primary; `Content-Length: 0` short-circuit (size `Some(0)`, ranges `Unknown`, no follow-up request); strict header parsers (`Content-Length` with multi-header conflict rejection, `Content-Range` exact-interval form, 416 `bytes */N` total form, `Content-Encoding` identity/transform classification); contradiction rules (HEAD vs `Content-Range` total, duplicated CL, mismatched interval, non-identity encoding ⇒ distrust everything range-related); non-206/200/416 ranged answers and transport failures degrade ranges to `Unknown` while preserving HEAD-side facts (with a warning log); effective URL recorded after redirects; ETag/Last-Modified recorded (ranged/GET response wins when present). `probe_by_ranged_get()` serves HEAD-unsupported or size-silent resources. 37 inline tests |
| `src/main.rs` | `mod download;` wired; `#[cfg(test)] #[path = "../tests/common/mod.rs"] pub(crate) mod common;` declares the shared test support inside the binary crate (ADR-0014) |
| `src/logging.rs` | `redact_url` `dead_code` waiver removed: the Phase 3 modules call it on every logged/response URL (per the narrow-waiver rule, waivers retire when items become load-bearing) |
| `src/error.rs` | `AppError::with_source` `dead_code` waiver removed: `HttpClient` builds and `http_error()` are genuine callers now |
| `tests/common/server.rs` | Phase 3 knobs added to `Behavior` (all default `None`, so Phase 2 routes are behaviorally unchanged): `head_status` (HEAD-only answer override, e.g. 405/501), `head_content_length` (`HeadLength::Value(n)` via a manually set header - hyper transmits it verbatim, including `0`, which an exact zero size hint would omit - or `HeadLength::Omit`; an unparseable text cannot be simulated because hyper refuses to transmit it, documented on the type), `get_status` (GET-only sentinel, HEAD keeps succeeding), `content_encoding` (attaches `Content-Encoding` to HEAD/200/206 answers). `RecordedRequest` gains a lowercased `headers` capture (duplicate names comma-joined); the generic redirect responses now also increment route hit counts so tests can assert probe hops; `host_header()` helper exposes the HTTP/1.1-mandated `Host` value |
| `tests/common/mod.rs` | Doc comment updated: the module is also the binary unit tests' mock-server source (ADR-0014); still excluded from non-test builds |
| `tests/mock_server.rs` | The arrival-order test's expected literals carry the new `headers` field: the HTTP/1.1-mandated `host` entry (value from `MockServer::host_header()`, since the shot includes the ephemeral port) plus the raw `range` header; all pre-existing assertions (order, method, path, range, hit counts) unchanged |

### Behavior contract (Phase 3)

- `request shape`: at most two requests per probe (HEAD, then exactly one
  ranged GET `bytes=0-0` when the HEAD passes); every request goes through
  the single configured client with `Accept-Encoding: identity`; response
  bodies are never read.
- `facts`: size and range support are recorded only from response headers,
  with conflicts resolved by distrust (contradiction ⇒ `Unknown`/`None`,
  never a guess); absent or invalid `Content-Length`, `bytes 0-0/*`,
  416 `bytes */N`, and a lying HEAD are all handled without ever trusting a
  contradictory value.
- `redirects`: followed up to the limit; the effective URL is recorded;
  loops and over-limit chains fail with a typed HTTP error.
- `errors`: connection failures, timeouts, and non-2xx primary answers are
  `ErrorCategory::Http` (exit 1) with redacted URLs in every message;
  scheme-invalid and unparseable URLs are `ErrorCategory::Input`
  (exit 2) before any network traffic.

### Defects caught before commit (recorded honestly)

1. The secondary-degrade path originally carried a leftover
   `let _ = http_error(err);` that referenced an unimported symbol - the
   branch already receives a typed `AppError`, whose `Display` is log-safe;
   the line was removed before the first successful build.
2. First mapping folded every non-206/200/416 ranged answer into
   `Contradicted`, silently erasing good HEAD facts on a 5xx/404 blip.
   Added `RangeAnswer::Other`: HEAD-side size/validators survive, ranges
   go to `Unknown` (locked by the 500/404 degradation test).
3. `const fn` setters cannot assign `Option<HeadLength>` (a `String`
   lives inside `Raw` at the time); the compiler caught it; the setters
   dropped `const` and `HeadLength::Raw` was removed entirely once hyper
   proved it cannot transmit unparseable `Content-Length` values.
4. The `cl_map` test helper used `HeaderMap::insert`, which *replaces*
   same-name values, so the conflicting-duplicate parser case passed
   vacuously; switched to `append`, showing the strict multi-header rule
   actually firing.
5. Empirical discoveries (temporary scratch experiment, deleted after the
   observation): (a) hyper omits `Content-Length` entirely for HEAD
   answers whose exact size hint is 0, and (b) hyper aborts the
   connection rather than sending a manually set non-numeric
   `Content-Length`. Consequences: the empty-resource coverage became two
   tests - a manual `Content-Length: 0` HEAD (short-circuit, one request)
   and the natural no-CL path whose `bytes 0-0` range request answers 416
   `bytes */0`; and the probe's 416 arm gained its own
   `parse_unsatisfiable_total` (the 206 parser intentionally rejects the
   `bytes */N` form, which had silently dropped the total).
6. Redirect responses did not increment route hit counts (the counter
   sits after the redirect short-circuit), so the redirect-follow test
   could not assert the probe's hops; redirect answers now count too.

### Verification - actual results (all run on Windows)

| Check | Result |
|---|---|
| `cargo fmt --check` | pass (after applying rustfmt) |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings (one needless-borrow in new test support fixed) |
| `cargo test` | 124 passed, 0 failed: 74 unit (29 pre-existing + 2 http + 37 probe + 6 fixture-inline, the last obtained via the `#[path]` include and also counted in the fixtures target) + 11 cli + 15 fixtures + 24 mock_server |
| Fresh-clone gate reproduction | see the post-commit addendum at the bottom of this file |

### Scope-control and honesty notes

- No command wiring: `download`/`status`/`resume`/`retry` still print the
  not-implemented line and exit `1`; the CLI integration suite (11 tests)
  is byte-for-byte behaviorally unchanged.
- no lib.rs: the unit tests reuse `tests/common/` through the `cfg(test)`
  include (ADR-0014). Consequence (disclosed, not hidden): the fixture
  module's six inline tests run in two test targets, and the totals above
  double-count them.
- `dead_code` waivers: file-level in `src/download/http.rs` and
  `src/download/probe.rs` only, each citing the phase that wires them;
  two other waivers retired this phase (`logging::redact_url`,
  `error::with_source`) because they became load-bearing.
- HTTPS verify-limitation (disclosed): the TLS success path is not
  exercised by a local test - no live external URLs are allowed, and a
  local TLS endpoint would add non-stack dev crates (cert generation +
  tokio-rustls) for this phase only. What is verifiable offline is the
  scheme-level URL validation (`https` accepted without network, locked by
  a unit test) and identical code paths for both schemes; the actual
  SChannel handshake on Windows remains unverified by Phase 3 tests.
- Windows honesty: every check in the table ran on Windows (MSVC,
  rustc 1.99.1); nothing in this phase was verified on another platform,
  and nothing was presented as Windows proof that wasn't.

### Phase 3 verification addendum (post-commit, appended 2026-10-18)

- **Commit:** `f231176` - "Phase 3: HTTP client and resource probing
  (v0.5.0)" (code + tests + tracking files, 13 files changed,
  3058 insertions(+), 70 deletions(-)). The phase entry above is part of
  that commit, so the hash is recorded here and in `git log`, not inside
  the entry itself (same honesty rule as the Phase 1/2 entries).
- **Fresh-clone gate reproduction** (ADR-0008 recipe: `git clone` of
  `f231176` to a fresh temp directory on Windows, working tree verified
  clean, recipe run in order):

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings |
| `cargo test` | 124 passed (74 unit + 11 cli + 15 fixtures + 24 mock_server), 0 failed |

## Phase 4 - Sequential Transfer Engine

- **Date:** 2026-10-18
- **Version:** `0.5.0` → `0.6.0`
- **Outcome:** Phase 4 exit gate met (working tree). One-URL sequential
  downloader implemented and network-tested against the Phase 2 mock
  server: probe resolves the effective URL, the file name derives from the
  effective URL and is sanitized for Windows, only an HTTP 200 response may
  write bytes (error bodies never become file content), body chunks stream
  through bounded/idle-aware reads into an engine-owned partial file, a
  size claim when one exists is validated before finalization, and the
  final name is claimed without ever replacing an existing file
  (reserve-by-`create_new`, then same-volume rename). Failed transfers keep
  their `.part` file and never produce a final name. Every CLI command
  still dispatches an honest not-implemented error (wiring is Phase 11).
  No segmented transfers, no retry policy, no SQLite, no ZIP work (their
  own phases).

### Dependencies added (approved stack, first use)

Two product dependencies by ADR-0003 first-use discipline:

- `tokio 1` with `default-features = false`,
  `features = ["fs", "io-util", "time"]` - the engine is async production
  code: partial-file streaming (`fs`, `io-util`) and engine-side bounded
  timeouts (`time`, ADR-0015). Runtime/bootstrap features (rt, macros,
  signal...) stay in `[dev-dependencies]` for tests and remain excluded
  from the product build.
- `bytes 1` - the body-streaming type of reqwest's public API
  (`Response::chunk` yields `bytes::Bytes`), which `HttpClient::read_chunk`
  returns. reqwest already pulled the crate, so adding the direct entry
  changed **zero** package resolutions: `git diff --stat Cargo.lock` shows
  `1 file changed, 1 insertion(+), 1 deletion(-)` - exactly the
  `swift-fetch` version line to `0.6.0`.

Dev-dependencies unchanged from Phase 2/3.

### Code changes

| File | Change |
|---|---|
| `Cargo.toml` | version `0.6.0`; `[dependencies]` gains `tokio` and `bytes` with first-use phase comments (ADR-0003) |
| `Cargo.lock` | version-line-only diff (see above); committed per the standing rule |
| `src/main.rs` | `mod storage;` wired (the Phase 0 placeholder module becomes load-bearing) |
| `src/download/mod.rs` | declares `progress` and `sequential`; module doc states the transfer law and fixes the ADR-0014 path reference in prose to `../tests/common/mod.rs` (text only; the `#[path]` attribute lives in `main.rs` and was always correct) |
| `src/download/progress.rs` | NEW. Wait-free byte counters of one transfer (`bytes_received`, `bytes_written`, `largest_single_write` atomics), `Progress::shared() -> Arc` so tests/later phases observe during *and* after failures (the caller owning the handle is deliberate: after a failed `download_sequential` no counters would otherwise be observable). The CLI-facing display remains later-phase work; waiver cites Phase 11. One inline test |
| `src/download/http.rs` | ADR-0015. `HttpTimeouts { connect, metadata, body_idle }` (defaults 15 s/30 s/30 s, still locked by an updated unit test); the client-level `.timeout()` is removed entirely; `send()` wraps the request future in `tokio::time::timeout(metadata)` producing the same locked user message `the remote server did not answer in time: {redacted}`; NEW `read_chunk()` wraps each `response.chunk()` in `tokio::time::timeout(body_idle)` producing `the remote server stopped sending data during the transfer: {redacted}`; returns `bytes::Bytes`. Two new network tests: a body streaming across 8 delayed frames (~1.6 s) completes under a 250 ms metadata budget (header phase measured < the budget, body reads ≥ 800 ms), and a stalled second read aborts at the 250 ms idle budget with the recorded message |
| `src/download/probe.rs` | Visibility bumps so the engine reuses ONE strict header-parsing/trust policy for its own transfer response (no second parser to drift): `response_meta()`, `ResponseMeta` (+ fields), `ContentEncoding`, `ContentLength` become `pub(crate)`; the `quick_client` test helper's `HttpTimeouts` literal covers the restructured fields |
| `src/download/sequential.rs` | NEW. `download_sequential(client, url, output_dir, &Progress)` → `SequentialDownload`. Flow: `probe` the input URL (Phase 3 facts: size, validators, effective URL) → file name from the effective URL (`select_filename`: last non-empty path segment, manual per-segment percent-decode, Windows sanitization - invalid chars/control codes to `_`, trailing dots/spaces stripped, reserved device stems neutralized with a `_` prefix, 255 UTF-16-unit cap preserving the extension, constant `download` fallback for unusable results) → full GET through the shared client → require exactly `200` (anything else is `Http` with the probe-shaped message, before any file exists) → reuse `response_meta`: `Content-Encoding` transform refused as `RangeValidation` (nothing written, nothing created) → `expected_size_of`: the transfer's own `Content-Length` wins, the probe's recorded fact fills its absence, a contradiction between the two is refused (`RangeValidation`), no claim at all means the server-defined stream end is the completion criterion (no length invented) → only then create output dir + partial file → stream loop: bounded `read_chunk` + chunked `write_chunk`, counting received and written separately; mid-transfer failures are wrapped with the byte accounting (`stopped: {written} of {expected} expected bytes were written to the partial file; ...`) without hiding the cause or breaking log-safety → clean-end count validation (mismatch = `RangeValidation`, partial preserved) → `reserve_destination` + `finalize` (ADR-0016). The outcome record carries destination path, file name, bytes written, expected size, validators (final GET response wins over probe facts), effective URL. 25 inline tests: 9 pure filename tests, 2 pure size-decision tests, 14 network tests (byte-exact small + 4 MiB streamed transfer with counter checks, length-less stream-end completion, probe-fallback validation, truncation never finalizes with disk/counter agreement, 500 status never writes the sentinel body and leaves the directory untouched and uncounted, transformation refusal before any file, no-overwrite collision policy end-to-end, deterministic `(1)` siblings, missing output dir created, directory-occupied-by-file = `Filesystem`, GET-won validators, changing validators across downloads, redirect-effective-URL naming) |
| `src/storage/file_writer.rs` | NEW. Storage-layer file law: `ensure_output_dir` (creates nested trees; a path held by a regular file is `ErrorCategory::Filesystem` naming the path); `PartialFile::create/write_chunk/sync` (`<name>.swiftfetch-<pid>-<seq>.part` - pid + per-process counter so concurrent processes and future intra-process transfers never share a name; single-component assert; tokio chunked writes); `reserve_destination` (exclusive `create_new` claim, `stem (n).ext` collision numbering from 1, 255 UTF-16-unit candidates via `with_suffix` stem re-truncation); `finalize` (sync, close handle, `std::fs::rename` over the own empty reservation; rename failure preserves the partial and best-effort removes the own 0-byte reservation - the microsecond race is documented and can only shuffle suffix numbering, never destroy content); exports `cap_units` + `MAX_COMPONENT_UNITS` (`pub(crate)`) reused by the engine's sanitizer. 6 inline tests |
| `src/storage/mod.rs` | the Phase 0 doc-comment placeholder (ADR-0004) is replaced by the real module declaration plus storage-layer docs |

### Behavior contract (Phase 4)

- `request shape`: probe (≤ 2 requests) + one full GET = ≤ 3 requests per
  download, all through the single configured client; the full GET never
  carries a `Range` header.
- `content law`: HTTP 200 is the only response shape whose body may become
  file bytes; error statuses (and their bodies) leave the output directory
  untouched and the counters at zero; a `Content-Encoding` transform is
  refused before any file exists.
- `size law`: with a claim (transfer `Content-Length`, else the probe's
  recorded fact) the written byte count must equal it before
  finalization; a contradiction between transfer and probe facts is an
  error, never silently resolved; with no claim at all, stream end is the
  completion criterion and `expected_size` records `None` honestly.
- `failure law`: incomplete data never gains a final name; failures keep
  the partial file; mid-transfer errors report how many bytes were written
  vs expected (or unbounded) without leaking raw URLs; received and
  written counters never drift from each other or from the partial file's
  length on disk.
- `destination law`: existing files are never replaced - deterministic
  reservation `(n)` numbering on collision; the file name comes from the
  effective URL (redirects included), percent-decoded then sanitized for
  Windows (reserved device names, illegal/control characters, trailing
  dots/spaces, 255-UTF-16-unit component cap with the extension preserved).
- `time law` (ADR-0015): connect-through-headers is bounded
  (15 s connect / 30 s metadata defaults); each body read is bounded by the
  30 s idle default; total transfer duration is unbounded by design.
- `progress`: received/written/largest-single-write counters are exact and
  observable by the caller during and after the transfer.

### Defects caught before commit (recorded honestly)

1. **reqwest hands a small body over in ONE poll.** The Phase 2 mock
   streams in 512-byte frames, but `Response::chunk()` empirically
   returned the whole 4096-byte length-delimited body as a single chunk
   (verified with a temporary disposable test, `tests/scratch_sizes.rs`,
   output `[4096] total=4096`, file deleted after inspection). The
   first draft asserted `largest_single_write < total` for the small
   transfer and failed. The streaming-vs-buffering property is now proven
   where it is meaningful: the 4 MiB transfer (a single poll cannot carry
   a whole body through ~64 KiB-scale socket buffers), while the small
   test locks counters and byte-exactness only. reqwest's poll size is its
   own transport decision; the engine writes every poll immediately.
2. **The mock's per-frame delay arms the sleep *after* handing out a
   frame.** First draft assumed every frame slept before arriving and had
   `an_idle_body_read_is_aborted_at_the_idle_budget` read the first chunk
   expecting a stall: empirically frame 1 arrives free (~instantly) and
   only the *next* poll waits the armed delay. The test now reads the free
   first frame, then bounds the stalled *second* read (250 ms budget vs
   900 ms armed delay) and locks the mapped message. Same semantics make
   the streaming test's timing asserts phase-local with generous margins
   (headers < budget; body reads ≥ 800 ms across 7+ armed sleeps) instead
   of one brittle absolute end-to-end bound.
3. **A truncated body poisons the pooled probe connection.** With
   `truncate_after`, the probe's ranged request ends aborted (probe itself
   ignores bodies, so probing succeeds); the subsequent full GET reuses
   the dead pooled connection and the transport failure surfaces at the
   *request* stage ("the request to the remote server failed: URL"),
   before any byte or file exists. First draft asserted the mid-stream
   accounting unconditionally and failed. The truncation test now accepts
   both honest shapes: (a) failure before any body read - counters at 0,
   no partial file, nothing finalized; (b) failure mid-stream - partial
   file's length == written counter == received counter, ≤ 2 000, and the
   error states the durable byte count. Both are bounded and
   finalization-free.
4. **Two test-side mistakes (mine, not the engine's):** the
   destination-policy test seeded `fixture (1).txt` while asserting on the
   extension-less `fixture (1)` (the collision file must exist for the
   numbering to continue - fixed by seeding exactly what is asserted), and
   the hidden-source test never created the occupied file it relied on
   (`create_dir_all` on a *non-existent* path happily created a directory
   - fixed by seeding the file first).
5. **`char::encode_utf16()` takes a destination buffer** (unlike
   `str::encode_utf16()`, which is count-only): the per-character unit
   size in `cap_units` is `char::len_utf16()`. Caught by `cargo check`
   before any run, then reshaped by clippy (`to_digit(..).is_some()` →
   `is_ascii_hexdigit()`) with zero behavioral change.

### Verification - actual results (all run on Windows)

Working tree, in the ADR-0008 order:

| Check | Result |
|---|---|
| `cargo fmt --check` | pass (after `cargo fmt` applied rustfmt's own reflows; re-run clean) |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings (two lint fixes along the way, see defect #5) |
| `cargo test` | **158 passed, 0 failed** = 108 unit (74 Phase 3 baseline + sequential 25 + storage/file_writer 6 + progress 1 + http 2) + 11 cli + 15 fixtures + 24 mock_server |

Observed totals (per-module, `--list`): app 3, cli-in-binary 9,
common::fixture 6, config 7, download::http 4, download::probe 37,
download::progress 1, download::sequential 25, error 5, logging 5,
storage::file_writer 6 - together with the three integration targets
(11 cli, 15 fixtures, 24 mock_server).

- The CLI behavior contract is untouched and still locked by its Phase 1
  tests (commands print the not-implemented line, exit 1, empty stdout;
  parse/config rejects are exit 2; logging goes to stderr only).
- The ADR-0014 double-count disclosure still applies: the six common
  fixture inline tests run in multiple targets, so 158 double-counts them.
- Windows honesty: every check in the table ran on Windows (MSVC,
  rustc 1.99.1); nothing was verified on another platform, and nothing is
  presented as Windows proof that wasn't.
- Disclosed limitations carried/honest for this phase: HTTPS handshake is
  still not exercised by a local test (unchanged Phase 3 limitation); a
  clean-EOF-vs-`Content-Length` mismatch is not reproducible end-to-end
  against hyper (it aborts such bodies) - the length-decision logic is
  covered by the pure `expected_size_of` tests and the probe/GET
  contradiction shapes; kill-mid-write recovery (crash between file write
  and DB commit) belongs to the persistence phase, not Phase 4 (no
  storage engine state on disk exists beyond the partial file by design).
- `dead_code` waivers: file-level in `src/download/http.rs`,
  `src/download/probe.rs`, `src/download/sequential.rs`,
  `src/download/progress.rs`, and `src/storage/file_writer.rs`, each
  citing the phase that wires them (Phase 11); nothing else was suppressed.

### Scope-control notes

- No SQLite, no archive work, no concurrent/segmented transfers, no
  auto-resume policy, no GUI, no telemetry - all later phases per
  `DOCS/phases.md`.
- The engine is *not* wired to commands: a caller-less public surface
  stays exactly as honest as the not-implemented dispatch, and the wiring
  remains Phase 11's deliverable.
- `expected_size_of` treats a probe record as trustworthy input because
  the Phase 3 prober only records response-backed facts; it does not
  re-validate probe honesty (that is the prober's tested contract).
- Timeout defaults remain provisional numbers (only the 15 s connect
  timeout is architecture-fixed); changing them is a `HttpTimeouts`
  default + one locked test, not an API change.

### Phase 4 verification addendum (post-commit, appended 2026-10-18)

- **Commit:** `ec03cbe` - "Phase 4: sequential transfer engine (v0.6.0)"
  (code + tests + tracking files, 12 files changed, 2187 insertions(+),
  48 deletions(-)). The phase entry above is part of that commit, so the
  hash is recorded here and in `git log`, not inside the entry itself
  (same honesty rule as the Phase 1/2/3 entries).
- **Fresh-clone gate reproduction** (ADR-0008 recipe: `git clone` of
  `ec03cbe` to a fresh temp directory on Windows - the clone was removed
  after the run; working tree verified clean before the recipe - run in
  order):

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings (fresh uncached `target/`, 23.4 s check) |
| `cargo test` | 158 passed, 0 failed (108 unit + 11 cli + 15 fixtures + 24 mock_server), identical to the working-tree run |

## Phase 5 - SQLite Persistence and Lifecycle State

- **Date:** 2026-10-18
- **Version:** `0.6.0` → `0.7.0`
- **Outcome:** Phase 5 exit gate met (working tree). Queue and recovery
  metadata now persist in SQLite per the architecture's minimal model: a
  versioned schema with an atomic migration protocol (fresh databases are
  built in one transaction; failed initialization leaves generation 0 and
  re-runs cleanly; a database from a newer SwiftFetch is refused, never
  downgraded), a repository API for inserting/reading/updating/listing
  download, segment, and extraction records, and the ten-state download
  lifecycle with a fully tested transition graph. Downloads, segments, and
  extractions are metadata only - the database structurally cannot store
  file contents (no BLOB columns, test-locked). Every CLI command still
  dispatches an honest not-implemented error (wiring is Phase 11; the
  repository earns its production callers with Phase 6/7). No retry policy,
  no segmented transfers, no extraction work (their own phases).

### Dependencies added (approved stack, first use)

`rusqlite 0.40.2` with `features = ["bundled"]` - the first use of the
stack's SQLite component (ADR-0003 discipline, ADR-0017 record). `bundled`
compiles the SQLite engine itself via `libsqlite3-sys 0.38.2`, so no system
SQLite library is required and Windows builds stay reproducible. `git diff
--stat Cargo.lock`: `93 insertions(+), 5 deletions(-)` - the lock gains the
crates the SQLite integration resolves (rusqlite, libsqlite3-sys, hashlink,
hashbrown, foldhash, fallible-iterator, fallible-streaming-iterator,
sqlite-wasm-rs, rsqlite-vfs) plus the `swift-fetch` version line change.
Dev-dependencies unchanged from Phase 2/3.

### Code changes

| File | Change |
|---|---|
| `Cargo.toml` | version `0.7.0`; `[dependencies]` gains `rusqlite = { version = "0.40", features = ["bundled"] }` with the first-use comment (ADR-0003) |
| `Cargo.lock` | committed, diff above; `rusqlite 0.40.2` + `libsqlite3-sys 0.38.2` resolved |
| `src/storage/schema.rs` | NEW. The versioned schema and its migration protocol (ADR-0017): the ordered `MIGRATIONS` list (generation 1 = the architecture's four minimal tables `downloads`/`segments`/`extractions`/`schema_version`; child tables reference `downloads(id)` with `ON DELETE CASCADE`); `migrate_connection` applies every pending migration each inside its own immediate transaction together with the `schema_version` write, so a fresh database that fails initialization stays completely empty and re-runs cleanly, a partially applied generation can never exist, and a database claiming a newer generation than the build supports is refused with an actionable `Database` error; `version_of` reads the recorded generation (no table = generation 0); fixed explicit column projections (`DOWNLOAD_COLUMNS`/`SEGMENT_COLUMNS`/`EXTRACTION_COLUMNS`) shared with the repository's positional row mappers; shared `db_failure` mapping (operation text + hidden source, always `ErrorCategory::Database`). 8 inline tests: migration-list well-formedness, fresh-database init to the current version, idempotent reopen (records survive), future-version refusal, failed-migration full rollback + clean re-run, failed fresh initialization stays empty, no-BLOB-columns across all tables, typed rejection visibility through rusqlite's conversion-failure wrapper |
| `src/storage/repository.rs` | NEW. The repository (ADR-0017/0018): `DownloadState` (the ten architecture §7 states with `as_str`/`parse` and the ADR-0018 `allows` graph), `SegmentState`/`ExtractionState` (persistence round-trip now, transition graphs in phases 8/10), record structs (`DownloadRecord`/`SegmentRecord`/`ExtractionRecord` with `PartialEq` for test equality) and input specs (`NewDownload` always inserts `queued` - nothing truthful has happened yet; `TransferFacts` merge semantics: `None` fields keep their stored value); `default_database_path`/`database_path_in` (pure, testable resolution to `%LOCALAPPDATA%\SwiftFetch\swiftfetch.sqlite3`; missing/empty = typed `Filesystem` error, no fallback guessing); `Repository::open(path)` (parent-dir creation as `Filesystem` errors, connection pragmas: WAL + foreign_keys ON + 5 s busy timeout, then migrations); the complete CRUD surface - insert/get/list (all + per-status)/`update_status` (validated inside one immediate transaction)/`update_transfer_facts` (COALESCE merge)/`set_retry_count`/segment insert/update/list/extraction insert/update/get/list; value guards (u64→i64 range for byte counts/counts/timestamps, inverted-range and over-claimed-progress rejections) and read guards (negative numbers, unknown status/category/state text → typed `Database` failures that never silently default); `WriteFailure` enum so domain rejections roll their transaction back while rusqlite errors map through `db_failure`. 18 inline tests covering every phase 5 test requirement: persistence between program runs (+ unknown id reads `None`), full field round-trip, the ten-edge happy walk through the graph, the complete 100-pair truth table, atomic rejection of invalid/self/leap transitions (row untouched), failed-category requirement + no-categories-elsewhere, terminality of `failed`/`cancelled`, listing order + status filter, corrupt persisted names and negative values as typed errors, commit-before-return visibility from a second independent connection (the "no long-lived transaction across other work" proof), caller-supplied timestamps, segment round-trip/progress guards, FK enforcement for orphan children, extraction state/details tracking incl. failed-requires-details and clearing, all-update-methods not-found rejections, default-path derivation (pure + platform smoke), and the blocked-directory `Filesystem` error |
| `src/storage/mod.rs` | declares `repository` and `schema`; module docs state the storage-layer laws (metadata-only database, local app-data placement, ADR references) |

### Behavior contract (Phase 5)

- `versioning law`: the schema generation is recorded in `schema_version`;
  migrations run in order, each atomic with its version write; a newer
  database file refuses to open; existing generations are never rewritten
  (new migrations append to `MIGRATIONS`).
- `state law` (ADR-0018): a download record moves only along the tested
  graph; `update_status` validates existence, no-op, and edge inside the
  same immediate transaction that writes, so an invalid request writes
  nothing; `failed` requires an error category and other states reject one;
  extraction lives in its own table (`completed ⇄ extracting` round trip on
  the download, failures never downgrade the downloaded file's status).
- `transaction law`: every repository operation commits before returning;
  no public API holds a transaction across a callback, network transfer, or
  file write; the "writes are visible to an independent connection the
  moment the method returns" test is the observable proof.
- `metadata-only law`: no BLOB-typed columns exist in any table
  (test-enumerated); bytes never become database content.
- `honesty law`: unknown ids read as `None` on get and fail as typed
  not-found errors on update; unreadable persisted data (foreign status
  text, unknown categories, negative numbers) fails with typed `Database`
  errors naming the corruption, never silently masked.
- `path law`: the database lives under the local application-data directory
  (`%LOCALAPPDATA%\SwiftFetch\swiftfetch.sqlite3`), separate from any
  download directory; missing/empty `LOCALAPPDATA` and inaccessible
  directories are typed failures with actionable messages.

### Defects caught before commit (recorded honestly)

1. **rusqlite 0.40 API drift vs older samples:** `Connection::table_exists`
   takes a schema-name parameter first (`table_exists(Option<N>, N)`) - the
   one-arg form existed in older docs. Caught by `cargo build --tests`
   (E0061 + E0600) and fixed across all five call sites.
2. **Domain rejections inside transaction bodies needed their own carrier:**
   the first draft threaded rusqlite's `Result` through validation paths,
   which would either lose the typed message or misuse conversion-failure
   constructors for business logic. Resolved with the two-variant
   `WriteFailure` enum (`Sql(rusqlite::Error)` / `Rejected(AppError)`) plus
   `From` conversions, so domain rejections roll the transaction back with
   their typed message intact and SQL errors map through `db_failure`.
3. **Compiler-caught category/type mismatches in the same iteration** (e.g.
   `rejection()` used at function scope where an `AppError` was required in
   `update_extraction`): fixed to plain `AppError::new` at scope, keeping
   `rejection()`/`not_found` carriers only inside transaction closures.
4. Mechanical `cargo fmt` reflows after the first successful compile; no
   semantic change.

### Verification - actual results (all run on Windows)

Working tree, in the ADR-0008 order:

| Check | Result |
|---|---|
| `cargo fmt --check` | pass (after one `cargo fmt` round; re-run clean) |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings (three lint fixes: `match_like_matches_macro` on the transition graph → `matches!`, two `explicit_auto_deref` removals on the lock guards) |
| `cargo test` | **184 passed, 0 failed** = 134 unit (108 Phase 4 baseline + storage/repository 18 + storage/schema 8) + 11 cli + 15 fixtures + 24 mock_server |

Observed totals (per-module, `--list`): app 3, cli-in-binary 9,
common::fixture 6, config 7, download::http 4, download::probe 37,
download::progress 1, download::sequential 25, error 5, logging 5,
storage::file_writer 6, storage::repository 18, storage::schema 8 - together
with the three integration targets (11 cli, 15 fixtures, 24 mock_server).

- The CLI behavior contract is untouched and still locked by its Phase 1
  tests (commands print the not-implemented line to stderr, exit 1, empty
  stdout; parse/config rejects are exit 2; logging goes to stderr only).
- The ADR-0014 double-count disclosure still applies: the six common
  fixture inline tests run in multiple targets, so 184 (and 134) count them
  multiple times.
- Windows honesty: every check in the table ran on Windows (MSVC, rustc
  1.99.1); nothing was verified on another platform, and nothing is
  presented as Windows proof that wasn't.
- `dead_code` waivers: file-level in the two NEW modules
  (`src/storage/schema.rs` and `src/storage/repository.rs`), each citing
  the phases that wire them (Phase 6/7 onward, commands in Phase 11);
  nothing else was suppressed.
- Disclosed limitations, honest for this phase: no cross-process
  simultaneous-writer test exists (multi-process contention is bounded by
  `busy_timeout` but unexercised); the repository has no production caller
  yet (the not-implemented commands remain the CLI surface, per plan);
  crash-between-filesystem-write-and-DB-commit recovery semantics arrive
  with Phase 6's recovery protocol (Phase 5 ensures the DB side of that
  boundary is transactional and visible);
  segment/extraction lifecycle validation is deferred to their phases (8,
  10) by design.

### Scope-control notes

- No retry policy, no resume, no segment scheduling, no extraction work, no
  GUI, no telemetry - later phases per `DOCS/phases.md`.
- The repository is not wired to commands or the engine: like Phase 4's
  engine, it is a tested layer whose callers arrive in Phase 6 (transfer
  records + recovery), Phase 7 (queue), and Phase 11 (status/retry
  commands).
- The `failed → queued` edge (manual retry requeue) is deliberately absent;
  Phase 11's retry command adds it with its own tests under ADR-0018.
- New schema migrations are contract-risky: they must append to
  `MIGRATIONS` (never rewrite) with the phase's tests, per ADR-0017.

### Phase 5 verification addendum (post-commit, appended 2026-10-18)

- **Commit:** `b5da4d2` - "Phase 5: SQLite persistence and lifecycle state
  (v0.7.0)" (code + tests + tracking files, 7 files changed, 2657
  insertions(+), 8 deletions(-)). The phase entry above is part of that
  commit, so the hash is recorded here and in `git log`, not inside the
  entry itself (same honesty rule as the Phase 1-4 entries).
- **Fresh-clone gate reproduction** (ADR-0008 recipe: `git clone` of
  `b5da4d2` to a fresh temp directory on Windows - the clone was removed
  after the run; working tree verified clean before the recipe - run in
  order):

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass, 0 warnings (fresh uncached `target/`, 22.2 s check) |
| `cargo test` | 184 passed, 0 failed (134 unit + 11 cli + 15 fixtures + 24 mock_server), identical to the working-tree run |





