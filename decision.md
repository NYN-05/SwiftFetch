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

---

## Tracking addendum #2 — appended 2026-10-18 (SwiftFetch rename)

### ADR-0011 — Product identity "SwiftFetch" (owner-directed rename)

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** The owner directed "put name as SwiftFetch in all comments /
  documents everywhere" and, asked whether that included the technical
  identity, chose the *full* rename. The change lands between Phase 1 and
  Phase 2 — the cheapest possible moment: the CLI surface exists, the engine
  is absent, there are no users and no release to break.
- **Decision:**
  1. Display/product name: **SwiftFetch** — README title and intro, cargo
     `description`, clap `name` (help/version identity), about text.
  2. Crate/binary identifier: **swift-fetch** (Rust kebab-case convention) —
     package name, binary `swift-fetch.exe`, `CARGO_BIN_EXE_swift-fetch` in
     integration tests, and the `Cargo.lock` package entry.
  3. Version 0.2.0 → 0.3.0 (pre-1.0 minor bump: the binary name users invoke
     is user-visible).
  4. DOCS titles gain a "(SwiftFetch)" suffix; generic descriptive prose
     ("Windows-first CLI download manager") stays — it describes the
     product, it does not name it. The `DOCS/phases.md` citation of the
     upstream spec *filename* `windows-first-cli-download-manager-project.md`
     stays unchanged (citing a filename is not branding).
  5. The Phase 1 CLI contract surface is unchanged — commands, options,
     exit codes, and help honesty all stand; only the program name changes.
- **Alternatives considered:**
  1. Exact-case crate name `SwiftFetch` — rejected: lowercase kebab-case is
     the Rust ecosystem convention and avoids tooling surprises around
     case-sensitive identifiers on Windows and in CI.
  2. Text-only branding (identifiers stay `download-manager`) — rejected by
     the owner: incoherent identity, where docs say SwiftFetch while users
     type download-manager.exe.
  3. Rename without a version bump — rejected: 0.3.0 marks the observable
     identity change in the pre-1.0 sequence (Phase 1 was 0.2.0).
- **Tradeoffs:** Two coexisting forms — **SwiftFetch** (display) and
  **swift-fetch** (crate/binary/env var) — because Rust identifiers are
  conventionally lowercase-kebab and cannot carry the brand's casing; this
  record is the mapping. Historical `changes.md` records keep
  `download-manager` (append-only rule, ADR-0010) and are mapped through
  this ADR. `Cargo.lock` refreshed automatically on the first post-rename
  build and was committed with the rename per the standing rule.
- **Related:** ADR-0010 (append-only tracking rule), ADR-0001 (single crate
  at the repo root), ADR-0009 (Phase 1 CLI contract; surface unchanged).

### Addendum-index extension (ADR-0011 row; the frozen top index stays untouched)

| ID | Title | Status |
|---|---|---|
| ADR-0011 | Product identity "SwiftFetch" (owner-directed rename) | Accepted |

Commit record: rename commit `56e7501` — "rename: product SwiftFetch; crate
download-manager -> swift-fetch (v0.3.0)" (10 files changed, 37 insertions,
41 deletions, including the regenerated `Cargo.lock`).

---

## Tracking addendum #3 — appended 2026-10-18 (remote + first push, owner-directed)

### ADR-0012 — Public remote repository and first push (owner-directed)

- **Status:** Accepted (supersedes the *local-only* aspect of ADR-0002; the
  ADR-0002 text remains byte-for-byte as written per ADR-0010)
- **Date:** 2026-10-18
- **Context:** The owner instructed, in exact order: update `.gitignore`
  against unwanted files/build artifacts, stage everything with
  `git add -A`, commit once descriptively, and push to
  `https://github.com/NYN-05/SwiftFetch.git`. This ends the intentional
  local-only state adopted in ADR-0002 and also settles `AGENTS.md` tracking
  (previously deferred pending an owner decision).
- **Decision:**
  1. `.gitignore` keeps `/target` and `*.tmp` and gains preventive patterns
     (`*.bak`, `*.rs.bk`, `Thumbs.db`, `Desktop.ini`, `.DS_Store`,
     `.env`, `.vscode/`); no tracked file matches any pattern, so no history
     rewrite is involved.
  2. `AGENTS.md` becomes tracked via `git add -A` per the owner's
     instruction, committed as written despite its stale status line.
  3. Remote `origin` points at the SwiftFetch GitHub URL; the first push
     sends `main` with the full history (Phase 0 through the SwiftFetch
     rename) — no squash, no re-init.
