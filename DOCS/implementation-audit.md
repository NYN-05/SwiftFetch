# Implementation Audit — All 14 Phases

**Audit scope:** Every task, feature, test, and acceptance criterion defined in
`DOCS/phases.md`, cross-checked against `DOCS/PRD.md`, `DOCS/architecture.md`,
and the actual source code at commit `fba337b` (Phase 14 complete).

**Authoritative sources:** `DOCS/phases.md`, `DOCS/PRD.md`,
`DOCS/architecture.md`, source under `src/`, tests under `tests/`.
Code comments are supplementary only; they never override documented
requirements or observed implementation behavior.

**Verification environment:** Windows (rustc 1.99.1, cargo 1.99.1, MSVC).
All checks ran on Windows; no results are presented as Windows proof that
were not executed on Windows.

---

## Executive Summary

All 14 phases are implemented. The codebase builds cleanly, `cargo fmt --check`
and `cargo clippy --all-targets --all-features -- -D warnings` pass with zero
warnings, and a release-mode executable builds and was smoke-tested end-to-end
against a local HTTP fixture (concurrent download, byte-exact verification,
`status`, automatic ZIP extraction, `--no-extract` — all pass).

**Test results (working tree, canonical recipe):**

| Target | Passed | Failed | Ignored |
|---|---|---|---|
| Unit (`src/**`) | 277 | 7 | 5 |
| `tests/cli.rs` | 24 | 0 | 0 |
| `tests/fixtures.rs` | 15 | 0 | 0 |
| `tests/mock_server.rs` | 33 | 0 | 0 |
| `tests/windows.rs` | 24 | 0 | 0 |
| Benchmarks (`--ignored`) | 5 | 0 | 0 |

**The 7 unit-test failures are pre-existing Phase 9-lineage
segmented-recovery failures** (`download::resume::tests::*`):
`segmented_recovery_skips_verified_completed_segments`,
`segmented_recovery_failed_segment_retried`,
`segmented_recovery_handles_oversized_partial`,
`segmented_recovery_handles_truncated_partial`,
`segmented_recovery_handles_unknown_size`,
`segmented_recovery_shutdown_parks_paused`,
`a_truncated_segment_is_retried_within_the_budget_and_never_finalized_early`.

These fail identically on the v0.14.0 baseline (verified before any Phase 13/14
change) and were deliberately left untouched per the phase-boundary law
(ADR-0023). They are documented in the `changes.md` Phase 10/11/12/13 entries
and in ADR-0023 as "reproduced, not repaired," pending the owner's explicit
decision on a Phase 9 repair. No Phase 13 or Phase 14 change introduced or
worsened any failure.

**No unauthorized features, dependencies, or architectural changes** were
introduced. The product binary contains no test-only crates (hyper,
hyper-util, http-body-util, sha2, windows-sys are `[dev-dependencies]` only;
plain `cargo build` re-locks this boundary every run).

---

## Phase-by-Phase Traceability Matrix

