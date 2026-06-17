# Feature Specification: Fix SIGBUS Crash in Batched WAL Replay — Drop FTS/HNSW Indexes Before Replay, Rebuild After

**Feature Branch**: `fabrik/issue-141`
**Created**: 2026-06-16
**Status**: Draft
**Input**: User description: "After #140 (batched UNWIND WAL replay), a full WAL Rebuild crashes with EXC_BAD_ACCESS / SIGBUS inside LadybugDB's FTS extension during batched SET on FTS-indexed columns. Fix: drop FTS/HNSW indexes before replay, rebuild after."

## Background

After #140 landed batched `UNWIND` WAL replay for performance, a full WAL Rebuild **crashes the `liminis-context-graph` process with `EXC_BAD_ACCESS` / `SIGBUS`** inside LadybugDB's FTS extension, during a batched node-property `SET`. The crash leaves a half-written, corrupt lbug WAL, so the next start enters a degraded state (`lbug_wal_corrupt`) — a crash loop that bricks the workspace until `.lcg/db` is deleted by hand.

This is a **#140 regression**: the pre-batching per-row replay (`LCG_REPLAY_BATCH_SIZE=1`) does not crash. Batching is the trigger; the underlying fault is incremental FTS-index maintenance under the batched update path.

### Crash signature (macOS .ips, reproducible)

```
exceptionType: EXC_BAD_ACCESS (SIGBUS) — "Bus error: 10"
faulting thread:
  lbug::storage::NodeTable::initScanState
  lbug::fts_extension::FTSIndex::deleteFromTermsTable
  lbug::fts_extension::FTSIndex::delete_
  lbug::fts_extension::FTSIndex::update
  lbug::storage::NodeTable::update
  lbug::processor::SingleLabelNodeSetExecutor::set       <-- batched SET node property
  lbug::processor::SetNodeProperty::getNextTuplesInternal
  lbug::processor::ResultCollector::executeInternal
  lbug::common::TaskScheduler::runWorkerThread
```

### Root cause

`RelatesToNode_` carries an FTS index (`CREATE_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact', ['name','fact'])`). During replay, `SET r.name/r.fact = …` updates those indexed columns, driving `FTSIndex::update → delete_ → deleteFromTermsTable`. Under #140's batched `UNWIND` update, that code path makes a bad memory access (`initScanState`) and SIGBUS-es. Per-row updates (the pre-#140 path) do not trigger it.

### Why the drop-then-rebuild approach is correct

