# ADR-0028: DB→WAL Dump / Compaction Pattern

**Status**: Accepted  
**Date**: 2026-06-22  
**Issue**: #161 — `knowledge_dump_wal`

## Context

The liminis-context-graph service accumulates WAL files incrementally as mutations arrive. Over the lifetime of a production workspace this produces tens of thousands of files spanning multiple dialect eras, making recovery slow. A DB→WAL dump operation that reads the current DB state and emits a fresh compact WAL of idempotent mutations solves both the compaction problem and the dialect-modernisation problem (eliminating legacy `vecf32(...)` syntax from the output).

A Python utility (`graphiti_core/driver/wal_dump.py`) already existed for this purpose but was never ported to the Rust engine. This ADR records the design decisions made during the port.

## Decision

### 1. Separate WalWriter for dump output — never use `state.wal_writer`

The dump handler creates its own `WalWriter` pointed at the caller-supplied target directory. It does NOT share the service's live `state.wal_writer`.

**Why**: The live writer appends to the service's active WAL. Mixing dump output into the live WAL would corrupt the WAL's idempotency invariant (MERGE lines would be interspersed with incremental mutations, making it impossible to replace the old WAL with the dump). The dump WAL must remain a standalone, self-contained snapshot.

**Consequence**: `wal_exec::wal_flush_*` and `conn.drain_mutations()` are NOT called during the dump. Those helpers write to `state.wal_writer`. Dump mutations are written directly to the dump writer via `writer.with_chunk(|w| w.log_mutation(...))`.

### 2. Two-phase ordering: nodes before edges

Phase 1 enumerates all node tables (Entity, Episodic, RelatesToNode_, Community, Saga). Phase 2 enumerates all edge tables (RELATES_TO, MENTIONS, HAS_EPISODE, HAS_MEMBER, NEXT_EPISODE).

**Why**: Edge WAL lines use `MATCH (src:... {uuid: $uuid}), (dst:... {uuid: $uuid}) MERGE ...`. If edges are replayed before their endpoint nodes exist, the MATCH fails and the edge is silently dropped (no error, just zero rows matched). The two-phase guarantee ensures all nodes are present before any edge is written.

**Consequence**: The dump is not a streaming interleaved snapshot — it is always nodes-then-edges in sequence.

### 3. RELATES_TO properties stored on RelatesToNode_ shadow node — Phase 2 is structural only

The Rust write path creates a `RelatesToNode_` shadow node that carries all meaningful RELATES_TO properties (`uuid`, `name`, `group_id`, `fact`, `fact_embedding`, `valid_at`, `invalid_at`, `attributes`, `relation_type`). The actual RELATES_TO relationship edges (`Entity→RelatesToNode_` and `RelatesToNode_→Entity`) carry no data.

**Why**: Phase 2 only needs to re-create the structural connections:
```cypher
MATCH (src:Entity {uuid: $src_uuid}), (rn:RelatesToNode_ {uuid: $rn_uuid}), (dst:Entity {uuid: $dst_uuid})
MERGE (src)-[:RELATES_TO]->(rn) MERGE (rn)-[:RELATES_TO]->(dst)
```
No SET is needed on the relationship — properties are already on the Phase 1 `RelatesToNode_` node.

### 4. Embeddings stored as JSON numeric arrays (`FLOAT[]` not `vecf32(...)`)

All embedding columns (`name_embedding`, `content_embedding`, `fact_embedding`) are stored in WAL params as JSON arrays of floating-point numbers, e.g. `[0.1, 0.2, 0.3, ...]`.

**Why**: The WAL replayer's `json_to_value` function converts a JSON numeric array to `Value::List(LogicalType::Double, ...)`, which lbug coerces to `FLOAT[N]`. No special encoding is required and no `vecf32(...)` FalkorDB syntax is emitted.

**Consequence**: The dump output is free of legacy `vecf32(...)` syntax and does not require the `strip_vecf32` shim during replay. SC-006 verifies this by grepping dump files.