| Phase | Requirement (DOCS/phases.md) | Implementation | Tests | Actual Result | Status |
|---|---|---|---|---|---|
| **0** | Reproducible Rust project; package, source layout, tests; lockfile; fmt/clippy/test commands; README | `Cargo.toml` (edition 2021, empty deps), `src/main.rs`, module placeholders, `Cargo.lock` tracked, `README.md` | Unit smoke test | Builds; 2 tests pass; fmt/clippy/test green | **PASS** |
| **1** | Clap CLI: `download`/`status`/`resume`/`retry` + `-v`; `AppConfig` defaults; typed errors; tracing to stderr; `redact_url`; early input validation | `src/cli/commands.rs`, `src/cli/mod.rs`, `src/config.rs`, `src/error.rs`, `src/logging.rs`, `src/main.rs` | 11 integration + unit tests | All pass; exit codes 0/1/2 verified; URL redaction verified | **PASS** |
| **2** | Mock server (hyper): honored/ignored/broken ranges, truncation, delays, errors, missing length, redirects, changed validators; deterministic fixtures | `tests/common/server.rs`, `tests/common/fixture.rs` | 15 fixtures + 24 mock_server | All pass; server asserts methods/range headers/request counts | **PASS** |
| **3** | Shared `reqwest::Client`; probe HEAD + ranged GET; `RangeSupport` Yes/No/Unknown; strict `Content-Length`/`Content-Range` parsing; contradiction downgrade; effective URL; validators | `src/download/http.rs`, `src/download/probe.rs` | 37 probe + 4 http unit tests | All pass; no live external URLs; contradiction handling verified | **PASS** |
| **4** | Sequential download; bounded streaming; Windows-safe filename sanitization; partial file `.swiftfetch-<pid>-<n>.part`; size validation; no-overwrite `create_new` + rename; error bodies never written | `src/download/sequential.rs`, `src/storage/file_writer.rs` | 25 sequential + 6 file_writer | All pass; byte-exact; bounded memory (largest write < file size); no silent overwrite | **PASS** |
| **5** | SQLite (rusqlite bundled); schema versioning; 10-state lifecycle graph; repository CRUD; transactions; `%LOCALAPPDATA%\SwiftFetch\swiftfetch.sqlite3`; no BLOB columns | `src/storage/schema.rs`, `src/storage/repository.rs` | 18 repository + 8 schema | All pass; metadata persists between runs; transitions validated; no BLOB columns | **PASS** |
| **6** | Resume with `min(db-confirmed, on-disk)` trust rule; validator revalidation (ETag/Last-Modified comparable pairs); bounded retries (5 retries, exponential backoff + jitter); graceful shutdown parking; crash-between-write-and-commit safety | `src/download/resume.rs`, `src/download/retry.rs` | 27 resume + 11 retry | **7 FAIL** (Phase 9-lineage, see below); all other resume/retry tests pass | **FAIL** (pre-existing) |
| **7** | URL batches; URL-list file; bounded worker pool `min(limit, URLs)`; queue-before-network; adoption-not-duplication; independent failures; peak concurrency gauge | `src/app/batch.rs` | 23 batch tests | All pass; concurrency limit enforced; per-download independence verified | **PASS** |
| **8** | Segmented downloads; evidence-gated (`RangeSupport::Yes` + known size ≥ 2 spans); overflow-safe planner; per-span validation (206 + Content-Range + Content-Length + no transform); sequential fallback; bounded workers | `src/download/segmented.rs` | 12 planner tests | All pass; exact cover verified; overflow boundaries safe | **PASS** |
| **9** | Crash-consistent segmented recovery; `reconcile_segmented_download`; segment state graph; `Pending → Downloading → Completed`; conservative restart | `src/download/resume.rs` (Phase 9 logic integrated here) | Part of the 27 resume tests | **7 FAIL** (same Phase 9-lineage set as Phase 6) | **FAIL** (pre-existing) |
| **10** | ZIP extraction after finalization; magic-byte detection; sealed entry paths (no traversal, no drive/UNC, no reserved stems); no silent overwrite; archive preserved on failure; extraction retried without redownload; extraction state separate from download | `src/archive/zip_extract.rs` (engine-level tests in `resume.rs`) | Phase 10 tests in resume.rs | All pass; path traversal blocked; archive preserved; retry without download verified | **PASS** |
| **11** | All commands wired; progress display (rolling 2s window, honest unknowns); status listing; resume/retry from persistent state; exit codes; `SWIFT_FETCH_DATABASE` override; Ctrl+C shutdown | `src/app/mod.rs`, `src/app/commands_handler.rs`, `src/cli/progress.rs`, `src/cli/status.rs` | 24 integration + unit tests | All pass; exit codes 0/1/2 verified; redaction verified; hermetic DB via env var | **PASS** |
| **12** | Windows resilience: reserved stems, illegal chars, overlong names, long paths, access-denied (deny ACE), locked files/DB, forced termination + resume, concurrent processes, bounded queue, clean install (no services/auto-start) | `tests/windows.rs` (24 tests) | 24 Windows tests | All pass on Windows; graceful Ctrl+C e2e disclosed as not simulatable on this runner | **PASS** |
| **13** | Benchmarks: sequential baseline, segment-count comparison, multi-file concurrency, large-file resource bound, long URL-list stress; throughput/CPU/RAM/requests recorded; byte-exact verification | `src/benchmarks.rs` (5 ignored + 1 sanity) | 5 ignored + 1 sanity | All pass; no universal speed claim; defaults retained as provisional | **PASS** |
| **14** | README documentation; command reference; config; data location; retry/resume/ZIP behavior; known limitations; build/test/release commands; release build + smoke test | `README.md`, `changes.md` | N/A (documentation phase) | Release build passes; smoke test passes; docs accurate | **PASS** |