- **Alternatives considered:**
  1. Squash/re-init history before the first push for a "clean" remote start
     — rejected: the append-only tracking history *is* the project's record;
     hiding it would contradict the owner's own append-only policy and the
     Phase 0-heading recovery disclosures.
  2. Never commit `AGENTS.md` — rejected by the owner's `add -A`.
  3. Branch-per-push or mirrored feature branches — rejected: a single
     `main` mirrors the local branch policy from ADR-0002.
- **Tradeoffs:** The pre-release history becomes externally visible as-is,
  including the honest incident disclosures (Phase 0 heading restoration,
  in-place mutation disclosures). The remote repository name encodes the
  brand (`NYN-05/SwiftFetch`), locking the identity to GitHub's naming
  (uppercase retained there, while the crate stays kebab-case — see
  ADR-0011). No secrets exist in the tree; `.env` is newly ignored as
  preventive hardening.
- **Related:** ADR-0002 (git init; remote aspect superseded here),
  ADR-0008 (verification recipe unchanged), ADR-0010 (append-only rule),
  ADR-0011 (product identity matching the remote name).

### Addendum-index extension (ADR-0012 row; frozen top index untouched)

| ID | Title | Status |
|---|---|---|
| ADR-0012 | Public remote repository and first push (owner-directed) | Accepted |

---

## ADR-0013 — Phase 2 test infrastructure: tokio + hyper stack in `[dev-dependencies]` only

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** Phase 2 (DOCS/phases.md §5) requires a controllable local
  HTTP test server able to serve correct/malformed/incomplete responses,
  honor or break byte ranges, truncate connections, delay delivery, omit
  `Content-Length`, redirect, and change validators — before any
  production download code exists (an explicit exit-gate constraint). The
  approved product stack (architecture §3) names **Reqwest** as the
  engine's HTTP client (added with Phase 3, per ADR-0003 phase gating);
  Reqwest itself sits on hyper. No previous phase introduced any network
  crate.
- **Decision:** Build the mock server on **hyper 1.x** (raw server
  connection control) with **hyper-util** (legacy client + Tokio IO),
  **tokio** (tests-only runtime), **http-body-util**, and **bytes**,
  declared strictly under `[dev-dependencies]` with a version-lift noted
  in `changes.md` (`0.3.0` → `0.4.0`). These crates compile under
  `tests/` only and never link into the product binary. Default fixtures
  are a pure function of `(seed, index)` (splitmix-style scramble),
  giving exact-content checks and a recorded golden hash contract without
  a crypto dependency.
- **Alternatives considered:**
  1. **Reqwest-only dev-dep plus a third-party mocking crate** (e.g. a
     wiremock-style library) — rejected: introduces a crate outside the
     approved stack without replacing hyper directly; precise response
     metadata lies (mismatched Content-Range), mid-body RST, and omitted
     Content-Length control are cleaner against raw hyper.
  2. **Hand-rolled HTTP server on `TcpListener`** — rejected: a nonconforming
     parser would make engine tests lie about HTTP/1.1 semantics.
  3. **Put tokio/hyper into `[dependencies]` from Phase 2** — rejected:
     test-only crates must never leak into the product binary; the
     product will get its own stack entries in the phases that first use
     them (Reqwest at Phase 3).
- **Tradeoffs:** Integration-test builds compile hyper + tokio + hyper-util
  (+ transitive) dev crates; the product binary is unaffected. A future
  ADR will record Reqwest arriving as the engine's HTTP client with a
  single shared, configured client (never per-request).
- **Related:** ADR-0003 (phase-gated dependency onboarding),
  ADR-0008 (verification recipe), ADR-0011 (identity/naming),
  `changes.md` Phase 2 entry.

### Addendum-index extension (ADR-0013 row; frozen top index untouched)

| ID | Title | Status |
|---|---|---|
| ADR-0013 | Phase 2 test infrastructure stack (tokio + hyper dev-deps) | Accepted |

---

