# Feature Specification: Reload owns index maintenance — drop FTS before WAL replay, build all indexes once after

**Feature Branch**: `fabrik/issue-146`
**Created**: 2026-06-17
**Status**: Draft
**Input**: WAL reload (`knowledge_rebuild_from_wal` → batched replay) maintains FTS indexes inline on every write, so per-insert cost rises as the index grows and reload throughput degrades super-linearly with graph size. Separately, reload never builds the HNSW vector indexes — they are deferred to a lazy auto-heal on first search miss.

## Background

WAL reload is the primary recovery path for liminis-graph. After the fidelity fixes in #128–#136 and the batching throughput fix in #139, replay is now *correct* and *faster* — but a secondary throughput bottleneck remains: **FTS index maintenance runs inline on every write during reload**, causing per-mutation cost to rise as the FTS index fills.

Measured locally with an identical 12k Entity-create workload (two 6k halves), FTS present vs. dropped:

| | half 1 (first 6k) | half 2 (next 6k) | slowdown h2/h1 |
|---|---|---|---|
| **FTS ON** (current) | 115 mutations/s | **62/s** | **1.85×** |
| **FTS OFF** | 172/s | **169/s** | **1.02× (flat)** |

FTS-ON nearly halves by the second half; FTS-OFF stays flat and is ~2.7× faster by half-2, with the gap widening. Extrapolated to a ~2.5M-mutation WAL, inline FTS maintenance means multi-hour, ever-slowing reloads.

A second problem: reload **never builds the HNSW vector indexes** — they are deferred to a lazy auto-heal triggered by the first search-that-misses-an-index, so the first post-reload query is slow and the graph is effectively unsearchable until auto-heal runs.

The fix is the standard **bulk-load pattern** that databases use for large ingests: drop FTS indexes up front → bulk replay with no inline index maintenance → (re)build all indexes once at the end. This is the same pattern the removed #141 FTS-crash mitigation used; this issue restores it and extends it to also cover vector index construction at reload completion.

**Current code state (verified):**
- `schema::init` (called by `Conn::init_schema` on every open) runs `create_fts_indexes` for 3 FTS indexes (`Entity`/`node_name_and_summary`, `RelatesToNode_`/`edge_name_and_fact`, `Episodic`/`episode_content`) but does **not** create vector indexes. During reload, FTS is therefore present (and maintained inline) while vector is absent.
- `Db::build_indices_and_constraints()` builds **both** vector (`create_vector_indexes`) and FTS (`create_fts_indexes`, idempotent).
- `build_indices_and_constraints` is invoked only via `build_indices_once` (handlers.rs) from the search auto-heal path and the on-demand `knowledge_build_indices` — **not** from `handle_rebuild_from_wal`.
- No drop-FTS helper currently exists; the #141 mitigation was removed during the bound-params refactor (#143). This issue restores it.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Reload throughput is flat in graph size (Priority: P1)

An operator reloading a large workspace from WAL sees per-mutation throughput stay roughly constant as the graph grows, instead of degrading as the FTS index fills.

**Why this priority**: This is the headline performance fix. On a multi-million-mutation WAL the inline-FTS slowdown turns reload into a multi-hour, ever-slowing operation.

**Independent Test**: Replay two equal halves of a large Entity-create workload through the reload path; assert half-2 throughput is within ~10% of half-1 (flat), not ~2× slower.

**Acceptance Scenarios**:

1. **Given** a reload of N mutations, **When** replay runs, **Then** FTS indexes are absent during replay (no inline FTS maintenance).
2. **Given** the same reload, **When** half-2 vs half-1 throughput is compared, **Then** the ratio is ≈1.0 (flat), matching the measured FTS-OFF column above.

---

### User Story 2 — A reloaded graph is immediately searchable (Priority: P1)

After a reload completes, both FTS and HNSW vector indexes exist over the full data, so the first hybrid search returns promptly with no lazy-build stall.

