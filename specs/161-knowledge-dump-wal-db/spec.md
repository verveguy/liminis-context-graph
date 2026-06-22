# Feature Specification: knowledge_dump_wal — DB→WAL Dump / Compaction

**Feature Branch**: `fabrik/issue-161`
**Created**: 2026-06-22
**Status**: Draft
**Input**: Issue #161 — "knowledge_dump_wal: DB→WAL dump / compaction (port wal_dump.py, native FLOAT[] dialect)"

## Background

The liminis-graph engine accumulates WAL files incrementally as mutations arrive. Over the lifetime of a real workspace this produces tens of thousands of files spanning multiple dialect eras (FalkorDB-then-Kuzu), making recovery slow and fragile. A separate Python utility (`graphiti_core/driver/wal_dump.py`) exists for dumping the *current* DB state into a fresh, compact WAL of idempotent `MERGE`/`SET` mutations, but it was never ported to the Rust engine.

Three concrete needs drive this issue:

1. **WAL compaction.** A production workspace has accumulated **43,821 WAL files / ~5 GB** of incremental, mixed-dialect history. A DB-dump produces **one consistent snapshot** — far fewer files, smaller, faster to replay or recover.
2. **Durable cleanup.** Planned in-place graph cleanup (entity dedup via `same_as` corrections, dropping junk relations, fixing entity typing, canonicalizing relation labels) should result in the cleaned state becoming the durable source of truth. A post-cleanup dump makes future Reload/recovery reproduce the cleaned graph rather than replaying the original noise.
3. **Dialect modernization.** The accumulated WAL contains legacy FalkorDB constructs (e.g., `vecf32(...)` embedding syntax). A fresh dump emits **native Kuzu `FLOAT[]`** list literals and clean Cypher, eliminating the need for the `strip_vecf32` legacy shim on replay.

The Rust engine already has `WalWriter` for appending mutations and `WalReplayer` for replaying them. This issue adds a third operation — a DB enumeration that writes into a *new* WAL via the existing writer infrastructure.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — User compacts a large accumulated WAL into a fresh snapshot (Priority: P1)

A workspace has years of incremental WAL history. The user (or liminis-app on their behalf) calls `knowledge_dump_wal` to produce a single fresh WAL directory containing idempotent `MERGE`/`SET` mutations for every node and edge currently in the DB. The caller then archives the old WAL and substitutes the new one, dramatically reducing WAL size and eliminating legacy-dialect replay warnings.

**Why this priority**: Core use case motivating the entire issue. Directly solves the 43,821-file / 5 GB compaction need.

**Independent Test**: Populate a liminis-graph instance with known entity, episode, and relationship data. Call `knowledge_dump_wal`. Count the files in the output directory, count lines, confirm zero `[WAL WARN]`/`[WAL SKIP]` on replay, and confirm node/edge/episode counts match the original DB after a wipe-and-replay.

**Acceptance Scenarios**:

1. **Given** a running service with N nodes and M edges, **When** `knowledge_dump_wal` is called (no `target_dir` provided), **Then** the service creates a fresh WAL directory at the default path, writes all N nodes (all labels, all properties) followed by all M edges, and returns `{success: true, nodes_dumped: N, edges_dumped: M, files_written: K, target_dir: "<path>"}`.
2. **Given** `target_dir` is provided as a parameter, **When** the call is made, **Then** the dump is written to that directory (created if absent); the response includes `target_dir` set to the caller-supplied path.
3. **Given** the old WAL is archived and the dumped WAL replayed against an empty DB, **When** `knowledge_status` is called on the resulting DB, **Then** entity count, edge count, and episode count match the pre-dump DB, FTS returns the same results, and vector search returns comparable results (embeddings preserved as `FLOAT[]`).
4. **Given** the dump completes, **When** the WAL is replayed with `WalReplayer`, **Then** zero `[WAL WARN]` or `[WAL SKIP]` lines appear in logs.

---

### User Story 2 — User dumps after in-place graph cleanup for durable cleaned state (Priority: P1)