---

## Detailed Findings per Phase

### Phases 0–5: PASS (no defects)

All foundational phases are fully implemented and tested. The code matches the
documented requirements:

- **Phase 0:** Single package at repo root; `Cargo.lock` tracked; fmt/clippy/test commands established; `README.md` present.
- **Phase 1:** Clap contract with all 4 commands + `--verbose`; `AppConfig` defaults match `architecture.md` §6 (3 files, 4 segments, 5 retries, extract enabled); `ErrorCategory` taxonomy with exit codes (Input=2, else=1); tracing to stderr; `redact_url` strips query/fragment and masks passwords.
- **Phase 2:** Mock server supports all required modes; fixtures are deterministic splitmix pattern; no live external URLs.
- **Phase 3:** Single shared `reqwest::Client` (`HttpClient` is `Clone`, clones share the connection pool); probe performs ≤ 2 requests; strict header parsing; contradiction → `Unknown`; effective URL recorded after redirects.
- **Phase 4:** Sequential streaming with bounded reads; Windows filename sanitization (reserved stems, invalid chars, trailing dots/spaces, 255 UTF-16 cap); partial file naming `<name>.swiftfetch-<pid>-<n>.part`; `reserve_destination` uses `create_new` (atomic, no overwrite); `finalize` does same-volume rename; error bodies never written; size contradiction refused.
- **Phase 5:** SQLite with WAL + foreign keys + 5s busy timeout; schema version `SCHEMA_VERSION = 1`; atomic migrations; 10-state lifecycle with 20 tested edges; `failed` requires category; no BLOB columns; DB in `%LOCALAPPDATA%`.

### Phase 6: PASS (except 7 pre-existing Phase 9 failures)

The resume/retry engine implements the ADR-0019 protocol:
- `plan_for` derives `RecoveryPlan` from DB + disk before any network.
- Trust rule: `min(db-confirmed, on-disk)`; deconfirmation persisted.
- Identity revalidation via comparable validator pairs only.
- Bounded retries: `RetryPolicy { max_retries: 5, base_delay: 500ms, factor: 2, max_delay: 8s }` with full jitter.
- Graceful shutdown via `watch` channel; parks `paused`; sync-before-claim ordering.

All Phase 6 tests pass except the 7 Phase 9-lineage failures listed below.

### Phase 7: PASS

Batch layer implements all laws:
- Validation before work (all-or-nothing list file validation).
- Queue-before-network; one `queued` record per URL before any bytes fetched.
- Bounded pool: `min(max_concurrent_files, URLs)` workers; bounded channels.
- Independent failures; `peak_active_files` gauge.
- Adoption-not-duplication; pristine-name refresh on first probe.

### Phase 8: PASS

Segmented downloads:
- Eligibility: `RangeSupport::Yes` + known size ≥ 2 spans + ≥ 32 MiB.
- Overflow-safe planner (`div_ceil`, saturating extension); exact cover.
- Per-span validation: 206 + exact Content-Range + Content-Length reconciliation + no transform.
- Independent positioned handles (`open_for_segment`); no shared cursor.
- Safe fallback: retire rows + truncate to zero + sequential re-fetch.
- Bounded: `JoinSet` with `max_segments` workers; global bound via batch layer.

### Phase 8: PASS (segmented engine tests)

All 12 planner tests pass; segmented end-to-end tests in `resume.rs` pass (happy path, parallel workers, threshold gating, unknown-size gating, range-ignoring fallback, broken-headers fallback, overrun refusal, segment 500, truncated segment retry, shutdown parking).