For a bulk WAL replay, maintaining secondary indexes incrementally is both unnecessary and — in this case — broken. The standard bulk-load practice is to load data without index maintenance and then build indexes once over the fully-loaded table. This:
- Sidesteps the lbug FTS incremental-update crash entirely
- Is *faster* than incremental maintenance (aligns with the #139/#140 performance goal)
- Applies equally to HNSW vector indexes (consistent with ADR-0047: HNSW creation is a serialised write transaction, better done once)

The fallback of reverting only FTS-indexed-column `SET`s to per-row is not pursued — the drop-then-rebuild approach is strictly superior.

The upstream lbug `FTSIndex::deleteFromTermsTable` SIGBUS is a genuine LadybugDB bug worth reporting upstream, but the drop-then-rebuild approach makes liminis-graph robust regardless of when or whether it is fixed.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Full WAL rebuild completes without crashing (Priority: P1)

An operator triggers a "Rebuild from WAL" (or equivalent workspace reload) on a workspace that contains `RelatesToNode_` edges with `name`/`fact` properties. The rebuild runs to completion with batching enabled, produces a correct database, and does not crash.

**Why this priority**: This is the core regression introduced by #140. A WAL rebuild that crashes before completion leaves the workspace bricked. No other scenario is meaningful until this is fixed.

**Independent Test**: Construct a WAL fixture that includes at least one batched `UNWIND`-style `SET r.name = … SET r.fact = …` mutation on a `RelatesToNode_` edge. Run a full replay with `LCG_REPLAY_BATCH_SIZE > 1` (batching enabled). Assert the replay completes without panic or `SIGBUS`, returns `failed_lines == 0` for the FTS-mutating lines, and the resulting DB has the correct `name`/`fact` values on `RelatesToNode_` edges.

**Acceptance Scenarios**:

1. **Given** a WAL containing batched `SET r.name/r.fact = …` mutations on `RelatesToNode_`, **When** replay runs with `LCG_REPLAY_BATCH_SIZE > 1`, **Then** the process does not crash (no SIGBUS / EXC_BAD_ACCESS) and returns a non-degraded DB.
2. **Given** a replay that drops FTS and HNSW indexes before starting, **When** all WAL mutations have been applied, **Then** the FTS index (`edge_name_and_fact` on `RelatesToNode_`) and the HNSW vector index are rebuilt over the loaded data and are queryable.
3. **Given** a replay that drops then rebuilds the FTS index, **When** a full-text search query runs post-replay, **Then** it returns results consistent with the replayed `name`/`fact` values.
4. **Given** a replay that drops then rebuilds the HNSW index, **When** a vector similarity query runs post-replay, **Then** it returns results consistent with the replayed embeddings.
5. **Given** `LCG_REPLAY_BATCH_SIZE=1` (per-row mode), **When** replay runs, **Then** behavior is unchanged: no index drop/rebuild lifecycle applies (per-row updates do not trigger the FTS bug; this path remains valid as a workaround but is not the target behavior).

---

### User Story 2 — Replay is not slower than the pre-fix baseline (Priority: P1)

An operator performing a full WAL rebuild does not experience a performance regression compared to the batched approach landed in #140. Dropping and rebuilding indexes at the boundary should be net-neutral or faster than incremental index maintenance.

**Why this priority**: #140 was a deliberate performance improvement. The fix must not undo that gain — the drop-then-rebuild approach should be performance-neutral or better, as bulk index construction is cheaper than per-mutation incremental updates.

**Independent Test**: Time a full replay of a WAL with ≥ 1,000 `RelatesToNode_` edges (FTS-indexed) in both before-fix (per-row fallback) and after-fix (drop-then-rebuild batched) configurations. The after-fix run should not be slower than the per-row baseline.

**Acceptance Scenarios**:

1. **Given** a WAL replay with batching enabled and the drop-then-rebuild fix, **When** it completes, **Then** total wall-clock time is not worse than the equivalent per-row (`LCG_REPLAY_BATCH_SIZE=1`) replay of the same WAL.

---

### User Story 3 — A fresh database created by replay is FTS-queryable immediately (Priority: P1)

A workspace rebuilt from WAL is immediately usable for FTS queries without any manual index rebuild step. The index rebuild happens automatically at the end of the replay sequence, before the service returns to the ready state.

**Why this priority**: A replay that produces a database without a working FTS index would be a silent regression — the service would start, appear ready, but silently return no FTS results.

**Acceptance Scenarios**:

1. **Given** a WAL rebuild completes with the fix applied, **When** the service enters the ready state, **Then** the FTS index `edge_name_and_fact` on `RelatesToNode_` is present and populated.
2. **Given** a WAL rebuild completes with the fix applied, **When** the service enters the ready state, **Then** the HNSW vector index on the relevant table is present and populated.

---

### Edge Cases

- **WAL contains no `RelatesToNode_` mutations**: Index drop/rebuild sequence still runs; dropping a non-existent FTS index must be handled gracefully (no-op or explicit existence check before `DROP`).
- **WAL contains only node mutations, no edges**: Same as above — the FTS index on `RelatesToNode_` may never have been created; the drop step must be idempotent.
- **Replay cancelled mid-run**: If the replay is cancelled after indexes are dropped but before the rebuild runs, the resulting DB has no FTS/HNSW indexes. On the next replay attempt, the drop step must be idempotent (index may already be absent). The service should not present a false-ready state with missing indexes.
- **HNSW index creation failure**: If HNSW index creation fails post-replay (e.g., not enough embeddings), this should be logged as a non-fatal warning, not a replay failure — consistent with ADR-0047 and the existing index-creation behavior.
- **FTS index creation failure**: Should be treated consistently — log and continue rather than hard-failing the service startup.
- **Per-row replay path (`LCG_REPLAY_BATCH_SIZE=1`)**: This path bypasses the SIGBUS by not triggering the batched FTS update. The fix must not regress this path: per-row replay should not drop/rebuild indexes (it didn't need to before; changing its behavior would be a scope creep).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Before the WAL mutation loop begins, the replay code MUST drop the FTS index on `RelatesToNode_` (`edge_name_and_fact`) if it exists. The drop MUST be idempotent (no error if the index is already absent).
- **FR-002**: Before the WAL mutation loop begins, the replay code MUST drop the HNSW vector index (if one exists on the relevant table) if it exists. The drop MUST be idempotent.
- **FR-003**: After the WAL mutation loop completes successfully, the replay code MUST rebuild the FTS index on `RelatesToNode_` via `CREATE_FTS_INDEX('RelatesToNode_', 'edge_name_and_fact', ['name','fact'])` (or equivalent). Failure to create the index MUST be logged as a non-fatal warning rather than crashing or failing the replay.
- **FR-004**: After the WAL mutation loop completes successfully, the replay code MUST rebuild the HNSW vector index via the appropriate lbug index creation statement. Failure to create the index MUST be logged as a non-fatal warning rather than crashing or failing the replay.
- **FR-005**: The drop-before / rebuild-after lifecycle MUST apply only to the batched replay path. The per-row replay path (`LCG_REPLAY_BATCH_SIZE=1`) MUST NOT be modified.
- **FR-006**: A regression test MUST be added that: (a) constructs a WAL containing batched `SET r.name = … SET r.fact = …` mutations on `RelatesToNode_` edges, (b) runs replay with batching enabled, (c) asserts no panic occurs and `failed_lines == 0` for those mutations, and (d) asserts the FTS index is present and queryable after replay.
- **FR-007**: Pre-commit gates MUST pass: `cargo fmt --all && cargo test && cargo clippy --release --all-targets -- -D warnings`.

### Key Entities

- **`RelatesToNode_`**: Reified-edge node table in lbug (Kuzu). Carries an FTS index `edge_name_and_fact` over the `name` and `fact` columns. Defined in `liminis-graph-core/src/schema.rs`. This is the table whose FTS incremental update crashes under batched replay.
- **FTS index (`edge_name_and_fact`)**: The full-text search index on `RelatesToNode_[name, fact]`, created via `CREATE_FTS_INDEX(...)`. This index is dropped before and rebuilt after the WAL mutation loop.
- **HNSW vector index**: The approximate nearest-neighbor index used for vector similarity queries. Created once after bulk load per ADR-0047. Dropped before and rebuilt after the WAL mutation loop (same lifecycle as FTS).
- **WAL replay batched path**: The code path in `liminis-graph-core/src/replay.rs` (and/or `handlers.rs`) that batches mutations into `UNWIND`-style Cypher statements when `LCG_REPLAY_BATCH_SIZE > 1`. This is the path that triggers the FTS SIGBUS.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A full WAL rebuild with `LCG_REPLAY_BATCH_SIZE > 1` on a workspace containing `RelatesToNode_` edges completes without `SIGBUS` / `EXC_BAD_ACCESS` (was: reproducible crash on every attempt).
- **SC-002**: After a full WAL rebuild, the FTS index `edge_name_and_fact` on `RelatesToNode_` is present and returns correct results for `name`/`fact` content from the replayed WAL.
- **SC-003**: After a full WAL rebuild, the HNSW vector index is present and returns correct vector similarity results.
- **SC-004**: The regression test added per FR-006 passes: batched replay of FTS-indexed edge mutations does not panic, reports `failed_lines == 0`, and produces a queryable FTS index.
- **SC-005**: The per-row replay path (`LCG_REPLAY_BATCH_SIZE=1`) is unaffected: all existing tests for this path continue to pass.
- **SC-006**: All pre-commit gates are green (`cargo fmt --all`, `cargo test`, `cargo clippy --release --all-targets -- -D warnings`).

## Assumptions

- **A1.** The FTS index (`CREATE_FTS_INDEX`) and HNSW index in lbug support idempotent drop-if-exists semantics, or the implementation can probe for existence before dropping (mirroring the `ALTER TABLE … ADD … IF NOT EXISTS` pattern used in schema migration).
- **A2.** `CREATE_FTS_INDEX` and the HNSW index creation operate over existing table data — i.e., they can be run after rows are present, consistent with the issue description and ADR-0047.
- **A3.** The FTS SIGBUS is only triggered by the batched `UNWIND` update path. Per-row updates remain safe and unaffected.
- **A4.** The HNSW index exists on at least one table involved in replay (the issue references ADR-0047 and the existing behavior for HNSW). The specific table and index name must be confirmed by the Research stage from `schema.rs`.
- **A5.** A replay cancelled mid-run (after index drop, before rebuild) leaves the DB without FTS/HNSW indexes. This is acceptable — the next replay will drop-then-rebuild idempotently. The service MUST NOT enter a ready state with missing indexes after a completed replay.
- **A6.** The crash-safety companion concern (#846 — corrupt WAL leaving degraded mode requiring manual `rm -rf .lcg/db`) is out of scope for this issue. It is tracked separately in #846. This issue addresses only the crash cause, not the recovery behavior after a crash.
- **A7.** The lbug `FTSIndex::deleteFromTermsTable` upstream bug will not be fixed within the timeframe of this issue. The drop-then-rebuild approach makes liminis-graph robust regardless.

## Out of Scope

- **Crash-safety / degraded-mode auto-recovery** (#846): The behavior after a crash (corrupt WAL, degraded mode, need for manual `rm -rf .lcg/db`) is tracked in #846 and is not addressed here. This issue addresses the crash itself, not what happens if the process crashes for any other reason.
- **Per-row replay path changes**: `LCG_REPLAY_BATCH_SIZE=1` is unchanged.
- **Upstream lbug FTS bug report**: Reporting `FTSIndex::deleteFromTermsTable` SIGBUS to upstream LadybugDB is valuable but is not a deliverable of this issue.
- **FTS index on tables other than `RelatesToNode_`**: Only the known-crashing index is in scope. A broader audit of all FTS indexes is a follow-up.
- **ETA or progress reporting for the index rebuild phase**: Index rebuild time is not surfaced via `ReplayProgress`; that is a follow-up to #135.

## Source References

- `liminis-graph-core/src/replay.rs` — batched WAL replay path; `replay_opts`; mutation dispatch
- `liminis-graph-core/src/schema.rs` — `RelatesToNode_` schema; FTS and HNSW index creation
- `liminis-graph-core/src/handlers.rs` — `handle_rebuild_from_wal`; replay orchestration
- Crash reports: `~/Library/Logs/DiagnosticReports/liminis-context-graph-2026-06-16-2132*.ips`
- #139 / #140 — UNWIND batching (regression source)
- #846 — degraded-mode lockout / crash-safety (out of scope here, companion issue)
- #133 / #136 — `RelatesToNode_` schema (the FTS-indexed table involved)
- ADR-0047 — HNSW index creation as serialised write transaction (referenced in issue)
