# Feature Specification: Fix four tests failing on `main` behind the masked CI gate

**Feature Branch**: `fabrik/issue-429`
**Created**: 2026-08-17
**Status**: Specified
**Input**: User description: "Four tests fail on main today. All four are invisible because the jobs running them cannot report failure — see the companion issue on the `| tee`/pipefail gate defect. This issue is the prerequisite: every one of these must be fixed or explicitly quarantined before the gate is armed, or arming it turns main red on pre-existing failures."

## Background

A companion issue is fixing a CI defect where the `test` job pipes output through `tee` in a way that masks `cargo test`'s exit code, so the job reports green even when tests fail. Before that gate is armed and starts enforcing failures, `main` must actually be green — otherwise arming the gate immediately turns `main` red on failures that were already there, just invisible.

Diagnosis of three consecutive "successful" `main` runs (`31964057813`, `31961733817`, `31946235305`) shows all three contain `test result: FAILED` for four distinct tests, each with a different, already-understood root cause:

1. **`real_corpus_e2e`** — two sub-tests fail in ~1.3s because the fixture at `crates/core/tests/fixtures/real_corpus_wal/wal/` stores WAL files flat (pre-0.13.0 layout). ADR-0378 moved the WAL root to a per-group layout (`wal/<group>/`) in 0.13.0, and this fixture was never migrated. The test drives `handlers::dispatch` in-process rather than launching the compiled binary, so the startup migration logic that would otherwise upgrade a legacy layout on the fly never runs. This suite has provided no working WAL replay-determinism coverage since 0.13.0 — three releases (0.13.0, 0.13.1, 0.13.2).
2. **`mcp_real_corpus_admin_data_e2e`** — same root cause, but the flat layout is hand-built inline in the test (`crates/service/tests/mcp_real_corpus_admin_data_e2e.rs`) rather than coming from a checked-in fixture. The test also emits `[WAL WARN] replay execution error: ... Found duplicated primary key value ...` warnings whose relationship to the layout problem is not yet confirmed.
3. **`ipc_parity` multibyte test** — a genuine test-isolation race, not a layout issue. `test_knowledge_process_chunk_advisory_threshold_behavior` (`crates/core/tests/ipc_parity.rs:1572`) mutates the process-global env var `LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS` to `50` around one dispatch. `resolve_chunk_text_advisory_max_chars()` (`crates/core/src/handlers.rs:678`) reads that same process-global variable. `test_knowledge_process_chunk_multibyte_chars_use_char_count_not_byte_count` (`ipc_parity.rs:1630`) sends a 4,000-character `chunk_text` and, under `RUST_TEST_THREADS=4`, can run concurrently with the mutating test and read the leaked value of `50`. The victim's own doc comment claims it is race-free because it performs no env var *mutation* — true, but irrelevant, since the hazard is in reading a variable another test is concurrently writing. Introduced by #407 (shipped in 0.13.2).
4. **`migration_binary_tests::binary_migrates_legacy_workspace_on_startup`** — fails in 0.07s, not because migration is broken, but because the test spawns the real compiled binary and the binary exits at startup with `embedder unreachable at startup` before migration ever runs, since no embedder sidecar is available in the CI environment. The migration logic itself is not exercised or shown broken by this failure.

This dead coverage has a direct consequence: tests 1, 2, and 4 are precisely the tests that exercise the WAL migration path, and all three have been silently failing. A working `real_corpus_e2e` would very likely have caught the missing `.wal-generation.json` stamp that caused #428, a regression that reached a release.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Restore WAL replay-determinism coverage (Priority: P1)

As a maintainer relying on CI to catch WAL replay regressions, I need `real_corpus_e2e`'s two sub-tests (`rebuild_and_assert_all_non_determinism_expectations`, `replay_is_deterministic_across_independent_processes`) to actually exercise replay against a realistic corpus, instead of failing before they start.