### Phase 9: **FAIL — 7 pre-existing failures** (see Defects section below)

The crash-consistent segmented recovery logic in `reconcile_segmented_download`
(`src/download/resume.rs` lines 1036–1236) and `segmented_resume_attempt`
(lines 1981–2426) has 7 failing tests. These are **pre-existing** — they fail
identically on the v0.14.0 baseline and were documented as such in the
Phase 10/11/12/13 `changes.md` entries and ADR-0023.

The failing tests are:
1. `segmented_recovery_skips_verified_completed_segments`
2. `segmented_recovery_failed_segment_retried`
3. `segmented_recovery_handles_oversized_partial`
4. `segmented_recovery_handles_truncated_partial`
5. `segmented_recovery_handles_unknown_size`
6. `segmented_recovery_shutdown_parks_paused`
7. `a_truncated_segment_is_retried_within_the_budget_and_never_finalized_early`

These are not regressions from Phase 13 or 14 (neither phase changed engine
code). They are pending the owner's explicit decision on a Phase 9 repair.

### Phase 10: PASS

ZIP extraction engine:
- Magic-byte detection (`PK\x03\x04`, `PK\x05\x06`, `PK\x07\x08`); extension is only a hint.
- Sealed entry paths: absolute, `..`, drive/UNC, reserved stems all rejected; resolved-path containment check.
- No silent overwrite: skip-and-report by default; explicit `overwrite_existing` opt-in.
- Bounded: 1 GiB/entry, 10 GiB total, checked on declared AND written size; partial bomb removed.
- Archive preserved on failure; extraction state separate from download; retry without network.

### Phase 11: PASS

All commands wired end-to-end:
- `download` → `run_batch` with live progress (250ms sweep, rolling 2s speed window).
- `status` → read-only listing with confirmed bytes and extraction failure separately reported.
- `resume` → `RECOVERABLE` states only; `retry` → `failed → queued` requeue edge.
- Exit codes: 0/1/2 mapped via `CommandEnd` / `ErrorCategory`.
- `SWIFT_FETCH_DATABASE` env override for hermetic tests.
- Ctrl+C trips shutdown handle; in-flight transfers park `paused`.

### Phase 12: PASS (Windows, native)

All 24 Windows tests pass:
- Reserved stems (`CON` → `_CON`), illegal chars, overlong names (255 UTF-16 cap), long paths (honest branch).
- Access-denied via deny ACE (not read-only attribute); occupied paths; locked files/DB via `ExclusiveLock` (share-mode-0 `CreateFileW`).
- Forced termination + resume through real CLI; multi-transfer kill + resume; 3-cycle idempotency.
- Large queue: 40 URLs, `peak_active_gets ≤ 4`, working set < 250 MiB.
- Clean install: no services/auto-start/resident process; missing `LOCALAPPDATA` refused typed.

**Disclosed limitation:** Graceful Ctrl+C e2e is not simulatable on this test runner (three probe campaigns confirmed the accepted console event never dispatches to any registered handler under the OpenCode console host). The engine's graceful-shutdown contract remains unit-locked. Disk-full and cross-volume finalize are disclosed as not simulatable.

### Phase 13: PASS

Benchmarks implemented as `#[ignore]`d tests (repeat with `--ignored --nocapture`):
- Sequential baseline: 64 MiB, 25.9–38.2 MiB/s, byte-exact.
- Segment comparison: 64 MiB, max_segments {1,2,4,8}; sequential fastest on loopback (no universal speed claim).
- Multi-file concurrency: 8×16 MiB, limits {1,3,8}; `peak_active_files == limit` in all runs.
- Large file: 128 MiB, working-set delta ≤ ~5.1 MiB (bounded streaming).
- Long URL list: 200×4 KiB, limit 4; all complete; `peak_active_files == 4`.
- Sanity check: 4 KiB, 3 requests (probe + GET), runs in every `cargo test`.

### Phase 14: PASS