### 5. Optional timestamps use `CASE WHEN $x IS NULL THEN NULL ELSE timestamp($x) END`

Timestamp columns that may be NULL (`expired_at`, `valid_at`, `invalid_at` on RelatesToNode_; `valid_at` on Episodic) are stored in params as JSON null (when absent) or a space-format string (when present). The Cypher template wraps them with `CASE WHEN $x IS NULL THEN NULL ELSE timestamp($x) END`.

**Why**: The space-format timestamp string (e.g. `"2026-01-01 00:00:00"`) cannot bind directly to a TIMESTAMP column — it needs the `timestamp()` Kuzu function to parse it. But calling `timestamp(null)` on a null parameter causes a query error. The CASE guard handles the null branch cleanly.

Required-always-present timestamps (e.g., `created_at`) use `timestamp($x)` directly without a CASE guard.

### 6. Pagination via SKIP/LIMIT with page size 500

All dump queries use `ORDER BY n.uuid SKIP $offset LIMIT $limit` with a page size of 500 rows. After each page, `offset += count`; the loop ends when `count < page_size`.

**Why**: Loading all nodes simultaneously for a 43K-node / 5 GB workspace would require hundreds of MB of memory. SKIP/LIMIT is supported by lbug 0.17 (confirmed at `db.rs:1352`) and provides bounded per-page memory usage (~4 MB for a page of 500 embedding-heavy nodes).

### 7. Write lock held for the entire dump duration

The handler acquires `state.write_lock.write().await` before creating the WalWriter and holds it until the dump completes (including the response construction).

**Why**: The dump produces a point-in-time snapshot of the DB. If concurrent writes are allowed to proceed, the dump could capture a mix of pre- and post-mutation states, producing a WAL that recreates a partially-updated graph. The write lock prevents this by quiescing all `knowledge_process_chunk` calls for the duration.

**Consequence**: `knowledge_dump_wal` is a blocking operation. For large graphs, this can hold the lock for minutes. Callers are expected to be aware of this.

### 8. Partial failure cleanup

On any failure during the dump, `std::fs::remove_dir_all(&target_dir)` is called as a best-effort cleanup before returning the error. The live WAL is never touched.

**Why**: A partial dump directory (some files written, some not) is useless and misleading — it cannot be relied on for recovery. Cleaning it up on failure ensures the caller gets a clean error with no ambiguous partial state.

## Alternatives Considered

**Inline in `handlers.rs`**: The dump logic is ~350 lines including 11 node/edge table handlers. Inlining would make `handlers.rs` exceed 2600 lines. Extracted to `dump.rs` for maintainability.

**Cursor-based pagination**: Instead of SKIP/LIMIT, cursor-based pagination (`WHERE n.uuid > $last_uuid ORDER BY n.uuid ASC LIMIT $size`) avoids any O(N) scan overhead from SKIP. However, SKIP/LIMIT is already proven in the codebase (line 1352), simpler to implement, and adequate for current scale.

**Progress streaming**: The spec explicitly defers `_progress_token` support to a follow-up. The initial implementation returns a synchronous response.

**WAL→WAL compaction**: Merging incremental WAL files without querying the DB would be faster but requires parsing and de-duplicating Cypher templates — significantly more complex and error-prone for mixed-dialect WALs.

## Consequences

- A new `dump.rs` module is the authoritative home for all DB→WAL dump logic.
- `db.rs` gains 11 `pub(crate)` paginated dump query methods and the value accessor functions (`value_as_string`, `value_as_timestamp_str`, etc.) are promoted to `pub(crate)` for use by `dump.rs`.
- `db.rs` also gains `count_mentions_edges` (public) for test verification.
- The Python `service_protocol.py` in the liminis-app repo must be updated to expose `knowledge_dump_wal` to callers (FR-014 — tracked separately, out of scope for this repo).
