# Feature Specification: `indices_built` is not set after runtime recovery, so `knowledge_status` under-reports readiness

**Feature Branch**: `fabrik/issue-297`
**Created**: 2026-07-30
**Status**: Draft
**Input**: User description: "indices_built is not set after runtime recovery — knowledge_status under-reports readiness"

## Background

`crates/service/tests/mcp_real_corpus_admin_lifecycle_e2e.rs` performs runtime recovery against the golden real-corpus fixture — once via `knowledge_recover` (strategy `drop_lbug_wal`), once via `knowledge_recover_full` — and in both cases asserts that a subsequent `knowledge_status` call reports `indices_built == true`. It reports `false` in both cases, and the test has failed on every push to `main` since the day it was added (2026-07-26).

The indices themselves are fine: searches against them succeed and match the fixture's golden expectations. What's missing is the flag update on `AppState.indices_built`, which today is only ever stored by `bootstrap_app_state` at startup (ADR-0036), by the `knowledge_build_indices` handler, by `knowledge_rebuild_from_wal`, and by the search-time auto-heal helper (`build_indices_once`, triggered only when a search hits a genuinely missing-index error). None of the runtime recovery paths reachable through `knowledge_recover` or `knowledge_recover_full` touch it, and — critically — a search against indices that are already valid succeeds directly without ever tripping that auto-heal path, so there is no fallback that masks the gap.

`knowledge_recover` dispatches to one of three strategies, which differ in whether they touch indices at all:

- **`rebuild_from_workspace_wal`** drops the FTS/HNSW indices and explicitly rebuilds them (calls `build_indices_and_constraints`), then never records the outcome on `AppState`.
- **`drop_lbug_wal`** and **`restore_from_backup`** reopen an existing, already-indexed database state (respectively: the last lbug checkpoint after discarding a corrupt WAL tail; a `.pre-*-backup` snapshot) without touching indices at all. There is no explicit "build" step on these paths for a flag update to hook onto — the indices were never invalidated, they just were never observed as valid either.

`knowledge_recover_full` calls `run_full_recovery_sequence` (`crates/core/src/recovery.rs`), which does call `build_indices_and_constraints` (`recovery.rs:255`) and already captures the outcome in `RecoveryReport.indexes_rebuilt` — that field is returned in the JSON response body but never fed into `AppState.indices_built`.

So across all four runtime recovery paths, `knowledge_status` keeps reporting `indices_built == false` after a successful recovery: in the `rebuild_from_workspace_wal` and `knowledge_recover_full` cases the indices were just rebuilt and the outcome is known but discarded; in the `drop_lbug_wal`/`restore_from_backup` cases the indices were never invalidated in the first place, but the flag has no way to learn that.

