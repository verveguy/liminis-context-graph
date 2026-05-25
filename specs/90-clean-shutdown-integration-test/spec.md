# Feature Specification: Fix clean_shutdown Integration Test on macOS (lbug hash_index Assertion)

**Feature Branch**: `fabrik/issue-90`
**Created**: 2026-05-25
**Status**: Draft
**Input**: User description: "clean_shutdown integration test fails on macOS arm64 with lbug hash_index assertion after #89 unblocks cargo test on Apple Silicon"

## Background

The `sigterm_produces_clean_exit_and_no_wal_corruption` test in `liminis-graph/tests/clean_shutdown.rs` is the regression guard for issue #71 / PR #72. It verifies that a SIGTERM causes the service to:
1. Exit cleanly (code 0, not killed by signal)
2. Leave the LadybugDB WAL fully checkpointed so the DB can be re-opened without corruption

The test sequence is: spawn binary → wait for socket → call `knowledge_build_indices` (writes vector and FTS index pages to lbug) → send SIGTERM → assert exit code 0 → re-open DB and assert no lbug assertion.

After PR #89 unblocked `cargo test` on macOS arm64 (Apple Silicon), this test fails 100% of the time locally with the following lbug internal assertion on DB re-open:

```
DB re-open failed after clean shutdown — possible WAL corruption:
  Lbug(Assertion failed in file ".../lbug-0.16.1/lbug-src/src/storage/index/hash_index.cpp" on line 483:
   hashIndexStorageInfo.overflowHeaderPage == INVALID_PAGE_IDX)
```

The assertion is a lbug consistency check: on DB open, the hash-index storage metadata must show `INVALID_PAGE_IDX` for the overflow header page (meaning no overflow page has been allocated). Failing means lbug's hash-index metadata was written in a partially-committed state — an overflow page was registered in the index structure but the metadata on disk still reflects the pre-write sentinel.

The test passes in CI (Linux x86_64) and production use on macOS (real Cmd+Q of the Liminis app) works correctly, suggesting the failure is specific to the test's timing or the local macOS source build of lbug.

The current shutdown path is:
- SIGTERM → tokio signal handler fires → `shutdown_notify.notify_one()` → accept loop exits → `AppState` is dropped → `Arc<Db>` refcount reaches zero → lbug C++ destructor fires the WAL checkpoint

There is no explicit flush or checkpoint call before drop; the checkpoint is entirely implicit via the lbug destructor.

Hypotheses ranked by likelihood:
1. **Timing race**: The test sends SIGTERM immediately after receiving the `knowledge_build_indices` response. lbug may still be finishing internal async writes (hash-index overflow page allocation) when SIGTERM is handled. A short sleep would confirm this.
2. **macOS source-build vs. prebuilt difference**: CI uses a prebuilt `liblbug.a`; local macOS uses `LBUG_BUILD_FROM_SOURCE=1` from PR #89. The source build may expose a code path that the prebuilt suppresses or handles differently.
3. **Arc<Db> refcount not reaching zero before checkpoint**: Tokio `spawn_blocking` threads inside aborted tasks may hold a clone of `Arc<Db>` longer than expected on macOS, causing the checkpoint to fire before all write operations complete.
4. **Real shutdown-path bug exposed on macOS**: If the production binary were exercised under the same tight-timing scenario, it might also corrupt the DB.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Developer on macOS arm64 can run the full test suite without false failures (Priority: P1)

A contributor running `cargo test --release` on Apple Silicon after PR #89 sees all integration tests pass, including `sigterm_produces_clean_exit_and_no_wal_corruption`. The test was green when it landed on Linux and must be equally reliable on macOS once this issue is resolved.