- README rewritten with command reference, config table, data location, retry/resume/ZIP behavior, exit codes, build/test/release commands, known limitations.
- Release build (`cargo build --release`) passes; `target/release/swift-fetch.exe` smoke-tested against local HTTP fixture: concurrent download exit 0, byte-exact, `status` lists completed records, auto-extraction works, `--no-extract` preserves archive.
- Dependency/packaging review: no new deps; all dev-deps test-only; no services/telemetry/GUI.
- `changes.md` Phase 14 entry + verification addendum (append-only).

---

## Defects and Severity

### D-01: Phase 9 segmented-recovery tests fail (7 tests)

**Severity:** High (test-level; engine behavior is safe — failures cause safe restart, not data corruption)

**Description:** Seven tests in `src/download/resume.rs` fail. They exercise
Phase 9 crash-consistent segmented recovery. The failures occur because the
test fixtures store ETag values that do not match the mock server's static
ETags (`"etag-seg"`, `"etag-trunc"`, `"etag-over"` vs `"fixture-42-32768"`),
causing identity revalidation to return `Identity::Changed`, which triggers a
safe full restart instead of a segmented resume. The tests then assert
segmented-resume-specific behavior (only incomplete segments re-fetched) that
does not occur on a restart.

Additionally, `segmented_recovery_handles_oversized_partial` asserts
`ranged.len() == 0` but counts the probe's `bytes=0-0` evidence request as a
ranged GET (unlike `transfer_ranges` which filters it out).

**Evidence:** All 7 fail identically on the v0.14.0 baseline (verified
pre-Phase 13). Panic locations: `resume.rs:4726, 4959, 5114, 5257, 5441`.

**Behavior impact:** None in production. When identity cannot be verified,
the engine correctly restarts from zero (safe behavior). The tests assert
optimistic resume behavior that requires matching validator metadata.

**Recommended corrective action:** The test fixtures should store ETag values
that match the mock server's `Validators::Static` output
(`format!("\"fixture-{seed}-{len}\")"`), or the tests should be updated to
expect a full restart (0 ranged segment GETs) when identity cannot be
verified. Do not weaken the assertions; align the fixture data with the
server's actual behavior. Alternatively, the owner may authorize a Phase 9
repair of the engine's recovery logic if the code itself is found defective
rather than the test fixtures.

**Status:** Unresolved (pre-existing; pending owner decision per phase-boundary law).

---

## Test Results Detail

### `cargo test --no-fail-fast` (working tree, Windows)

```
unit target:    FAILED. 277 passed; 7 failed; 5 ignored (38.90s)
tests/cli.rs:   ok. 24 passed; 0 failed (5.89s)
tests/fixtures.rs: ok. 15 passed; 0 failed
tests/mock_server.rs: ok. 33 passed; 0 failed (2.04s)
tests/windows.rs: ok. 24 passed; 0 failed (6.61s)
```

The 7 failures are all in `download::resume::tests::*` (listed in D-01 above).
The 5 ignored are the Phase 13 benchmark long measurements.

### `cargo fmt --check`: PASS (exit 0)

### `cargo clippy --all-targets --all-features -- -D warnings`: PASS (exit 0, 0 warnings)

### `cargo build` (plain product build): PASS (exit 0)

### `cargo build --release`: PASS (exit 0)

### Release-exe smoke test: PASS

- `download url1 url2 --no-extract` → exit 0, "2 completed, 0 failed, 0 interrupted of 2; peak active downloads 2 of the configured 3"
- Byte-exact fixture verification
- `status` → lists completed records with sizes
- `--no-extract` → archive preserved, no extraction folder
- Default (extract enabled) → `payload\inside.txt` extracted; archive preserved

---

## Windows-Specific Verification Status

All Windows-specific claims were validated on Windows (MSVC, rustc 1.99.1):