## ADR-0014 - Phase 3: unit tests reuse the Phase 2 mock server via a `cfg(test)` module include; no `lib.rs`

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** Phase 3 (`DOCS/phases.md` §6) requires network-covered tests
  for the shared client and the resource prober, and the testing rule
  forbids live external URLs, so they must run against the Phase 2
  controlled mock server in `tests/common/`. The prober and its `HttpClient`
  live in the binary crate (`src/download/`, ADR-0004's modular monolith);
  Rust integration-test binaries cannot link against a `bin`-only crate's
  internals, so a `tests/` integration target cannot reach
  `HttpClient::default_client` or `probe()` directly.
- **Decision:** Declare the shared test support once more, as a test-only
  module of the binary crate, in `src/main.rs`:

  ```rust
  #[cfg(test)]
  #[path = "../tests/common/mod.rs"]
  pub(crate) mod common;
  ```

  The inline unit tests in `src/download/probe.rs` then use
  `crate::common::server::MockServer` exactly as the integration tests do.
  A `lib.rs` split is deliberately **not** introduced: no phase requires
  one, the architecture keeps a single binary crate, and such a split is
  the kind of structural change that needs its own ADR (per the
  "thin lib split is an ADR, not a quiet change" rule).
- **Alternatives considered:**
  1. **Introduce `lib.rs` now** so `tests/` can link the engine - rejected:
     late structural churn without a phase that needs it; would also
     silently change which items are "public API".
  2. **Duplicate a small mock server inside `src/` guarded by `cfg(test)`**
     - rejected: two copies of the server must never drift; `tests/common/`
     is the single source of truth for HTTP failure simulation.
  3. **Merge `tests/mock_server.rs` into the unit target** - rejected: the
     Phase 2 integration binaries keep testing the shared module through
     the normal `tests/` mechanism; nothing changes for them.
- **Consequences:**
  - With-angle-path resolution: `#[path]` is relative to the containing
    file, so the declaration lives in `main.rs` (`../tests/common/mod.rs`
    = repo-root `tests/`); deeper modules reference it as `crate::common`.
  - The fixture's six inline tests now execute in **both** test targets:
    `tests/fixtures.rs` and the binary's unit target (as
    `common::fixture::tests::...`). Total test counts double-count these
    six; this is disclosed in `changes.md` rather than hidden.
  - Zero impact on non-test builds: `cfg(test)` code never reaches the
    product binary.
- **Related:** ADR-0004 (single-crate layout), ADR-0008 (verification
  recipe), ADR-0010 (append-only tracking law), ADR-0013 (test-infra
  stack), `changes.md` Phase 3 entry.

### Addendum-index extension (ADR-0014 row; frozen top index untouched)

| ID | Title | Status |
|---|---|---|
| ADR-0014 | Phase 3 unit tests reuse `tests/common` via `cfg(test)` include | Accepted |

## ADR-0015 - Phase 4: sequential transfer timeout policy - engine-side phase bounds instead of a client-level overall cap

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** Phase 4 streams resources whose bodies may be arbitrarily
  large, while connect-through-headers must stay bounded and an idle
  (stalled) server must be detected. The Phase 3 `HttpClient` carried a
  provisional client-level overall timeout (30 s). A client-level reqwest
  timeout applies to the *whole* request including the body, so as a cap it
  would abort every legitimate long download's transfer, and as a raised
  number it would leave the header phase effectively unbounded. Empirical
  facts (verified against reqwest 0.12.28 before implementation):
  `RequestBuilder::timeout` accepts a plain `Duration` - there is no
  per-request way to clear or override the client default - and reqwest
  offers no per-request `read_timeout`, so "bounded ask, unbounded body"
  cannot be expressed through reqwest's own knobs.