**Why this priority**: This is the suite whose absence directly let #428 ship. Restoring it is the highest-leverage fix in this issue.

**Independent Test**: Run `cargo test --release -p lcg-core --test real_corpus_e2e` on a clean checkout; both sub-tests pass, and the run genuinely replays the fixture's WAL content (not a trivially-empty fixture) rather than short-circuiting before replay begins.

**Acceptance Scenarios**:

1. **Given** the `real_corpus_wal` fixture, **When** `rebuild_and_assert_all_non_determinism_expectations` runs, **Then** `knowledge_rebuild_from_wal` finds and replays the fixture's WAL files and the test's non-determinism assertions execute (rather than failing with "No WAL files found").
2. **Given** the `real_corpus_wal` fixture, **When** `replay_is_deterministic_across_independent_processes` runs, **Then** two independent replays of the fixture produce assertably-equivalent results.

---

### User Story 2 - Fix the hand-built flat-layout fixture in the MCP admin-data test (Priority: P1)

As a maintainer, I need `mcp_admin_data_operations_over_real_corpus_fixture` to build (or reference) a WAL layout that matches the current post-ADR-0378 on-disk format, so the test's `knowledge_rebuild_from_wal` assertion reflects real behavior.

**Why this priority**: Same root-cause family and same consequence (dead migration-path coverage) as User Story 1; independently fixable and independently valuable.

**Independent Test**: Run `cargo test --release -p lcg-service --test mcp_real_corpus_admin_data_e2e` on a clean checkout; the test passes without panicking at line 470.

**Acceptance Scenarios**:

1. **Given** the test's constructed WAL directory, **When** `knowledge_rebuild_from_wal` is invoked, **Then** it succeeds against the per-group layout.
2. **Given** the test run, **When** the `[WAL WARN] replay execution error: ... Found duplicated primary key value ...` warnings are investigated, **Then** the spec/PR explicitly states whether they share the layout root cause and are resolved by the same fix, or are a separate, pre-existing condition — this determination must be documented, not left implicit.

---

### User Story 3 - Eliminate the env-var race in `ipc_parity` (Priority: P2)

As a maintainer running the test suite with `RUST_TEST_THREADS=4`, I need `test_knowledge_process_chunk_multibyte_chars_use_char_count_not_byte_count` to pass deterministically regardless of which other tests in `ipc_parity.rs` happen to run concurrently.

**Why this priority**: Confirmed product behavior is correct; this is purely a test-isolation defect. Lower urgency than the two WAL-coverage fixes but still required before the CI gate can be armed.

**Independent Test**: Run `cargo test --release -p lcg-core --test ipc_parity` repeatedly (e.g., with `--test-threads=4`, in a loop of at least 10 iterations) on a clean checkout; the multibyte test passes every time.

**Acceptance Scenarios**:

1. **Given** `test_knowledge_process_chunk_advisory_threshold_behavior` and the multibyte test run concurrently under `RUST_TEST_THREADS=4`, **When** the suite runs repeatedly, **Then** the multibyte test's outcome is independent of the advisory-threshold test's env-var mutation.
2. **Given** the fix, **When** a future test in `ipc_parity.rs` sends a `chunk_text` between 51 and 8,000 characters, **Then** it is not exposed to the same hazard (i.e., the fix addresses the isolation mechanism, not just this one victim test).

---

### User Story 4 - Make the migration-on-startup test independent of embedder availability (Priority: P2)

As a maintainer, I need `binary_migrates_legacy_workspace_on_startup` to actually exercise the migration code path in CI, rather than failing before migration runs because no embedder sidecar is reachable.

**Why this priority**: Currently provides zero coverage of the binary-startup migration path — one of the three tests whose absence let #428 ship.

**Independent Test**: Run `cargo test --release -p lcg-service --test migration_binary` on a clean checkout (CI environment, no embedder sidecar running); the test passes and its assertion is shown to depend on a binary that actually reached the migration step.