| Claim | Evidence | Status |
|---|---|---|
| Reserved stems sanitized | `a_reserved_stem_url_never_reaches_a_windows_device` | PASS |
| Illegal chars sanitized | `illegal_url_characters_sanitize_for_windows_end_to_end` | PASS |
| Overlong name cap (255 UTF-16) | `an_overlong_effective_name_is_capped_at_255_units_end_to_end` | PASS |
| Long paths (honest branch) | `deep_long_path_destination_matches_the_environment_honestly` | PASS |
| Access-denied (deny ACE) | `a_write_denied_output_directory_fails_typed_and_records_the_failure` | PASS |
| Occupied output path | `an_output_directory_occupied_by_a_file_fails_typed` | PASS |
| No silent overwrite | `an_occupied_destination_is_never_overwritten_and_a_sibling_is_used` | PASS |
| Locked DB fails typed + recovers | `a_database_locked_by_a_foreign_handle_fails_typed_and_recovers` | PASS |
| Locked partial fails resume + retry recovers | `an_engine_partial_held_locked_fails_the_resume_typed_and_retry_recovers` | PASS |
| Concurrent processes share DB | `concurrent_processes_on_disjoint_urls_share_one_database_safely` | PASS |
| Same-scope race (honest outcome) | `concurrent_same_scope_processes_never_corrupt_or_fake_success` | PASS |
| Missing LOCALAPPDATA refused | `a_missing_localappdata_refuses_typed_before_any_network` | PASS |
| Forced termination + resume | `forced_termination_mid_transfer_recovers_through_the_actual_cli` | PASS |
| Multi-transfer kill + resume | `forced_termination_of_multiple_active_transfers_recovers_all` | PASS |
| 3-cycle idempotency | `repeated_startup_shutdown_cycles_are_idempotent_and_nonduplicating` | PASS |
| Large queue bounded | `a_large_queue_respects_the_global_limit_and_stays_reasonably_bound` | PASS |
| Clean install (no services/auto-start) | `a_clean_run_installs_no_services_auto_start_or_resident_processes` | PASS |

**Disclosed as not simulatable on this runner:**
- Graceful Ctrl+C e2e (console event never dispatches to handlers under OpenCode console host)
- True disk-full
- Cross-volume finalize
- Kill inside sub-millisecond rename window

---

## Scope and Dependency Review

**No unauthorized features:**
- No GUI, browser integration, plugins, telemetry, advertising, services, daemons, auto-start.
- CLI-only; confirmed by `tests/windows.rs` clean-install test and code review.

**No unauthorized dependencies:**
- `[dependencies]`: reqwest, bytes, zip, tokio, rusqlite, clap, thiserror, tracing, tracing-subscriber — all from the approved stack, added at first use.
- `[dev-dependencies]`: tokio, hyper, hyper-util, http-body-util, bytes, sha2, windows-sys — test-only; never linked into product binary (verified: plain `cargo build` compiles without them).
- `windows-sys` features: `Win32_Foundation`, `Win32_Storage_FileSystem`, `Win32_System_Threading` — dev-only, used by benchmarks for CPU sampling and by `tests/windows.rs` for exclusive locks.

**No architectural changes:** Module layout matches `architecture.md` §5; single modular monolith; no `lib.rs` split; no workspace.

---

## Final Verification Summary