After running entity dedup corrections and junk-relation removal directly against the DB, the user calls `knowledge_dump_wal` to snapshot the cleaned graph. The resulting WAL, when replayed, reproduces the cleaned graph rather than re-applying all the original noise and then the corrections.

**Why this priority**: Co-equal with compaction — this is the explicit production motivation stated in the issue.

**Independent Test**: Ingest data, apply corrections (drop entities, fix labels), call `knowledge_dump_wal`, wipe the DB, replay the dump. Assert that the dropped entities are absent, that corrected labels are correct, and that the original noisy data has not re-appeared.

**Acceptance Scenarios**:

1. **Given** N nodes were deleted before the dump, **When** the dump is taken and replayed, **Then** those N nodes do not appear in the replayed DB — the dump reflects DB state at call time, not WAL history.
2. **Given** entity `uuid=X` was renamed (display_name updated) before the dump, **When** replayed, **Then** `X`'s `display_name` in the replayed DB matches the post-rename value.
3. **Given** `group_id` is provided, **When** the call is made, **Then** only nodes and edges whose `group_id` matches are included in the dump; other groups are excluded.

---

### User Story 3 — User verifies round-trip fidelity for disaster-recovery testing (Priority: P2)

A developer or operator wants to confirm that the dump→wipe→replay cycle is loss-free before relying on it in production. The `knowledge_dump_wal` call produces output that the existing `knowledge_rebuild_from_wal` can consume without modifications.

**Why this priority**: Required for trust in the feature, but a derived test scenario — the dump either works or it doesn't; this story is about verification confidence, not new behavior.

**Independent Test**: Use the `knowledge_rebuild_from_wal` streaming path on the dumped WAL directory and confirm the terminal response's `mutations_replayed` matches `nodes_dumped + edges_dumped` from the dump response (accounting for the two-phase node-then-edge structure).

**Acceptance Scenarios**:

1. **Given** a dump has been taken, **When** `knowledge_rebuild_from_wal` is run against the dump's `target_dir`, **Then** it completes with `{success: true}` and zero errors.
2. **Given** a mid-dump failure (simulated by killing the service), **When** the service restarts, **Then** the partial `target_dir` either does not exist or is clearly incomplete (no silent corruption); a subsequent `knowledge_dump_wal` call succeeds cleanly.

---

### Edge Cases