**Why this priority**: Today the first post-reload search pays a full index build (or fails into the missing-index path). Building once at end-of-reload removes that cliff and eliminates the unsearchable window.

**Independent Test**: Run a complete reload, then immediately issue `knowledge_find_entities` and assert it returns results without triggering an on-demand index build.

**Acceptance Scenarios**:

1. **Given** a completed reload, **When** `knowledge_status` (or an index probe) is queried, **Then** all 3 FTS indexes and all 3 vector indexes are present.
2. **Given** a completed reload, **When** the first `knowledge_find_entities`/`knowledge_find_relationships` runs, **Then** it returns results without triggering an on-demand index build.

---

### User Story 3 — Interrupted reload self-heals on next open (Priority: P1)

If reload is interrupted (process killed or cancelled) before the end-of-reload build, the indexes are missing but the graph becomes searchable on next startup via the existing auto-heal path.

**Why this priority**: Dropping indexes up front means a crash mid-reload leaves the graph index-less. Recovery must not require manual intervention.

**Independent Test**: Kill the service after the drop step but before completion; restart; assert that the auto-heal path successfully rebuilds all missing indexes before the first search.

**Acceptance Scenarios**:

1. **Given** a reload interrupted after drop+partial-replay but before the end build, **When** the service restarts and a search runs, **Then** the auto-heal path rebuilds the missing indexes (both FTS and vector) and the search succeeds.
2. **Given** that scenario, **When** the missing index is FTS (not just vector), **Then** `is_missing_index_error` recognizes it and triggers the rebuild — today's auto-heal must cover the FTS-missing case, not only vector.

---

### Edge Cases

- **Fresh DB, first reload**: `init_schema` created the FTS indexes, so the drop has something to drop; after replay the build recreates them. The drop helper must also no-op cleanly if they were already absent.
- **Reload with zero mutations**: drop → no-op replay → build rebuilds empty indexes. Must not error.
- **Reload interrupted between drop and build**: indexes missing → next-open auto-heal (FR-005). The drop is committed, so no half-built FTS state is left torn.
- **Vector index already absent during reload** (normal): build creates it fresh; no drop needed for vector.
- **`#141` interaction**: dropping FTS before replay also removes the inline-FTS `deleteFromTermsTable` SIGBUS path under batched UPDATE — a correctness bonus, not the primary goal.
- **Per-line replay failures**: the end-of-reload build still runs at completion (FR-004), so the surviving data is indexed even when some mutations failed.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Add an idempotent FTS-drop helper (e.g. `schema::drop_fts_indexes`) that drops the 3 FTS indexes via lbug's `CALL DROP_FTS_INDEX('<table>', '<index>')`. It MUST tolerate already-absent indexes (suppress "does not exist" errors, mirroring `create_fts_indexes`'s idempotency).
- **FR-002**: `handle_rebuild_from_wal` MUST, in order: (a) drop the FTS indexes, (b) run the batched replay, (c) on completion call `Db::build_indices_and_constraints()` once to (re)build FTS **and** vector indexes over the fully-loaded data, then mark indices built (`state.indices_built`) so the auto-heal path is a no-op.
- **FR-003**: Index DDL lifecycle (drop/build) MUST live in the reload **orchestration** (`handle_rebuild_from_wal`), NOT inside `WalReplayer` — the replayer stays a pure mutation executor with no index-management responsibility.
- **FR-004**: The end-of-reload build MUST run whenever replay reaches completion, including when individual lines failed/were-skipped (a partially-loaded graph still gets indexed). The build may be skipped only when reload is aborted/cancelled mid-stream — in which case FR-005 covers recovery.
- **FR-005**: The search auto-heal path MUST rebuild **both** FTS and vector indexes when missing. Confirm `is_missing_index_error` matches lbug's FTS-missing-index error text (not only the vector case); extend it if not. (`build_indices_and_constraints` already builds both, so once the error is recognized, recovery is fully covered.)
- **FR-006**: No behavior change to the steady-state (non-reload) write path — normal `knowledge_process_chunk` writes continue to maintain indexes as today. This issue changes only the reload/rebuild path.
- **FR-007**: Tests must cover: (a) a throughput regression/benchmark (the existing `#[ignore]`d `throughput_probe_fts_on_vs_off` style) asserting flat half-2/half-1 under the new reload path; (b) a reload integration test asserting all 6 indexes exist and a search succeeds without a lazy build after reload; (c) an interrupted-reload test asserting next-open auto-heal restores FTS + vector.
- **FR-008**: Pre-commit gate must pass: `cargo fmt --all --check` · `cargo test --release` · `cargo clippy --release --all-targets -- -D warnings`.