- **Decision:** Keep the one-client law; move the time bounds to the
  engine, applied per phase:
  - `HttpTimeouts { connect, metadata, body_idle }` (defaults 15 s / 30 s /
    30 s, locked by a unit test). The former `overall` field is replaced by
    `metadata` + `body_idle`.
  - No client-level `.timeout()` anymore (this supersedes the Phase 3
    provisional-overall note in `http.rs`'s documentation).
  - `HttpClient::send` wraps the request future in
    `tokio::time::timeout(metadata)`: it bounds connecting, the redirect
    chain, and response-header arrival. The elapsed arm produces the same
    user-facing shape the Phase 3 tests lock:
    `the remote server did not answer in time: {redacted}`.
  - New `HttpClient::read_chunk(&self, &mut Response)` wraps each
    `response.chunk()` in `tokio::time::timeout(body_idle)`: every body
    read must make progress within the idle window, while the total body
    duration is deliberately unbounded (large downloads are legitimate).
    The elapsed arm produces
    `the remote server stopped sending data during the transfer: {redacted}`.
  - reqwest's own `connect_timeout` stays the client-side knob; connect
    failures still surface through `http_error`'s `is_timeout` branch with
    the same user message as in Phase 3.
- **Alternatives considered:**
  1. **Keep a client-level overall cap** - rejected: it bounds the body
     (the transfer would die mid-stream at the cap) or, if raised, leaves
     the header phase unbounded; neither matches the phase contract.
  2. **Per-request `.timeout(Duration)`** - rejected: a per-request timeout
     is also an overall future bound (bodies included); it cannot express
     "bounded metadata exchange, unbounded body with bounded idleness".
  3. **Idle watchdogs / instrumentation around reqwest's internals** -
     rejected: significant complexity for the first use case; reqwest
     exposes no per-request read timeout to hang such logic on.
- **Consequences:**
  - `tokio` becomes a product dependency with
    `features = ["fs", "io-util", "time"]` (first use in Phase 4, per
    ADR-0003's first-use discipline; runtime/bootstrap features stay out
    until a phase needs them). `bytes` becomes a direct dependency because
    `Response::chunk` yields `bytes::Bytes` in its public API; reqwest
    already pulled the crate, so the committed `Cargo.lock` gains zero new
    packages (the diff is the version line only - verified with
    `git diff --stat Cargo.lock`).
  - `Progress`-independent policy lives in `HttpClient`; callers (prober,
    engine) keep their own status/EOF handling.
  - Total transfer duration is intentionally unbounded; stall detection is
    strictly per read.
  - Verification: the idle path is bounded and test-verified
    (250 ms budget against a 900 ms stalled frame; both timing budgets are
    mocked, not live).
- **Related:** ADR-0003 (first-use), ADR-0008 (verification recipe),
  `changes.md` Phase 4 entry.

### Addendum-index extension (ADR-0015 row; frozen top index untouched)

| ID | Title | Status |
|---|---|---|
| ADR-0015 | Phase 4 transfer timeouts: engine-side metadata/idle bounds, no client-level overall cap | Accepted |

## ADR-0016 - Phase 4 destination naming and finalization policy

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** The reliability invariants forbid silently overwriting user
  files and forbid finalizing incomplete data; the PRD makes Windows the
  first-class platform (reserved names, illegal characters, per-component
  path length); and a failed transfer must keep its diagnostics. Phase 4
  needs a deterministic, honest name for a fetched resource and a
  finalization order that cannot race into replacing user content.
- **Decision:**
  1. **Filename derivation** (engine, `select_filename` from the *effective*
     URL): take the last non-empty path segment (the effective URL after
     redirects names the resource that was actually fetched; the query
     string never becomes the file name), percent-decode it manually per
     segment (RFC 3986, no direct `url`-crate dependency), then sanitize
     for Windows: invalid characters (`< > : " / \ | ? *`) and control
     codes become `_`; trailing dots/spaces are stripped; reserved device
     stems (`CON PRN AUX NUL COM1-9 LPT1-9`, case-insensitive, before any
     extension) gain a `_` prefix; the result is capped at 255 UTF-16 code
     units preserving the extension; results that collapse to empty
     (including `.`/`..`) fall back to the constant `download`.
     Decoded-before-sanitized order is mandatory: a decoded `/` or `\`
     must never survive to the filesystem.
  2. **Partial file** (storage layer, `PartialFile`):
     `<name>.swiftfetch-<pid>-<seq>.part` - process id plus a per-process
     one-up counter, so concurrent processes *and* future concurrent
     transfers inside one process can never share a partial name. It is
     created only after the transfer response proved to be content (HTTP
     200, no content transform, size agreement), so refused responses
     leave no filesystem trace. A leftover partial of a pid-reusing older
     process is deliberately truncated-recreated inside the engine's own
     namespace; anything outside `*.swiftfetch-*.part` is never touched.
  3. **Finalization exactly once, only for validated content:**
     `reserve_destination` claims the final name by exclusive creation
     (`create_new`, which fails atomically if the name exists) across the
     plain name and then `stem (n).ext` candidates (n from 1), each kept
     within the 255-UTF-16-unit component limit (stem re-truncation,
     extension preserved); `finalize` fsyncs the partial file and
     same-directory `std::fs::rename`s it over the own empty reservation
     (same directory ⇒ same volume, atomic). On rename failure the partial
     data is preserved and the own 0-byte reservation is best-effort
     removed; the documented microsecond race (another finalizer claims
     the plain name in between) can only ever shuffle suffix numbering -
     reservations are always empty, so no user content can be destroyed.
  4. **Failure law:** incomplete or failed transfers never reserve a final
     name; the partial file remains for the later recovery phase.
- **Alternatives considered:**
  1. **Timestamps or random tokens for fallback names** - rejected:
     non-deterministic; the same URL would silently produce different
     names across runs, and the deterministic `download` + `(n)` numbering
     is test-locked behavior.
  2. **Overwrite-by-default (a later `--force` could opt in)** - rejected
     for Phase 4: it contradicts the standing never-overwrite invariant;
     any such switch is a product-level decision of a later phase.
  3. **Copy partial → final, then delete the partial** - rejected: doubles
     I/O, and a crash between copy and delete leaves a stray extra file;
     rename is atomic and moves the already-synced inode-equivalent.
- **Consequences:**
  - Repeated downloads of one URL land on deterministic siblings
    `name`, `name (1)`, `name (2)`… (test-locked).
  - The recovery phase (later phases) identifies engine partials by the
    `*.swiftfetch-<pid>-<n>.part` suffix pattern.
  - Windows-specific semantics (reserved stems, UTF-16 component cap,
    `create_new` exclusivity, rename-over-empty-reservation) are validated
    on Windows in this phase; nothing is claimed as verified elsewhere.
- **Related:** ADR-0004 (modular layout), ADR-0008 (verification recipe),
  PRD Windows-first file handling, `changes.md` Phase 4 entry.

### Addendum-index extension (ADR-0016 row; frozen top index untouched)

| ID | Title | Status |
|---|---|---|
| ADR-0016 | Phase 4 naming and finalization: sanitized effective-URL names, engine-owned partials, no-overwrite reserve-and-rename | Accepted |

## ADR-0017 - Phase 5 SQLite persistence: rusqlite (bundled), single-writer repository, local app-data database, migration protocol

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** Phase 5 makes SQLite the source of truth for queue and
  recovery metadata (`DOCS/architecture.md` §9). The approved stack names
  "SQLite" and "a maintained Rust SQLite crate" without a concrete version;
  the database must live in the user's local application-data directory,
  separate from any download directory; the schema must be versioned from
  the first persisted release; the database registers metadata only — never
  file contents.
- **Decision:**
  1. **Crate:** `rusqlite 0.40` with the `bundled` feature (the SQLite
     engine itself compiles into the binary via `libsqlite3-sys`; no system
     SQLite library is required, keeping Windows builds reproducible).
     rusqlite is the long-maintained, conventional Rust SQLite wrapper; it
     is a sync API.
  2. **Connection architecture:** one SQLite connection per repository,
     serialized behind a `std::sync::Mutex` ("a controlled database-writer
     path is sufficient for this single-user application", §9). Operations
     are short; connection-level pragmas: `journal_mode = WAL`,
     `foreign_keys = ON` (SQLite defaults it off; the record tables rely on
     it), and `busy_timeout = 5 s` for cross-process contention only.
     `synchronous` is left at SQLite's FULL default: the durability of
     recovery metadata matters more than its write microspeed.
  3. **Repository law:** every public operation runs entirely within the
     call and commits before returning; there is no API that keeps a
     transaction open across a callback, a network transfer, or a file
     write. Multi-statement logical writes (status validation + update,
     record existence + update) run inside one
     `TransactionBehavior::Immediate` transaction every time; single-statement
     writes are atomic on their own. A panic inside any operation poisons
     the mutex, and later operations fail honestly rather than wrapping
     parallel work around damaged state.
  4. **Location:** `%LOCALAPPDATA%\SwiftFetch\swiftfetch.sqlite3` on
     Windows (the platform SwiftFetch treats as first-class). A missing or
     empty `LOCALAPPDATA` is a typed `Filesystem` failure with an
     actionable message - SwiftFetch refuses to guess a fallback location
     and silently write elsewhere. The parent directory is created when
     missing; an inaccessible location fails as "filesystem" naming the
     path.
  5. **Migration protocol:** an ordered `MIGRATIONS` list starting at
     generation 1; each entry applies its DDL plus the `schema_version`
     bookkeeping write inside one immediate transaction, so a fresh
     database that fails initialization stays completely empty and re-runs
     cleanly, and a partially applied generation never exists. A database
     claiming a generation newer than this build is refused outright
     ("update SwiftFetch or restore a compatible database"), never
     downgraded or guessed at.
  6. **Metadata-only law:** the record tables (`downloads`, `segments`,
     `extractions`) carry metadata columns only; a test enumerates
     `PRAGMA table_info` for every table and requires zero BLOB-typed
     columns, so downloaded file contents structurally cannot enter the
     database. Child tables reference `downloads(id)` with
     `ON DELETE CASCADE` so a later queue-cleanup phase cannot strand
     orphan segments/extractions.
- **Alternatives considered:**
  1. **`sqlx` (async, what the runtime runs)** - rejected: far heavier
     (async runtime integration, proc-macro query surface, more crates) for
     a workload that is short local statements behind one writer; nothing
     in the phase needs async database calls.
  2. **Depend on a system `sqlite3` / dynamic linking** - rejected: breaks
     reproducible Windows builds and clean-clone verification, and makes
     behavior depend on which SQLite each OS exposes.
  3. **PRAGMA `user_version` instead of a `schema_version` table** -
     rejected: the architecture names a `schema_version` table; that is the
     recorded contract (§9), not swappable in phase.
  4. **Caller-owned transactions (open/close handles returned to callers)** -
     rejected: it invites holding transactions across network or file work,
     which phase 5 forbids explicitly ("do not hold a database transaction
     open during network transfers or long file writes").
- **Consequences:**
  - The repository is `pub(crate)` and unwired until a later phase uses it
    (Phase 6 transfer records, Phase 7 queue, Phase 11 commands); the
    phase-standard file-level waiver cites that.
  - `created_at`/`updated_at` are caller-supplied unix-seconds stamps, so
    recovery phases stamp consistently and tests stay deterministic.
  - Out-of-range values (u64 beyond i64, inverted ranges, over-claimed
    progress) are rejected as owned before any write; persisted names that
    fail to parse on read (unknown status/category/state text, negative
    counts) are typed `Database` failures, never silently defaulted.
  - Multi-process contention is bounded by `busy_timeout`, but there is no
    cross-process simultaneous-writer test in this phase (disclosed in
    `changes.md`).
- **Related:** ADR-0003 (first-use), ADR-0018 (lifecycle graph),
  `DOCS/architecture.md` §3/§9, `changes.md` Phase 5 entry.

### Addendum-index extension (ADR-0017 row; frozen top index untouched)

| ID | Title | Status |
|---|---|---|
| ADR-0017 | Phase 5 SQLite persistence: rusqlite bundled, single-writer repository, local app-data location, atomic migrations | Accepted |

## ADR-0018 - Phase 5 download lifecycle graph: validated transitions at the storage boundary

- **Status:** Accepted
- **Date:** 2026-10-18
- **Context:** `DOCS/architecture.md` §7 lists ten lifecycle states and
  requires "state transitions must be validated by the application layer",
  with download completion and extraction completion as separate outcomes.
  Phase 5 defines the explicit, validated transition graph before resume
  (Phase 6) and scheduling (Phase 7) can drive it; commands remain unwired
  until Phase 11.
- **Decision:** The truth table, tested for all 100 `(from, to)` pairs
  (`DownloadState::allows`):
  - edges: `queued → probing, cancelled`; `probing → downloading, failed,
    cancelled`; `downloading → finalizing, retrying, paused, failed,
    cancelled`; `paused → downloading, cancelled`; `retrying → downloading,
    paused, failed, cancelled`; `finalizing → completed, failed`;
    `completed → extracting; extracting → completed`; `failed` and
    `cancelled` are terminal.
  - `update_status` validates and writes inside one immediate transaction:
    the record must exist, the target must differ from the current state
    (a no-transition write is an error, not a silent success), and the edge
    must be permitted; a rejected request changes nothing.
  - **Error-category policy:** entering `failed` *requires* a category;
    entering any other state *rejects* one. The column keeps one sharp
    meaning: why this download failed. Transient errors during
    `retrying`/`paused` carry no download-status category (their
    diagnostics log at the engine; the retry policy that would use them
    lands with Phase 6).
  - **Extraction is not download completion:** the download goes
    `completed → extracting → completed` while the `extractions` table
    records the extraction's own states, details, and failures. A
    successfully downloaded file never flips to `failed` because its
    extraction failed ("a file can be successfully downloaded while
    extraction fails", §7).
  - **Terminality:** `failed` may not re-route (the Phase 11 `retry`
    command will get its requeue edge by an explicit, tested decision when
    that phase arrives); `cancelled` is final and never reached after
    `finalizing` (the atomic rename lands in `completed`/`failed`; forced
    termination between the two is a startup-reconciliation case, not a
    status transition).
  - Extraction/segment states exist for persistence now but have no
    transition graph until their phases (8 and 10).
- **Alternatives considered:**
  1. **`completed → failed` allowed** - rejected: contradicts §7's complete
     ⟂ extraction-failure separation and risks erasing a good file's
     status.
  2. **`probing → paused` / `queued → failed` / other speculative edges** -
     rejected: pause takes effect at transfer/retry boundaries (Phase 7
     scheduling semantics), and input rejected before a record exists never
     becomes a failed download. Each later phase that needs an edge adds
     exactly that edge with its test, under this recorded contract.
  3. **Category recorded on every transition** - rejected: blurs the column
     into a general "last error" log; recovery would then need to guess
     whether a non-failed download actually failed.
  4. **State transitions outside transactions** - rejected: read-validate-
     write as one unit is the phase's explicit "transactions for changes
     that must be consistent" requirement.
- **Consequences:**
  - The truth table is part of the project contract: an edge may be added
    only with its owning phase's tests, never silently.
  - Corruptly stored status texts, categories, or negative numbers fail
    as typed `Database` errors on read (unknown names are refused with
    "the file may be corrupt or belong to another program" phrasing).
  - Honest reads: `get_download` of an unknown id is `None`; update
    operations of unknown ids are typed not-found failures.
- **Related:** ADR-0017 (repository), `DOCS/architecture.md` §7,
  `changes.md` Phase 5 entry.

### Addendum-index extension (ADR-0018 row; frozen top index untouched)

| ID | Title | Status |
|---|---|---|
| ADR-0018 | Phase 5 lifecycle graph: 20 tested edges, category policy, extraction stays separate from completion | Accepted |

## ADR-0019 - Phase 6 resume protocol: plan-before-network recovery, the min() trust rule, revalidated append-or-restart, bounded retries, and graceful-shutdown parking

- **Status:** Accepted
- **Date:** 2026-10-19
- **Context:** Phase 6 adds single-file resume, retry, and restart
  recovery for one URL + one output directory while the commands remain
  unwired until Phase 11. The phase's hard constraints: a crash between a
  file-system write and its database commit must be safe in both
  directions; bytes may never be appended to a partial unless the remote
  revalidates the continuing representation and answers with an exact
  ranged response; retries must be bounded; shutdown must arrive at any
  point and persist only trustworthy state; and the record's statuses move
  only along the ADR-0018 graph.
- **Decision:**
  - **Plan before network.** `plan_for` derives the recovery verdict for
    one `(url, output_dir)` scope from the database and the disk alone,
    before any request:
    `RecoveryPlan::{Fresh{record, partial}, AlreadyComplete{record},
    FinalizeAvailable{record, segment, partial}, CompleteInPlace{record},
    Resume{record, segment, partial, offset}}`.
    `AlreadyComplete` touches and sends nothing (idempotent recovery for a
    completed record whose destination file still matches its expected
    name); a completed record whose file is missing is a typed `Database`
    error that never re-runs the transfer; two unfinished records for one
    scope are refused as an input error rather than resolved by guessing.
  - **Trust rule (the min() law):** the resumable offset is
    `min(database-confirmed segment progress, partial file length)` over
    the newest non-`failed` `min_byte = 0` segment row of the record. A
    disk shorter than the database claim deconfirms the row immediately
    (`update_segment(state, file_len)`); a claim beyond the known expected
    size, or a partial longer than the expected size, downgrades to
    `Fresh` (restart into the same partial, truncated). A record whose
    expected size is unknown, or which has no segment row, never appends:
    no anchor means no append.
  - **Crash-between-write-and-commit is closed in both directions:** the
    stream fsyncs the partial *before* claiming the bytes in the segment
    row (confirm cadence 1 MiB; the final claimed write happens only after
    the closing `sync`), and the plan never trusts more than the disk
    holds, so either side lagging the other leads to a smaller - never a
    larger - optimistic offset.
  - **Revalidated append-or-restart.** A `Resume` continuation is granted
    only through all gates: a stored validator exists; `revalidate_identity`
    concludes `Verified`/`Tentative` - comparable validator pairs only (an
    ETag matches an ETag, a `Last-Modified` matches a `Last-Modified`; an
    identity whose effective URL drifted elsewhere is `Changed`); range
    support is proven by the probe's evidence; the ranged challenge is
    answered `206 Partial Content` whose `Content-Range` starts exactly at
    the offset, whose span agrees with the response length, and whose
    total does not contradict the record's expected size; and the
    response's own validators are cross-checked against the stored ones.
    A `200` (fetch the full content that is in hand), a `416`, a
    tentative identity confirmed only by the challenge, an unrepresentable
    header value, and any contradiction all downgrade to a full restart
    within the same attempt - and a restart never appends.
  - **Bounded retries with per-process jitter.** `RetryPolicy::default`
    allows 5 retries (`attempt_budget = max_retries + 1` attempts per
    invocation), nominal delay 500 ms × 2^(attempt) capped at 8 s, full
    jitter over `[0, nominal]` seeded from `RandomState` so simultaneous
    failures do not synchronize. `classify` retries only transient
    failures (5xx, unknown statuses, timeouts, transport drops,
    premature-end) through the hidden `HttpFailure` marker carried in
    sources; everything else fails the record at once. A transient failure
    accumulates `retry_count` only when a record exists (the fresh flow
    inserts its record after the first probe succeeds, so a pre-record
    blip is retried but not counted), and shutdown during the backoff
    parks the record `paused` with the count intact.
  - **Graceful shutdown.** `ShutdownSignal`/`ShutdownHandle` trip a
    `tokio::sync::watch` bool. The stream races every chunk read against
    the signal; shutdown inside the stream syncs the partial, claims the
    confirmed total in the segment row, moves the record to `paused`, and
    returns a `Cancelled` error whose message promises `paused and`
    `resumable`. Shutdown between phases is honored at the next boundary;
    a metadata exchange already in flight finishes under its own bounded
    timeout (ADR-0015) or fails transiently. Finalization itself is never
    interrupted: the atomic rename always completes once claimed.
  - **Finalization and best-effort status writes.** Finalization claims
    the destination via an exclusive reservation, renames same-volume
    under the no-overwrite policy (ADR-0016), records the divergence via
    `set_destination` when the final name resolves elsewhere, and writes
    `completed`. A failure to reserve/rename becomes a typed failure plus
    a best-effort `failed` status write (category reuses the failure);
    status or retry-count write failures during failure handling are
    logged best-effort and never mask the transfer's own error - the
    record remains recoverable, which is the point of parking.
  - **Engine entry.** `transfer_persisted(client, repo, url, output_dir,
    Arc<Progress>, &RetryPolicy, now, &ShutdownHandle) -> Result<
    SequentialDownload, AppError>` is the one persisted engine entry
    point; its eight parameters are the wiring contract Phase 11 will
    satisfy exactly once per configured download (narrow
    `too_many_arguments` waiver on the function).
- **Alternatives considered:**
  1. **Trust the database claim alone** (sum of segment received bytes) -
     rejected: a crash between write and commit would append a hole or
     duplicate bytes onto a shorter file forever; the trust minimum exists
     precisely because fs + SQLite are not one transaction.
  2. **Trust the partial length alone without deconfirming** - rejected:
     silently held progress would re-plans forever larger; the row is
     corrected down so that the database and the disk agree by the time
     the plan is used, and repeated reconstruction is idempotent.
  3. **`If-Range` trust alone** (send the stored validator, trust the
     response) - rejected: a server may ignore `If-Range` and answer `206`
     with unmatched bytes; the engine cross-checks echoed validators
     against the record so a lying or range-ignorant server can never
     produce a mixed content stream through this path.
  4. **Validate only status/`Content-Range` on the challenge; accept an
     unvalidated first-chunk append** - rejected: contradicts the phase
     law "never append without revalidation"; the strictness also costs
     nothing in cases where the mock's honest answers are used.
  5. **Retry backoff without jitter / per-attempt linear backoff** -
     rejected: synchronized retries would stampede the remote after an
     outage; the exponential-with-full-jitter shape is standard and cheap.
  6. **Shutdown by aborting the task / dropping futures** - rejected:
     resurrect-time state must be trustworthy without a journal; explicit
     park (sync → claim → `paused`) gives the record a durable, replayable
     resting state (`Interrupted`), and the process-level kill path is
     just the Phase 6 startup recovery running against whatever parts of
     that park landed.
- **Consequences:**
  - The truth rule and downgrade matrix are part of the contract: changing
    any gate requires updating the ADR and its tests together.
  - The `200`-falls-back-to-full-content rule wastes no bytes but also
    never lets a `206` masquerade as full content: a ranged response to a
    non-ranged request is an `Http` failure, not file content.
  - Records whose partial file grows beyond the expected size restart on
    recovery; nothing about partial sizes is user-visible state.
  - The engine is honest about progress accounting: a resume's
    `bytes_written` reports the whole session's streamed total including a
    resumed prefix, while the DB records only what this process added.
  - The retry budget is per invocation, not global: successive user-invoked
    runs of the same download each reset the attempt budget while
    `retry_count` keeps the cumulative story in the record.
- **Related:** ADR-0015 (timeout policy), ADR-0016 (naming/finalization),
  ADR-0017 (repository/storage), ADR-0018 (lifecycle graph), `DOCS/
  phases.md` Phase 6, `changes.md` Phase 6 entry.

### Addendum-index extension (ADR-0019 row; frozen top index untouched)

| ID | Title | Status |
|---|---|---|
| ADR-0019 | Phase 6 resume protocol: plan-before-network recovery, min() trust rule, revalidated append-or-restart, bounded retries, graceful shutdown parking | Accepted |