This was known in the narrower, startup-only case. ADR-0034 §5 recorded it on 2026-07-16 as *"a separate, pre-existing, functionally harmless inconsistency (a subsequent search succeeds directly, since the indices genuinely exist and never trip the auto-heal path) — flagged here as a candidate follow-up, not fixed in this change."* ADR-0036 (#208) then closed the **startup** half by having `bootstrap_app_state` track an `indices_ready` flag across the direct-open and post-recovery-at-startup paths. The **runtime** `knowledge_recover` / `knowledge_recover_full` paths remained out of scope and are what this issue covers — across all four strategies, not only the single `run_full_recovery_sequence` call site the original bug report focused on.

### Why it is worth fixing rather than relaxing the test

"Functionally harmless" understates it now that the field is a documented readiness signal:

- The README documents `indices_built` as reflecting real index state, and states it is normally `true` from the first `knowledge_status` call onward.
- `knowledge_status` is the readiness check clients use. Under-reporting invites a client to conclude the graph is unusable, or to issue a redundant `knowledge_build_indices` — which is exactly the manual recovery step reported in #203's thread. Rebuilding HNSW over a large graph is expensive to do for no reason.
- The flag is wrong in the *safe* direction today, but it is still wrong, and a status field that lies is the class of problem PR #294 spent a day correcting elsewhere.

The test's expectation is correct as written. The product should match it.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Status is accurate after a runtime recovery (Priority: P1)

An operator recovers a degraded service — via any `knowledge_recover` strategy, or via `knowledge_recover_full` — then checks `knowledge_status` to confirm the graph is usable.

**Why this priority**: This is the core defect. A readiness signal that lies after the exact operation an operator uses to restore service is the highest-value thing to fix; everything else in this issue (ADR bookkeeping, the failing e2e assertion) follows from getting this right.

**Independent Test**: Can be verified without the full e2e fixture by driving each recovery path (`drop_lbug_wal`, `rebuild_from_workspace_wal`, `restore_from_backup`, `knowledge_recover_full`) against a small test database and asserting `AppState.indices_built` (or `knowledge_status`'s `indices_built` field) afterward.

**Acceptance Scenarios**:

1. **Given** a service that has completed a successful runtime recovery via any `knowledge_recover` strategy or `knowledge_recover_full`, **When** `knowledge_status` is called, **Then** `indices_built` is `true`.
2. **Given** a recovery that fails before its index build completes (for the strategies that perform one), **When** `knowledge_status` is called, **Then** `indices_built` is `false` — the flag must reflect the real outcome, not be set optimistically.

---

### User Story 2 - The e2e suite passes on `main` (Priority: P1)

A contributor pushes to `main` and expects the real-corpus e2e suite to be green, since it is the strongest end-to-end signal that recovery genuinely restores a usable graph.

**Why this priority**: The failing assertion is the concrete, checkable proof that User Story 1 is satisfied. Without it passing, there's no automated guard against this regressing again.

**Independent Test**: Run `mcp_real_corpus_admin_lifecycle_e2e` and confirm both `indices_built` assertions (post `knowledge_recover`, post `knowledge_recover_full`) pass without modification to the test itself.

**Acceptance Scenarios**:

1. **Given** `main` after this change, **When** `real-corpus-e2e` runs, **Then** `mcp_real_corpus_admin_lifecycle_e2e` passes without modifying its `indices_built` assertions.

---

### Edge Cases

- Recovery that ends in the fallback path (`wal_auto_recovery` phase `"fallback_triggered"`) must not report indices as built unless that fallback path itself produces a database with valid indices.
- Concurrent `knowledge_status` during recovery should not observe a transiently-true flag before the recovery (and any index build it performs) has actually completed.
- `knowledge_build_indices` sets the flag `false` then `true` around its work (`handlers.rs`, function `handle_build_indices`); recovery paths that explicitly rebuild indices should follow the same ordering discipline rather than inventing a second convention.
- `drop_lbug_wal` and `restore_from_backup` never call `build_indices_and_constraints` — their indices come from the reopened checkpoint or backup file, not a fresh build. `indices_built` must still become `true` on their success path; the fix cannot be scoped to "wherever `build_indices_and_constraints` is called" without missing these two strategies, which is exactly the gap the failing test's first assertion (`drop_lbug_wal`) exercises today.
- A `knowledge_recover` call that fails outright (DB doesn't reopen, no backup file found, etc.) must leave `indices_built` unchanged at its prior value — a failed recovery does not make the *previous* state's indices any less valid or any more valid.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A successful runtime recovery — via any `knowledge_recover` strategy (`drop_lbug_wal`, `rebuild_from_workspace_wal`, `restore_from_backup`) or via `knowledge_recover_full` — MUST result in `AppState.indices_built` being `true` once the recovered database is confirmed to be a healthy, currently-indexed graph.
- **FR-002**: The flag MUST reflect the real outcome, not be set optimistically:
  - For the two paths that explicitly rebuild indices (`rebuild_from_workspace_wal`, and `knowledge_recover_full`'s `run_full_recovery_sequence`), if `build_indices_and_constraints` fails or the recovery aborts before reaching it, `indices_built` MUST remain `false`.
  - For the two paths that reopen an existing, already-indexed checkpoint or backup without rebuilding (`drop_lbug_wal`, `restore_from_backup`), if the reopen or schema-init step fails, the recovery call itself fails and `indices_built` MUST NOT be set to `true`.
- **FR-003**: The fix MUST cover every runtime recovery path that currently leaves `AppState.indices_built` stale after producing a healthy, indexed database — not only the one the failing test's first assertion exercises. At minimum this means all four paths named in FR-001: `recover_rebuild_from_workspace_wal`, `recover_drop_lbug_wal`, `recover_restore_from_backup` (all reached via `handle_knowledge_recover`), and `run_full_recovery_sequence` (reached via `handle_knowledge_recover_full`). For each, the PR MUST state whether it now updates the flag and why — including, for `drop_lbug_wal`/`restore_from_backup`, how the flag is set to `true` correctly despite no explicit index-build call occurring on that path.
- **FR-004**: `mcp_real_corpus_admin_lifecycle_e2e`'s `indices_built` assertions (both the one following `knowledge_recover` and the one following `knowledge_recover_full`) MUST NOT be weakened or removed — they assert correct behaviour.
- **FR-005**: ADR-0034 §5's annotation and ADR-0036 MUST be updated to record that the runtime recovery paths are now covered, so the ADR trail does not still describe this as an open gap.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `mcp_real_corpus_admin_lifecycle_e2e` passes on `main`.
- **SC-002**: A test asserts `indices_built` is `false` after a recovery whose index build fails (for a path that performs one), so the fix cannot be implemented as an unconditional `store(true)` on every recovery path.
- **SC-003**: Every runtime recovery path identified in FR-003 is documented in the PR as either updating the flag (and how) or deliberately not doing so (and why).

## Assumptions

- `run_full_recovery_sequence`'s existing `build_indices_and_constraints()?` call is correct and stays; this issue is about the flag, not the build.
- For `drop_lbug_wal` and `restore_from_backup`, a successfully reopened checkpoint/backup is assumed to carry valid, current indices — these strategies do not independently re-verify index integrity, matching their existing behaviour for the graph data itself (they already trust the reopened file's contents).

## Out of Scope

- The startup path, already handled by ADR-0036.
- Why `real-corpus-e2e` failing for 24 consecutive runs went unnoticed — filed separately.
- Verifying index *integrity* (as opposed to *presence*) on the `drop_lbug_wal`/`restore_from_backup` paths — the fix reports the flag accurately given the existing trust model for reopened checkpoints/backups; it does not add new validation of those checkpoints/backups.

## Source References

- `crates/service/tests/mcp_real_corpus_admin_lifecycle_e2e.rs` — the failing e2e test (`indices_built` assertions after `knowledge_recover` and `knowledge_recover_full`).
- `crates/core/src/recovery.rs` — `run_full_recovery_sequence`, `RecoveryReport.indexes_rebuilt`.
- `crates/core/src/handlers.rs` — `handle_knowledge_recover`, `handle_knowledge_recover_full`, `recover_drop_lbug_wal`, `recover_rebuild_from_workspace_wal`, `recover_restore_from_backup`, `handle_build_indices`, `handle_rebuild_from_wal`.
- `crates/core/src/app_state.rs` — `AppState.indices_built`, `build_indices_once` (search-time auto-heal).
- `docs/adr/0034-observable-index-build-outcome.md` §5, `docs/adr/0036-eager-index-build-at-startup.md` — prior art on the startup half of this gap.
- Issue #203 — prior report of the manual `knowledge_build_indices` workaround this issue aims to make unnecessary.
- Issue #208 — landed ADR-0036 (startup-path fix).
