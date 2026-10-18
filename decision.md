# Architecture Decision Record (`decision.md`)

Architecture Decision Records (ADRs) for `download-manager`. Each record
documents one key technical decision, why it was chosen over the
alternatives, and the specific tradeoffs accepted. Decisions operate *inside*
the approved source documents (`DOCS/PRD.md`, `DOCS/architecture.md`,
`DOCS/phases.md`); those documents remain the source of truth and are changed
only by the project owner.

## Conventions

- **Append-only.** New records take the next ID. A decision is never silently
  rewritten here; it is superseded by a later record that references its ID.
- **Status values:** Accepted / Proposed / Superseded (by ADR-xxxx).
- Every record states: **Context** (the forcing problem), **Decision**,
  **Alternatives** (options actually considered), and **Tradeoffs** (what was
  given up and its practical consequence).
- Technology choices mandated by `DOCS/architecture.md` §3 (Rust, Tokio,
  Reqwest, SQLite via a maintained crate, Clap, `tracing`/`tracing-subscriber`,
  `thiserror`, `serde` where needed, a maintained ZIP crate) are approved
  requirements, not project decisions; they appear here only when a concrete
  choice within that mandate is worth logging (e.g., *when* a dependency is
  first added, and with which features).

## Index

| ID | Title | Status |
|---|---|---|
| ADR-0001 | Single Rust package at the repository root | Accepted |
| ADR-0002 | Git initialized in the workspace; `Cargo.lock` committed, local only | Accepted |
| ADR-0003 | Zero runtime dependencies in Phase 0; stack crates deferred to first use | Accepted |
| ADR-0004 | Intended module directories as doc-comment placeholders, unwired | Accepted |
| ADR-0005 | Phase 0 entry point: banner + unit test in the binary crate, integration test via `CARGO_BIN_EXE`; no `lib.rs` yet | Accepted |
| ADR-0006 | Edition 2021 pinned; `rustfmt.toml` mirrors it | Accepted |
| ADR-0007 | Repo hygiene: `.gitignore`, `.gitattributes`, tracked `Cargo.lock` | Accepted |
| ADR-0008 | Canonical verification recipe: fixed check sequence + fresh-clone reproduction | Accepted |
| ADR-0009 | Phase 1 CLI contract: honest unwired commands, exit codes 0/1/2, tracing-to-stderr logging, URL redaction | Accepted |

---

## ADR-0001 — Single Rust package at the repository root

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** `DOCS/architecture.md` §5 *suggests* a source layout rooted in a
  `download-manager/` folder containing `docs/`, while the actual workspace
  root already contains `DOCS/` (uppercase, holding `PRD.md`, `phases.md`,
  `architecture.md`). Phase 0 requires one package for the "modular monolith"
  and a reproducible build from a clean checkout.
- **Decision:** Place the single cargo package at the workspace root
  (`Cargo.toml`, `src/`, `tests/`, `README.md`); keep the existing `DOCS/`
  folder untouched as the documentation location, referenced from `README.md`.
- **Alternatives considered:**
  1. Nested `download-manager/` subfolder — matches the illustration
     literally, but inserts an extra nesting level and a second "top" dir
     beside `DOCS/`; the file itself calls this layout "suggested", not
     required.
  2. Cargo workspace with several crates — rejected: the architecture mandates
     one modular monolith; extra crates at this stage are premature
     abstractions.
- **Tradeoffs:** Accept a slight layout deviation from the architecture
  illustration (package at root; docs in `DOCS/` instead of `docs/`). Chosen
  to preserve the owner's existing files and to keep `cargo build/test`
  runnable directly from the checkout root — the strongest reading of "builds
  from a clean checkout". If strict conformance is later demanded, moving the
  package one level down is a mechanical change. Confirmed with the project
  owner during Phase 0.
- **Related:** ADR-0004 (module placeholder strategy).

## ADR-0002 — Git initialized in the workspace; `Cargo.lock` committed, local only

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** `DOCS/phases.md` Phase 0 requires committing the dependency
  lockfile ("for reproducible application builds") and proving a build from a
  fresh checkout, but the working directory was not a git repository when
  Phase 0 started.