**Acceptance Scenarios**:

1. **Given** no embedder sidecar is running, **When** the test starts the binary, **Then** the binary either reaches the migration step regardless (e.g., via a stub embedder made available to the test, or because startup no longer requires embedder reachability before migration runs) or the test is restructured so its assertion is not gated behind embedder-dependent startup.
2. **Given** the fix, **When** the test runs, **Then** it asserts on the actual post-migration WAL layout (`.lcg/wal/liminis/`) reached by a binary that completed startup, not on a binary that exited early.

---

### Edge Cases

- If, while implementing User Story 4, it turns out migration genuinely cannot run without embedder reachability without a product-code change (i.e., the "worth considering" design question in the original issue turns out not to be optional), this must be surfaced explicitly per the Assumptions/scope note below rather than folded in silently.
- If, while implementing User Story 2, the duplicated-primary-key WAL warnings turn out to indicate a real (if currently harmless) defect distinct from the layout issue, that must be captured as a follow-up issue rather than silently fixed or silently ignored.
- If a fix for any of the four tests turns out to require more than trivial effort or carries risk of masking a real product defect, quarantining it with `#[ignore]` plus a linked follow-up issue is an acceptable outcome for that specific test, per the Acceptance criteria below — but silent deletion of a failing test is never acceptable.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The `real_corpus_e2e` fixture at `crates/core/tests/fixtures/real_corpus_wal/wal/` MUST be migrated to the per-group WAL layout introduced by ADR-0378, OR the test MUST be changed to drive the actual startup migration path that converts a legacy flat layout — whichever approach keeps the suite exercising real upgrade behavior is preferred over one that merely sidesteps the layout mismatch.
- **FR-002**: `rebuild_and_assert_all_non_determinism_expectations` and `replay_is_deterministic_across_independent_processes` MUST pass on a clean checkout, or be quarantined per FR-007.
- **FR-003**: `mcp_admin_data_operations_over_real_corpus_fixture`'s inline WAL construction MUST be updated to build (or reference) a layout compatible with `knowledge_rebuild_from_wal` post-ADR-0378, and the test MUST pass on a clean checkout, or be quarantined per FR-007.
- **FR-004**: The `[WAL WARN] replay execution error: ... Found duplicated primary key value ...` warnings observed during `mcp_real_corpus_admin_data_e2e` MUST be investigated, and the outcome (same root cause as the layout issue vs. a distinct pre-existing condition, and whether it needs its own follow-up issue) MUST be documented in the PR.
- **FR-005**: The env-var race between `test_knowledge_process_chunk_advisory_threshold_behavior` and `test_knowledge_process_chunk_multibyte_chars_use_char_count_not_byte_count` in `crates/core/tests/ipc_parity.rs` MUST be eliminated such that the outcome of either test does not depend on the other running concurrently, and the fix MUST protect any future test in the same file that sends `chunk_text` between 51 and 8,000 characters, not just today's two tests.
- **FR-006**: `migration_binary_tests::binary_migrates_legacy_workspace_on_startup` MUST pass in an environment with no embedder sidecar reachable, by either providing a stub embedder for the test or restructuring the test/assertion so it does not depend on embedder reachability at startup, or be quarantined per FR-007.
- **FR-007**: Any of the four tests that is not fixed outright MUST instead be quarantined with an explicit `#[ignore]` attribute carrying a reason string, plus a linked GitHub follow-up issue describing what remains — silent deletion or silent skipping without a linked issue is not acceptable.
- **FR-008**: None of the fixes in this issue may change product (non-test) behavior. If, during implementation, any of the four turns out to require a product-code change to fix properly, that requirement MUST be stated explicitly in the PR description rather than folded in quietly — per the original issue, this would constitute a second defect of the same family as #428.
- **FR-009**: `cargo test --release` MUST report zero failures on a clean checkout of the resulting branch.

### Key Entities