- **Empty graph**: `knowledge_dump_wal` on a DB with zero nodes and zero edges returns `{success: true, nodes_dumped: 0, edges_dumped: 0, files_written: 0}` — not an error.
- **`target_dir` already exists and contains files**: The service returns an error rather than overwriting or appending to a pre-existing dump directory. Callers must supply a clean path or omit `target_dir` to use the service-generated default.
- **Write-active graph**: A `knowledge_process_chunk` call in flight when `knowledge_dump_wal` is called — the dump waits to acquire the write lock; it does not interleave with an in-progress write.
- **Large graph (10K+ nodes / 80K+ edges)**: The service must not accumulate the entire result set in memory; it must enumerate and flush incrementally.
- **Embedding-less nodes**: Some nodes may not have an embedding (not yet indexed). The dump must handle `null` or absent embedding fields without panicking.
- **Partial dump failure**: If the dump fails after writing some files (e.g., disk full, I/O error), the partial `target_dir` should be cleaned up and an error returned. The existing WAL is untouched.
- **`group_id` that matches zero documents**: Returns `{success: true, nodes_dumped: 0, edges_dumped: 0, ...}` — not an error.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A new `knowledge_dump_wal` IPC method MUST be registered in the handler-dispatch table in `handlers.rs`, following the JSON-RPC 2.0 pattern established by prior Tier methods.
- **FR-002**: The method MUST accept two optional parameters: `target_dir` (string path for the output WAL directory) and `group_id` (string to limit the dump to one group).
- **FR-003**: When `target_dir` is omitted, the service MUST choose a default path of `{workspace_root}/.lcg/wal-compacted/` and include the resolved path in the response.
- **FR-004**: When `target_dir` is provided and already contains files, the service MUST return an error without writing any output. When `target_dir` is provided and does not exist, the service MUST create it.
- **FR-005**: The dump MUST be a two-phase operation: Phase 1 enumerates all nodes (or all nodes matching `group_id`) and emits `MERGE (n:<labels> {uuid: $uuid}) SET n = $props` WAL lines; Phase 2 enumerates all edges and emits `MATCH`-by-endpoint-uuid + `MERGE`/`SET` WAL lines. Phase 1 MUST complete before Phase 2 begins, so that edge `MATCH` clauses can resolve during replay.
- **FR-006**: The dump MUST write into the target directory via the existing `WalWriter` — do NOT append to the service's live WAL.
- **FR-007**: All embeddings in the output WAL MUST be emitted as native Kuzu/lbug `FLOAT[]` list literals (e.g., `[0.1, 0.2, ...]`). The `vecf32(...)` FalkorDB syntax MUST NOT appear in any output line.
- **FR-008**: Every property column declared in `schema.rs` for each node/edge type MUST be preserved in the dump (e.g., MENTIONS: `uuid`, `created_at`; RelatesToNode: `uuid`, `group_id`, `fact`, `name`, `episode_uuids`, `valid_at`, `created_at`, `name_embedding`). Missing declared columns are a correctness defect.
- **FR-009**: The dump MUST acquire the service-level write lock for its entire duration, preventing concurrent write mutations from interleaving with the snapshot.
- **FR-010**: The enumeration MUST stream results incrementally — it MUST NOT load all nodes or all edges into memory simultaneously. Batching is acceptable as long as per-batch memory is bounded.
- **FR-011**: On successful completion the method MUST return `{success: true, nodes_dumped: <int>, edges_dumped: <int>, files_written: <int>, target_dir: "<resolved path>"}`.
- **FR-012**: On any failure during the dump, the service MUST attempt to clean up the (partial) `target_dir`, leave the live WAL untouched, and return a JSON-RPC error.
- **FR-013**: The output WAL MUST be replayable by the existing `WalReplayer` (`knowledge_rebuild_from_wal`) without modification. The replay MUST produce zero `[WAL WARN]` or `[WAL SKIP]` log lines.
- **FR-014**: The method MUST be added to the Python-side `service_protocol.py` so that liminis-app callers can invoke it via the standard protocol layer.
- **FR-015**: An IPC parity test MUST be added to `crates/core/tests/ipc_parity.rs` covering the `knowledge_dump_wal` request/response shape.
- **FR-016**: A round-trip integration test MUST be added that: (a) populates a test DB with known data, (b) calls `knowledge_dump_wal`, (c) wipes the DB, (d) replays the dump via `WalReplayer`, (e) asserts node count, edge count, and episode count match.
- **FR-017**: The empty-graph case (zero nodes, zero edges) MUST return `{success: true, nodes_dumped: 0, edges_dumped: 0, files_written: 0, target_dir: "..."}` without error.

### Key Entities

- **Dump WAL directory**: A freshly created directory (at `target_dir` or the default path) containing `.jsonl` WAL files produced by the dump. Layout follows the same naming convention as the live WAL (`WalWriter` handles file naming/rotation). This directory is inert until the caller decides to use it; the service does not automatically swap it into the live WAL position.
- **Node dump line**: A WAL line in the format `MERGE (n:<labels> {uuid: $uuid}) SET n = $props`, where `$props` includes all schema-declared columns and any additional properties present on the node, with embeddings as `FLOAT[]`.
- **Edge dump line**: A WAL line that first MATCHes both endpoints by `uuid`, then MERGEs the relationship and SETs its properties.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A round-trip test (dump → wipe `.lcg/db` → replay) on a test graph with ≥ 100 nodes, ≥ 200 edges, and ≥ 50 episodes produces a DB with identical entity count, edge count, and episode count, and FTS returns the same top result for a known query.
- **SC-002**: Replaying the dumped WAL against an empty DB produces **zero** `[WAL WARN]` or `[WAL SKIP]` log lines.
- **SC-003**: The `files_written` count in the dump response is dramatically smaller than the original WAL file count for a workspace with hundreds or thousands of incremental WAL files.
- **SC-004**: `knowledge_dump_wal` on an empty graph (0 nodes, 0 edges) returns `{success: true, nodes_dumped: 0, edges_dumped: 0, files_written: 0}` and does not panic or error.
- **SC-005**: On a test graph with 10,000+ nodes, the dump completes without the process's RSS growing unboundedly (memory usage stays within ~2× the baseline, as measured before and during the dump).
- **SC-006**: The dumped WAL contains no occurrences of `vecf32(` in any output line (verified by grepping the output directory).
- **SC-007**: The `knowledge_dump_wal` method is callable from a Python client using the `service_protocol.py` interface without any code changes to the client beyond adding the method name.