- **Decision:** Initialize git (`main` branch) in the workspace root and
  commit `Cargo.lock` together with the Phase 0 skeleton. No remote is
  configured; no global git configuration was changed (the repository uses the
  owner's pre-existing identity).
- **Alternatives considered:**
  1. No VCS, files only — would leave "commit the lockfile" and the
     clean-checkout criteria unverifiable.
  2. Git plus a published remote — not requested; out of scope for Phase 0.
- **Tradeoffs:** Committing the lockfile means deliberate lock churn on every
  dependency change — that churn *is* the reproducibility record, so it is
  accepted. No remote yet means clones exist only from this machine; hosting
  remains an open question for the owner. Lockfile format `version = 4` is
  written by default since Cargo 1.83, so contributors need a reasonably
  recent Cargo.
- **Related:** ADR-0003 (dep deferral keeps the lockfile trivial for now),
  ADR-0007 (reproducibility files), ADR-0008 (fresh-clone check).

## ADR-0003 — Zero runtime dependencies in Phase 0; stack crates deferred to first use

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** Phase 0 permits adding stack dependencies "only when they are
  first required, or declare the minimal initial set"; `DOCS/architecture.md`
  §3 mandates the stack (Tokio, Reqwest, Clap, SQLite, `tracing`,
  `thiserror`, ZIP, `serde` where needed) but also instructs keeping
  dependencies "limited to features the implementation actually uses". Phase 0
  implements no CLI parsing, networking, storage, or logging.
- **Decision:** Ship Phase 0 with an empty `[dependencies]` table. Each crate
  from the approved stack is added in the phase that first uses it, in the
  same commit as the tests exercising it.
- **Alternatives considered:**
  1. Declare clap/tracing/thiserror/tokio now as the "minimal initial set" —
     rejected: unused dependencies contradict the architecture's usage rule,
     bloat the lockfile with transitive crates whose needed features (locking
     style for SQLite, TLS features for Reqwest, etc.) have not been decided,
     and bury the real dependency additions that should be visible in the phase
     commits.
- **Tradeoffs:** The current `Cargo.lock` is trivial (a single package), so it
  does not yet demonstrate lock resolution of a real tree; the check becomes
  meaningful from Phase 1 (clap, tracing, thiserror) onward. Accepted
  deliberately — it is the strictest reading of "only when first required"
  and matches "smallest solution that satisfies the assigned phase".
- **Related:** ADR-0002 (lockfile commitment), ADR-0008 (verification recipe).

## ADR-0004 — Intended module directories as doc-comment placeholders, unwired

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** Phase 0 requires creating `src/main.rs` *and* the intended
  module directories, while forbidding "large empty implementations or
  speculative abstractions".
- **Decision:** Create the five module directories from `DOCS/architecture.md`
  §5 (`src/cli`, `src/app`, `src/download`, `src/storage`, `src/archive`),
  each containing a `mod.rs` whose only content is a doc comment naming the
  module and the phase that will fill it. They are not declared via `mod` in
  `main.rs` until the phase that implements them, so they cannot become dead
  code paths or fake public APIs.
- **Alternatives considered:**
  1. Skip the directories until their phases — violates the explicit Phase 0
     instruction.
  2. Declare the modules in `main.rs` immediately — rejected: pulls(empty)
     modules into the build graph for no value; slightly widens the phase-0
     surface beyond what is needed.
  3. Stub functions returning placeholders/errors — rejected: exactly the
     "large empty implementation"/speculative abstraction Phase 0 forbids, and
     it invents API surface without behavior or tests.
- **Tradeoffs:** Placeholder files are inert until declared (cargo ignores
  them), which is the honest state; the cost is a temporary mismatch between
  the file tree and the crate's declared module tree. The mismatch is visible
  in the `mod.rs` headers by design, not documented only here.

## ADR-0005 — Phase 0 entry point: banner + unit test in the binary crate, integration test via `CARGO_BIN_EXE`; no `lib.rs` yet

- **Status:** Superseded (banner contract and Phase 0 test set replaced by the
  Phase 1 CLI contract — see ADR-0009). The `CARGO_BIN_EXE_*` integration-test
  technique itself is retained and extended.
- **Date:** 2026-10-18
- **Context:** Phase 0 requires that the default executable "starts and exits
  predictably" plus "a basic unit or smoke test proving the application crate
  builds and its entry point is reachable." Usable CLI commands and exit codes
  belong to later phases (Phase 1, Phase 11), and premature abstractions are
  forbidden.
- **Decision:** `main()` prints a one-line banner
  (`download-manager 0.1.0 (Phase 0 foundation build)`) and exits `0`. The
  banner is assembled by a small function and asserted by a unit test inside
  the binary crate; an integration test (`tests/entry_point.rs`) spawns the
  built binary via `env!("CARGO_BIN_EXE_download-manager")` and asserts the
  success status plus the banner on stdout. No `lib.rs` is introduced in
  Phase 0.
- **Alternatives considered:**
  1. Add `src/lib.rs` now (thin `main` over a library) — rejected for Phase 0:
     nothing outside the binary needs to link it yet; if later phases require
     shared-test access to internals, the planned minimal evolution is a thin
     binary + `lib.rs` split, superseding this record at that point.
  2. Integration test asserts only the exit status — weaker; it stays green
     even if the banner silently changes, so the stdout content is asserted
     deliberately as the only Phase 0 behavior contract.
  3. CI-only `cargo run` — rejected: not verifiable from `cargo test`.
- **Tradeoffs:** The smoke test freezes the exact stdout text, so Phase 1's
  CLI contract work must update that test and supersede this record when the
  banner/status semantics change — pinned change by design, not accidental
  brittleness. Unit tests in the binary crate cannot be imported by future
  integration tests; mitigated when needed by the `lib.rs` split above.

## ADR-0006 — Edition 2021 pinned; `rustfmt.toml` mirrors it

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** Phase 0 establishes formatting/linting/test commands and needs
  reproducible builds; the approved documents specify "Rust" without an
  edition. The development toolchain is rustc 1.99.1 (Windows MSVC), which
  supports edition 2024.
- **Decision:** Edition 2021 for the package; `rustfmt.toml` pins
  `edition = "2021"` so rustfmt and cargo agree regardless of a contributor's
  default edition.
- **Alternatives considered:**
  1. Edition 2024 — raises the toolchain floor to ≥ 1.85 for all contributors
     with zero present benefit; nothing in the current code uses 2024-only
     features.
  2. No `rustfmt.toml` — rustfmt would fall back to its own default edition
     assumption rather than reading the build edition with certainty; an
     explicit pin avoids tool-dependent formatting.
- **Tradeoffs:** Accept that 2024-only language features cannot be used until
  an edition bump happens (trivially reversible decision if needed later).
  Minor cost: one more repo file that must stay in sync with `Cargo.toml`.
  bf: rebencing edition later will also rewrite `Cargo.lock`/target state; do
  it in a dedicated commit, not mixed with feature work.

## ADR-0007 — Repo hygiene: `.gitignore`, `.gitattributes`, tracked `Cargo.lock`

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** The Windows-first project must stay reproducible: build outputs
  out of history, line endings consistent across `git add`/checkout systems,
  and the dependency lockfile pinned in history (per Phase 0).
- **Decision:** `.gitignore` ignores `/target` and `*.tmp` only; `.gitattributes`
  sets `* text=auto`; `Cargo.lock` is explicitly tracked; no other per-file
  exceptions are added.
- **Alternatives considered:**
  1. Ignore `Cargo.lock` (library convention) — rejected: this project is an
     application and `DOCS/phases.md` mandates tracking it.
  2. Force `* text eol=lf` everywhere — imposes LF checkout mechanics on a
     Windows-first repo without a need; `text=auto` (LF in the repository,
     respecting local checkout configuration) is the lower-friction norm.
  3. No `.gitattributes` — leaves `autocrlf` conversion warnings and platform
     divergence in diffs; rejected because reproducibility is a Phase 0 gate.
- **Tradeoffs:** `text=auto` still depends on contributors' `autocrlf`
  settings at checkout — a known, historic ecosystem compromise; the `*.tmp`
  entry covers common editor scratch files and may be extended if a concrete
  tool adds noise (extend the file, record it here, never ignore silently).

## ADR-0008 — Canonical verification recipe: fixed check sequence + fresh-clone reproduction

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** Phase 0's exit gate is evidenced by a specific command sequence
  and by building from a fresh checkout; future phases will accrete suites
  (fixtures, property tests, Windows-specific checks, benchmarks). Without a
  single agreed recipe, each phase risks claiming "done" with a different,
  weaker set of commands.
- **Decision:** Fix the canonical verification recipe for the whole project as
  exactly: `cargo fmt --check` → `cargo clippy --all-targets --all-features
  -- -D warnings` → `cargo test` (in that order), plus a full build + check
  sequence run once per phase in a **fresh clone** of the repository (checked
  out to a temporary directory, then removed). Phase-specific test suites are
  added on top of, never instead of, this recipe.
- **Alternatives considered:**
  1. `cargo clean` + rebuild in the dev workspace — rejected as the
     reproduction step: it still shares toolchain/caching state with the
     working machine and proves less than a fresh clone (the lockfile, repo
     hygiene, and placeholder strategy are what a collaborator actually gets).
  2. Single-recipe-per-phase hamburger (phase-specific only, no fixed
     baseline) — rejected: baseline drift is exactly the failure mode where
     each phase's "green" means a different thing; the fixed baseline keeps
     regression comparisons meaningful across 14 phases.
- **Tradeoffs:** Doubling the build/test time per phase handoff (trivial at
  Phase 0's single-crate size; re-evaluate if it ever becomes material). The
  recipe must be mirrored in `README.md` and referenced from phase handoffs so
  the documented truth and the executed truth stay the same.

---

## ADR-0009 — Phase 1 CLI contract: honest unwired commands, exit codes 0/1/2, tracing-to-stderr logging, URL redaction rule

- **Status:** Accepted (supersedes the banner-specific parts of ADR-0005)
- **Date:** 2026-10-18
- **Context:** Phase 1 requires a Clap-based CLI (download input, URL-list
  input, status, resume, retry plus configuration options), central
  configuration types, typed errors with stable categories, `tracing`-
  based structured logging with concise default output, and early validation
  of obviously invalid inputs — while commands may not falsely report
  success and must be "explicitly marked unavailable during development."
- **Decision:**
  1. *CLI surface (clap 4 derive):* subcommands `download` (positional URLs,
     `--url-file`, `--output`, `--files`, `--segments`, `--retries`,
     `--no-extract`), `status`, `resume`, `retry`; global `--verbose`.
     Help text labels every command as unwired; developer intent never
     leaks into user help (long_about pinned).
  2. *Exit codes:* `0` success; `1` requested operation cannot proceed —
     including any Phase 1 "not implemented" dispatch; `2` invalid
     invocation/configuration (Clap parse failures map to 2 natively, as do
     all `ErrorCategory::Input` rejections).
  3. *Errors (thiserror):* full `ErrorCategory` taxonomy from architecture
     §12 with stable kebab-case ids; `AppError` renders only the
     user-facing message, keeping internal source chains out of default
     output. `retries = 0` is accepted (means no retries); concurrency
     options must be ≥ 1; output paths must be non-empty, NUL-free, valid
     Unicode.
  4. *Logging:* tracing-subscriber writes to **stderr** (stdout reserved for
     results/progress); default INFO, `--verbose` DEBUG. `redact_url` is the
     mandatory log-safety rule: query *and* fragment are dropped wholesale
     (tokens cannot be reliably classified), userinfo passwords are masked;
     bare usernames pass through.
  5. *Test plumbing:* integration tests keep running the real binary via
     `CARGO_BIN_EXE_*`; the Phase 0 banner smoke test was *replaced* — not
     weakened — because the banner contract was deliberately superseded
     (this phase defined the replacement in the same commit).
- **Alternatives considered:**
  1. Hide unwired subcommands from help until their phases — rejected: the
     phase instructs to define the command surface now; hiding would make
     help less truthful about the intended product.
  2. `env_logger` — rejected: architecture §3 mandates `tracing`.
  3. `assert_cmd` for integration tests — deferred: `std::process::Command`
     over `CARGO_BIN_EXE_*` covers the process-level contract without an
     extra dev-dependency; revisit if tests need stdin/env control.
  4. Full URL parsing now (e.g. `url` crate) — deferred: Phase 1 validation
     is intentionally scheme-level "obvious invalid input"; deep parsing
     belongs to the HTTP phases, so no extra dependency yet.
  5. Selective query-param redaction (allowlist/denylist of keys) —
     rejected: classification of unknown remote query parameters is exactly
     the guesswork that could leak tokens; wholesale removal is the only
     conservative rule.
  6. Omit unwired taxonomy/categories until used — rejected: Phase 1's own
     test list requires stable error categories now; instead, narrow
     `allow(dead_code)` waivers (each citing its future phase) bridge the
     gap, keeping `-D warnings` meaningful.
- **Tradeoffs:** Every command is defined but does nothing yet (honesty in
  help + stderr is the mitigations). Config defaults exist twice — as the
  authoritative defaults in `AppConfig` and as help text; a test locks the
  mapping so drift fails loudly. Exit-code table is provisional until
  Phase 11 documents it fully (0/1/2 already stable and tested). Clap parse
  errors and validation errors share exit code 2 by design.
- **Related:** ADR-0003 (deps onboarded at first use), ADR-0005
  (superseded banner), ADR-0008 (verification recipe).

---

## Tracking addendum — appended 2026-10-18 (owner-directed, append-only)

**Placement rule going forward (owner direction):** every future update to
this file is appended at the very bottom. New ADRs take the next ID;
corrections never edit historical records in place. Nothing above this
addendum will be rewritten, moved, or deleted, including records that predate
this rule.

### ADR-0010 — Append-only bottom placement for tracking files (owner-directed)

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** The owner directed that `changes.md` and this file be
  maintained with strictly appended updates — "add all new updates to the
  very bottom; do not delete or overwrite any previous content" — after
  discovering that a Phase 1 documentation edit had destroyed the Phase 0
  section heading in `changes.md` (full disclosure in that file's
  bottom addendum dated 2026-10-18).
- **Decision:** Tracking updates are strictly append-only at the bottom of
  each file. Superseded decisions are marked through *new* records that
  reference the superseded ID (and through bottom addenda), not through
  editing an old record's existing status line.
- **Alternatives considered:**
  1. Conventional in-place "Superseded" status flips on old ADR records —
     now rejected by owner policy: in-place edits are indistinguishable from
     accidents in history, and an in-place edit is exactly how the Phase 0
     heading was destroyed.
  2. Newest-first insertion (the original `changes.md` convention) —
     superseded by this policy; bottom-append is the only mode guaranteed
     never to touch existing bytes.
- **Tradeoffs:** The newest state of a file sits behind older content, so
  scanning to the bottom is required to read the current truth;
  superseded/changed details are no longer annotated inline (mitigation:
  disclosure addenda, like this one, record every such state change). The
  index table at the top is frozen as-written and cannot be extended in
  place — the "Addendum-index" below serves as its formal extension, and any
  inconsistency between frozen index entries and later records is resolved
  by disclosure, not by table edits.
- **Related:** ADR-0005 (its in-place status flip predates this rule and is
  disclosed below), ADR-0009 (banner supersession reference).

### Addendum-index (formal extension of the frozen index table above)

| ID | Title | Status |
|---|---|---|
| ADR-0010 | Append-only bottom placement for tracking files (owner-directed) | Accepted |

### Integrity disclosure — in-place mutations recorded before this rule

1. **ADR-0005 `Status:` line.** Changed in place, from the original
   "**Accepted**" to "**Superseded** ((banner contract and Phase 0 test set
   replaced by the Phase 1 CLI contract — see ADR-0009). The
   `CARGO_BIN_EXE_*` integration-test technique itself is retained and
   extended.)", during commit `9903795` (Phase 1). The change aligns with
   the record's rationale and ADR-0009, but it was an in-place edit, which
   the new rule forbids going forward. Note: the frozen index table above
   still lists ADR-0005 as "Accepted"; per the frozen-index tradeoff, that
   inconsistency is resolved by this disclosure (authoritative status: the
   record's status line read together with ADR-0009), not by editing the
   table.
2. **`changes.md` Phase 0 heading loss.** Disclosed in that file's bottom
   addendum of the same date; the Phase 0 record body survived fully.
3. **ADR-0006 contains two typos** ("bf: rebencing edition later …" — the
   intended text was "Tradeoffs: rebasing edition later also rewrites
   `Cargo.lock`/target state; do it in a dedicated commit, not mixed with
   feature work."). Left unedited permanently per the strict append-only
   rule; meaning recoverable from this disclosure.