| Requirement Group | Status | Evidence |
|---|---|---|
| One URL download | PASS | `tests/cli.rs::a_single_url_downloads_end_to_end_and_exits_0` |
| Multiple URLs + URL-list file | PASS | `tests/cli.rs::inline_urls_and_a_list_file_download_together` |
| Concurrent files within limits | PASS | `tests/cli.rs`, `tests/windows.rs` large-queue test; `peak_active_files == limit` |
| Segmented download when safe | PASS | `resume.rs` segmented happy-path tests |
| Safe fallback when ranges absent/invalid | PASS | `resume.rs` range-ignoring/broken-headers fallback tests |
| Honest progress/speed/ETA | PASS | `src/cli/progress.rs` honesty rules; unknown-size/unknown-ETA wording |
| Resume interrupted downloads | PASS (except 7 pre-existing Phase 9 failures) | `resume.rs` resume tests; shutdown parking |
| Bounded retries | PASS | `retry.rs` classification + budget tests; exhausting-budget test |
| Recovery after forced termination | PASS | `tests/windows.rs` forced-termination tests |
| Detect changed remote files | PASS | `resume.rs` validator revalidation; `a_changed_validator_restarts_instead_of_appending` |
| Never finalize incomplete/corrupt | PASS | size validation; segment validation; no final name for incomplete data |
| No silent overwrites | PASS | `reserve_destination` `create_new`; `(n)` collision suffixes |
| Safe Windows filenames/paths | PASS | `sequential.rs` sanitization; `tests/windows.rs` |
| Auto-extract ZIP by default | PASS | `resume.rs` Phase 10 integration; `zip_extract.rs` |
| Disable ZIP extraction | PASS | `--no-extract`; `extraction_is_skipped_when_disabled` |
| Prevent ZIP path traversal | PASS | `zip_extract.rs` `validate_extracted_path` |
| Preserve archive on extraction failure | PASS | `a_corrupt_zip_completes_with_a_failed_extraction_record` |
| Retry extraction without redownload | PASS | extraction record retried without HTTP surface |
| Bound concurrency/memory/retries | PASS | bounded worker pool; streaming; RetryPolicy; working-set < 250 MiB |
| Full test suite on Windows | PARTIAL | 277/7 unit (7 pre-existing Phase 9 failures); all integration targets green |
| Accurate documentation | PASS | README rewritten for Phase 14; only documented implemented behavior |
| CLI-only, no unrelated features | PASS | clean-install test; code review |

**Overall: The project is NOT yet fully verified.**

7 acceptance criteria (Phase 9 segmented-recovery tests) remain **FAIL** due to pre-existing defects. All other criteria pass.

---

## Prioritized Unresolved Issues

### 1. Phase 9 segmented-recovery test failures (7 tests) — HIGH

**Issue:** Seven tests in `src/download/resume.rs` fail, all related to
crash-consistent segmented recovery. The failures stem from test fixtures
storing ETag values that don't match the mock server's static ETags, causing
identity revalidation to return `Changed` and triggering a safe full restart
instead of a segmented resume. One test (`segmented_recovery_handles_oversized_partial`)
also counts the probe's `bytes=0-0` evidence request as a ranged segment GET.

**Impact:** No production impact — the engine behaves safely (restarts instead
of resuming when identity can't be verified). The failures are test-level
only and were acknowledged as pre-existing in Phase 10/11/12/13.

**Recommended action:** The owner should authorize a Phase 9 repair. The fix
would involve either:
(a) Correcting test fixtures to use ETags matching the mock server's
`Validators::Static` output, or
(b) Updating tests to expect safe restart behavior when identity cannot be
verified, or
(c) Repairing the engine's `reconcile_segmented_download` /
`segmented_resume_attempt` logic if a genuine code defect is identified.

Do not weaken or remove the tests — align them with correct behavior.

### 2. Graceful Ctrl+C e2e not simulatable — MEDIUM (disclosed limitation)

**Issue:** Three probe campaigns confirmed that accepted console events never
dispatch to any registered handler under the OpenCode console host. A graceful
Ctrl+C e2e would test the host, not the product.

**Recommended action:** Documented as a limitation in Phase 12/14. The
engine's graceful-shutdown contract is unit-locked
(`ShutdownSignal`/`ShutdownHandle` unit tests in `resume.rs`).

### 3. True disk-full not simulatable — LOW (disclosed limitation)

**Issue:** No reliable absolute-space exhaustion primitive on Windows in CI
conditions.

**Recommended action:** Documented as a limitation. Discoverable write-failure
paths (access-denied, occupied outputs) are tested.

---

## Audit Conclusion

Phases 0–14 are implemented per `DOCS/phases.md`. The codebase is
well-structured, follows the approved stack and dependency-first-use
discipline, passes fmt/clippy/build, and all integration tests pass on
Windows. The only failures are 7 pre-existing Phase 9-lineage segmented
recovery tests that fail identically on the v0.14.0 baseline and are
documented as pending the owner's decision.

**The project may not be declared fully verified** until the 7 Phase 9
failures are resolved (either by test-fixture alignment, test expectation
correction, or engine repair authorized by the owner).