**Why this priority**: The test is the primary regression guard for the clean-shutdown feature (#71). A consistently-failing test on a developer platform creates constant noise, blocks local iteration, and risks being silently skipped or removed, eroding the guard entirely.

**Independent Test**: Run `cargo test --release -- --test clean_shutdown` on macOS arm64. The test must pass consistently over multiple runs (no flakiness).

**Acceptance Scenarios**:

1. **Given** a macOS arm64 machine with `LBUG_BUILD_FROM_SOURCE=1` and the fix applied, **When** `cargo test --release -- --test clean_shutdown` is run 5 times in succession, **Then** all 5 runs pass with no lbug assertion failure.
2. **Given** the fix applied, **When** `cargo test --release -- --test clean_shutdown` runs in CI (Linux x86_64), **Then** the test continues to pass with no regression.
3. **Given** the fix applied, **When** the exit code assertion is evaluated, **Then** the binary still exits with code 0 (clean shutdown, not killed by signal).

---

### User Story 2 - Production macOS DB integrity is confirmed (or protected) (Priority: P2)

If hypothesis 4 (real shutdown-path bug) is confirmed, users running the Liminis app on macOS are at risk of DB corruption on process termination. This story ensures that the investigation explicitly rules out or addresses production exposure.

**Why this priority**: The user validated production Cmd+Q on 2026-05-24 evening without corruption, but that test may have had different timing characteristics than the integration test. If a real bug exists, it needs a production-grade fix, not just a test workaround.

**Independent Test**: Post-fix, the DB re-open after a tight-timing SIGTERM (no artificial sleep) must succeed — if the fix is a real shutdown-path improvement rather than only a test timing adjustment, this is satisfied automatically.

**Acceptance Scenarios**:

1. **Given** the investigation determines hypothesis 4 is false (timing race only), **When** the fix is a test adjustment (sleep or post-response delay), **Then** the spec is updated to document explicitly that production is not at risk.
2. **Given** the investigation determines hypothesis 4 is true (real shutdown bug), **When** the shutdown path is fixed, **Then** the DB can be re-opened cleanly even without an artificial sleep in the test.

---

### Edge Cases

- The test uses `LCG_SHUTDOWN_TIMEOUT_MS=2000` (2 s), much shorter than the production default (5 s). A fix that depends on a longer drain window is not acceptable.
- lbug's `knowledge_build_indices` creates three HNSW vector indexes and three FTS indexes; any of these could leave hash-index overflow pages in a partially-committed state.
- The fix must not introduce a busy-wait or unconditional sleep into the production shutdown path — latency on clean exit is a user-facing concern.
- A `#[cfg_attr(target_arch = "aarch64", ignore)]` skip is acceptable only if the investigation confirms the root cause is an upstream lbug bug that cannot be fixed from the Rust side, and that production is not at risk.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The investigation MUST triage all four hypotheses (timing race, source-build divergence, Arc<Db> refcount hold, real shutdown bug) and identify the root cause before any code change is made.
- **FR-002**: The chosen fix MUST cause `sigterm_produces_clean_exit_and_no_wal_corruption` to pass consistently on macOS arm64 with `LBUG_BUILD_FROM_SOURCE=1`.
- **FR-003**: The fix MUST NOT regress the test on Linux x86_64 CI.
- **FR-004**: The fix MUST NOT introduce an unconditional sleep or busy-wait into the production shutdown path.
- **FR-005**: If the fix is a test-side timing adjustment (e.g., a brief sleep between `knowledge_build_indices` response and SIGTERM), the spec MUST be updated to document that production is not at risk and why.
- **FR-006**: If the fix is a shutdown-path change (e.g., explicit lbug flush before Arc<Db> drop), it MUST be gated on an investigation finding that confirms incomplete lbug flushing is the root cause — not introduced speculatively.
- **FR-007**: If the fix is a platform-specific skip (`#[cfg_attr(target_arch = "aarch64", ignore)]` or `#[cfg_attr(target_os = "macos", ignore)]`), a code comment MUST explain the upstream lbug issue, its upstream ticket (if any), and the condition under which the skip should be removed.

### Investigation Approach

The Research stage should perform the following steps (ordered by cost):

1. **Sleep probe**: Add a 100 ms sleep between reading the `knowledge_build_indices` response and sending SIGTERM. If the test passes, the root cause is a timing race (confirms hypothesis 1).
2. **Arc<Db> refcount tracing**: Add a debug log of the `Arc<Db>` strong count at key points during shutdown to verify it reaches 1 before the destructor fires on macOS.
3. **Linux source-build comparison**: Build with `LBUG_BUILD_FROM_SOURCE=1` on Linux x86_64 and run the test without the sleep. If it also fails, the bug is in the lbug source build, not macOS-specific.
4. **No-index variant**: Run the test without the `knowledge_build_indices` call. If it passes, the bug is specific to the hash/vector index pages written by that call.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `sigterm_produces_clean_exit_and_no_wal_corruption` passes on macOS arm64 with `LBUG_BUILD_FROM_SOURCE=1` in 5 consecutive runs with no lbug assertion failure.
- **SC-002**: The same test continues to pass in CI (Linux x86_64) after the fix.
- **SC-003**: The root cause is documented in either a code comment, the spec, or both — future contributors do not need to re-investigate.
- **SC-004**: No unconditional sleep is introduced into the production shutdown path.

## Assumptions

- PR #89's `.cargo/config.toml` (which sets `LBUG_BUILD_FROM_SOURCE=1` on macOS) is the canonical local build configuration; the fix must work with it, not around it.
- CI uses a prebuilt `liblbug.a`, not a source build; a fix that only affects source builds is acceptable if the root cause is confirmed to be a source-build–specific code path.
- The production Cmd+Q shutdown validated on 2026-05-24 went cleanly; this does not rule out hypothesis 4 but reduces its urgency.
- The lbug version is pinned at 0.16.1 for the duration of this fix; upgrading lbug is out of scope unless the investigation reveals the fix requires a newer version.

## Out of Scope

- Upgrading lbug beyond 0.16.1.
- Changes to the IPC protocol or `knowledge_build_indices` handler semantics.
- Adding an explicit `Db::checkpoint()` or `Db::close()` API unless the investigation requires it.
- Fixing any lbug upstream bug (can only work around it from the Rust side).

## Source References

- `liminis-graph/tests/clean_shutdown.rs` — the failing test
- `liminis-graph/src/main.rs` — SIGTERM handler and `drop(state)` shutdown sequence
- `liminis-graph-core/src/db.rs` — `Db::open`, `build_indices_and_constraints`, `create_vector_indexes`
- `liminis-graph-core/src/schema.rs` — `create_fts_indexes`
- `liminis-graph-core/src/app_state.rs` — `ArcSwapOption<Db>` management
- `liminis-graph-core/src/rebuild_job.rs` — abort-and-await at shutdown
- Issue #71 / PR #72 — original clean-shutdown implementation
- Issue #89 — macOS arm64 build fix (`LBUG_BUILD_FROM_SOURCE=1`)