### Key Entities

- **`handle_rebuild_from_wal`**: The IPC handler in `handlers.rs` that orchestrates WAL reload. This issue adds index drop before replay and index build after completion.
- **`schema::drop_fts_indexes`**: New idempotent helper that drops the 3 FTS indexes; symmetric counterpart to the existing `create_fts_indexes`.
- **`Db::build_indices_and_constraints()`**: Existing helper that creates both FTS and vector indexes. Called at end-of-reload to build all indexes over the complete dataset.
- **`is_missing_index_error`**: Error-classifier in `handlers.rs` that drives the search auto-heal path. Must recognize both vector-missing and FTS-missing error texts.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Under the new reload path, half-2/half-1 throughput ratio is ≈1.0 (flat), matching the measured FTS-OFF column — vs the current ~1.85×.
- **SC-002**: Absolute reload throughput improves materially on a large WAL (FTS-OFF is ~2.7× faster by half-2 and the gap widens with size).
- **SC-003**: After a completed reload, all 3 FTS and all 3 vector indexes exist; the first hybrid search succeeds without triggering an on-demand build.
- **SC-004**: An interrupted reload recovers on next open via auto-heal (FR-005) — both FTS and vector are restored without manual intervention.
- **SC-005**: Steady-state write path unchanged; full suite + clippy + fmt green.

## Assumptions

- **A1**: lbug 0.17 supports `CALL DROP_FTS_INDEX(table, index)` (the #141 mitigation used it; this restores that usage).
- **A2**: `build_indices_and_constraints()` building both index types in one pass over the full dataset is correct and at least as good as incremental (HNSW/FTS bulk build is standard).
- **A3**: Building all indexes once at end-of-reload is acceptably fast relative to the replay itself (bulk build amortizes far better than per-insert maintenance — the whole point).
- **A4**: Sole-user / maintenance-operation context: search being unavailable during a reload is acceptable.

## Out of Scope

- **"True multi-row `UNWIND` batching"** (the issue's original secondary suggestion). Deferred to its own issue. The inline-`UNWIND` literal construction is exactly what caused the #139 `db.wal` corruption fixed in #143 — a true multi-row UNWIND requires a different bound-list-param design and carries reintroduction risk.
- Changing steady-state write-path index maintenance.
- Building/streaming search availability *during* reload (search is unavailable mid-reload; acceptable).

## Source References

- `liminis-graph-core/src/handlers.rs` — `handle_rebuild_from_wal`, `is_missing_index_error`, `build_indices_once`, `handle_find_entities`, `handle_find_relationships`
- `liminis-graph-core/src/schema.rs` — `create_fts_indexes`, `create_vector_indexes`, `init` — the FTS/vector DDL site where `drop_fts_indexes` will be added
- `liminis-graph-core/src/db.rs` — `Db::build_indices_and_constraints()` — called at end-of-reload
- Issue #141 — original FTS-crash fix / prior drop-rebuild approach this restores
- Issue #143 — bound-params refactor that removed the prior drop-before-replay
- Issue #139 — batch WAL replay via UNWIND (prerequisite; this issue's performance improvements are layered on top)
- Issue #144 — MENTIONS/stub-table schema fixes (separate; correctness, not throughput)
- Issue #145 — community/saga stub tables (separate)