- **`real_corpus_wal` fixture**: A checked-in directory of `.jsonl` WAL segment files under `crates/core/tests/fixtures/real_corpus_wal/wal/`, used by `real_corpus_e2e` to exercise replay-determinism against realistic data volume. Currently in the pre-0.13.0 flat layout; per ADR-0378 the current on-disk layout is per-group (`wal/<group>/`).
- **WAL migration path**: The startup logic (exercised when the compiled binary starts against a legacy-layout workspace) that upgrades a flat WAL layout to the per-group layout. Tests 1, 2, and 4 all intend to exercise some portion of this path but currently do not.
- **`LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS`**: A process-global environment variable read by `resolve_chunk_text_advisory_max_chars()` (`crates/core/src/handlers.rs:678`) and mutated by one test in `ipc_parity.rs`, creating a cross-test isolation hazard under parallel test execution.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `cargo test --release` on a clean checkout of `main` (post-merge) reports zero failures.
- **SC-002**: Each of the four originally-failing tests either passes deterministically (verified by at least 10 consecutive local runs for the `ipc_parity` race specifically) or carries an `#[ignore]` attribute with a reason and a linked follow-up issue number.
- **SC-003**: The PR description explicitly states, for each of the four tests, whether the fix was test-only (as expected) or required a product-code change — with no silent scope expansion.
- **SC-004**: The investigation outcome for the duplicated-primary-key WAL warnings in `mcp_real_corpus_admin_data_e2e` is documented in the PR, with a follow-up issue filed if the warnings indicate a distinct, unresolved condition.

## Assumptions

- The companion CI-gate issue (fixing the `| tee`/pipefail masking defect) is tracked and landed separately; this issue's scope is limited to making the underlying tests actually pass, not to arming or modifying the CI gate itself.
- "Clean checkout" for SC-001 and FR-009 means a checkout of this issue's resulting branch merged onto current `main`, run per this project's standard `cargo test --release` local/CI gate — not a from-scratch environment bootstrap.
- Where the issue body offers two possible fix approaches (e.g., migrate the fixture vs. drive the migration path for User Story 1; stub embedder vs. restructure the assertion for User Story 4), selecting between them is a Research/Plan-stage decision, not fixed by this spec — the guiding principle stated in the original issue (prefer whichever keeps the suite exercising the real upgrade/startup path over one that merely sidesteps the mismatch) applies.
- Quarantining via `#[ignore]` (FR-007) is expected to be a fallback for at most one of the four, not the default outcome — the original issue's diagnosis indicates all four have understood, addressable root causes.
- No new test infrastructure (e.g., a shared stub-embedder harness) is assumed to already exist; if User Story 4 requires one, its design is left to the Plan stage.

## Out of Scope

- Arming or modifying the CI gate that currently masks test failures (companion issue).
- Any change to product (non-test) behavior, except as an explicitly-flagged exception per FR-008.
- Broader test-suite reliability work beyond the four tests named in this issue.
- Design work on whether WAL migration should be decoupled from embedder availability at startup — noted in the original issue as "worth considering" but explicitly not required in scope; Plan may choose the stub-embedder route instead without addressing this question.

## Source References

- ADR-0378 (`docs/adr/0378-multi-stream-wal-per-group-directory.md`) — the per-group WAL root layout these fixtures predate.
- #407 — introduced the `ipc_parity` env-var race (shipped in 0.13.2).
- #428 — the shipped regression this dead coverage failed to catch.
- `crates/core/tests/fixtures/real_corpus_wal/wal/` — the flat-layout fixture (User Story 1).
- `crates/service/tests/mcp_real_corpus_admin_data_e2e.rs:470` — the hand-built flat layout and failing assertion (User Story 2).
- `crates/core/tests/ipc_parity.rs:1572,1630` and `crates/core/src/handlers.rs:678` — the env-var race (User Story 3).
- `crates/service/tests/migration_binary.rs:162` — the embedder-gated migration test (User Story 4).