## Assumptions

- The existing `WalWriter` can be instantiated pointed at an arbitrary output directory, not just the live WAL directory. If it cannot, making it configurable is in scope for this issue.
- The existing node/edge enumeration in `db.rs` (`get_nodes_by_group` / `get_edges_by_group` or equivalent full-graph variants) can be adapted for streaming enumeration. If only batched queries exist, pagination via cursor or offset is acceptable as long as per-page memory is bounded.
- The service-level write lock from prior ADRs (ADR-042 and related) is available and sufficient to quiesce concurrent writes for the duration of the dump. Read-only queries may proceed concurrently during the dump.
- The `target_dir` is on a local filesystem accessible to the service process. Network filesystems or object storage are out of scope.
- `group_id` filtering, if provided, is applied at the DB query level (not post-filter in memory) to avoid loading the full graph for a partial dump.
- The dump response does NOT need to include episode nodes/edges as a separate category — episodes are nodes with appropriate labels and will be captured in Phase 1 (node dump) along with entity nodes.
- Progress streaming (`_progress_token`) is **not** required for this method in the initial implementation — the caller is expected to wait for the dump to complete synchronously. Progress reporting may be added as a follow-up.

## Out of Scope

- Automatically swapping the dumped WAL into the live WAL position — the caller (liminis-app) owns that workflow.
- WAL-to-WAL compaction (merging incremental WAL files without querying the DB) — this issue is DB→WAL, not WAL→WAL.
- Streaming progress lines during the dump (`_progress_token` support) — synchronous response only; streaming can be added later.
- Scheduled or automatic compaction triggered by WAL file count or size thresholds — future work.
- Cross-group dumps (dumping multiple groups in one call) — each call covers either all groups or one specified group.
- Importing/consuming a foreign dump directory produced by the Python `wal_dump.py` — the Python tool and the Rust tool may differ in minor formatting; cross-tool round-tripping is not a correctness requirement, only Rust→`WalReplayer` round-tripping is.

## Source References

- Port from: `graphiti/graphiti_core/driver/wal_dump.py` — `dump_wal()`, `_dump_nodes()`, `_dump_relationships()`
- `crates/core/src/wal.rs` — `WalWriter` (mutation serialization)
- `crates/core/src/wal_exec.rs` — WAL execution path
- `crates/core/src/db.rs` — node/edge enumeration (`get_nodes_by_group`, `get_edges_by_group`)
- `crates/core/src/schema.rs` — column parity reference (canonical list of declared properties per node/edge type)
- `crates/core/src/handlers.rs` — IPC handler registration
- `crates/core/src/legacy_wal.rs` — `strip_vecf32` shim (exists for legacy WALs; new dump MUST NOT need this)
- Python side: `graphiti_service.py` `service_protocol.py` — protocol layer to update
- `crates/core/tests/ipc_parity.rs` — IPC parity test suite to extend
- ADR-0051 — episode-cursor WAL resume (context for WAL layout)
- Issue #29 — Tier 2 WAL admin (`knowledge_rebuild_from_wal`, `knowledge_prepare_checkpoint`) — related; the dump output is consumed by `rebuild_from_wal`
