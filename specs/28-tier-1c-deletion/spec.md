# Feature Specification: Tier 1c — Deletion Methods (delete_by_source, delete_chunk_episode, clear_all)

**Feature Branch**: `fabrik/issue-28`
**Created**: 2026-05-22
**Status**: Draft
**Input**: Issue #28 — "Tier 1c: deletion methods (delete_by_source, delete_chunk_episode, clear_all)"

## Background

Tier 1c of the liminis-graph ↔ liminis integration. Three WRITE methods that let the client remove content from the graph — either targeted (one chunk's episode, all episodes for one file) or wholesale (clear everything). The Python reference implementations live in `graphiti_service.py` lines 1872–1960 and 2517–2540.

These are the most destructive methods in the API surface. They must acquire the writer lock (ADR-042), must be idempotent, and `clear_all` must refuse to act without explicit confirmation. All three must return structured JSON-RPC errors on failure — never crashing the daemon, never leaving the graph in a partially-deleted state.

Without these methods, deleted files continue contributing stale entities that pollute search and dedup results, stale chunk episodes accumulate in the corrections workflow, and full-graph rebuilds have no clean way to wipe and restart.

**Blocked by**: Tier 1a (#26) — shares the IPC handler-dispatch pattern and error-shape conventions.

**Constitution gates**:
- Principle I (IPC Parity): unmodified Python clients must be able to call all three methods without code changes.
- Principle IV (WAL Is Authoritative): `clear_all` must delete the WAL directory in addition to DB files, so the next replay does not reconstruct the old graph.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Delete One File's Worth of Content (Priority: P1)

When a user removes or moves a workspace file, liminis-app calls `knowledge_delete_by_source` to clear the entities and relationships that file contributed to the graph. There are 3 call sites in liminis-app. Without this method, deleted files keep contributing stale entities, polluting search and dedup.

**Why this priority**: Most common deletion path; directly blocks workspace file management. Stale-entity pollution degrades every search until addressed.

**Independent Test**: Index two chunks from `docs/a.md` (producing N episodes + entities) and one chunk from `docs/b.md`. Call `knowledge_delete_by_source` with `source_file=docs/a.md`. Assert all `docs/a.md` episodes are gone, the `docs/b.md` episode remains, and the returned `deleted_count` matches the number of deleted episodes.

**Acceptance Scenarios**:

1. **Given** episodes whose `source_description` equals `docs/a.md` OR starts with `docs/a.md:` (the chunk-id prefix convention), **When** client sends `knowledge_delete_by_source` with `source_file=docs/a.md`, **Then** all matching episodes are deleted and the response is `{success: true, source_file, deleted_count, deleted_uuids: [...]}`.
2. **Given** `source_file` is missing or empty, **When** the call is made, **Then** a structured JSON-RPC error is returned and no deletions are performed.
3. **Given** no episodes match the source, **When** the call is made, **Then** `{success: true, deleted_count: 0, deleted_uuids: []}` is returned — not an error.
4. **Given** a `group_ids` filter is supplied, **When** the call is made, **Then** deletion is scoped to episodes in those groups only (matches Python behaviour).

---

### User Story 2 — Delete One Chunk's Episode (Priority: P2)

When the indexing queue replaces a chunk that previously failed validation, liminis-app calls `knowledge_delete_chunk_episode` to clean up the stale episode. There is 1 direct call site, and the method is used internally by the corrections workflow.

**Why this priority**: Required for correct behaviour in the corrections workflow. A second priority because it operates on a finer scope than `delete_by_source` and is triggered less frequently.

**Independent Test**: Process a chunk via `knowledge_process_chunk` with `chunk_id=foo`. Verify one episode exists for that chunk. Call `knowledge_delete_chunk_episode` with `chunk_id=foo`. Verify the episode is gone.

**Acceptance Scenarios**:

1. **Given** at least one episode whose chunk identifier is `foo`, **When** the client sends `knowledge_delete_chunk_episode` with `chunk_id=foo`, **Then** all matching episodes are removed and the response is `{success: true, chunk_id, deleted_count, deleted_uuids: [...]}`.
2. **Given** `chunk_id` is missing or empty, **When** the call is made, **Then** a structured JSON-RPC error is returned.
3. **Given** no episodes match the `chunk_id`, **When** the call is made, **Then** `{success: true, deleted_count: 0, deleted_uuids: []}` is returned — idempotent, not an error.
4. **Given** multiple revisions of the same chunk exist (per Tier 1a's append-on-revision contract), **When** the call is made, **Then** all revisions are deleted, not just the most recent.

---

### User Story 3 — Clear Entire Graph (Priority: P3)

The corrections and rebuild flows occasionally need to nuke the graph and start over. There is 1 call site, but it is critical. The Python implementation deletes DB files on disk AND wipes the WAL — both must happen; otherwise the next replay reconstructs the old graph.

**Why this priority**: Low frequency but high impact. The method is irreversible by design and requires explicit confirmation, which reduces the urgency relative to the targeted methods.

**Independent Test**: Populate the DB with known entities. Call `knowledge_clear_all` with `confirm: true`. Verify entity/edge/episode counts are zero, DB files are absent or empty, WAL files are gone, and the service is still alive and accepting new requests.

**Acceptance Scenarios**:

1. **Given** `confirm: true` and a populated DB, **When** the client sends `knowledge_clear_all`, **Then** the service deletes the DB files, removes the WAL directory, reinitializes a fresh empty DB, and returns `{success: true, message: ...}`.
2. **Given** `confirm` is absent, false, or any non-true value, **When** the call is made, **Then** a structured error is returned (`"Must set 'confirm' to true to clear graph"`) and no deletion happens.
3. **Given** the service successfully clears, **When** a subsequent `knowledge_status` is called, **Then** `entity_count`, `edge_count`, and `episode_count` are all 0 and `wal` reports either absent or empty.
4. **Given** reinitialization fails after the delete phase, **When** the call returns, **Then** the response is an error (not success) and a recovery hint is included in the message — the service must not claim success if it cannot serve subsequent requests.

---

### Edge Cases

- `delete_by_source` while a write is in progress must serialize via the writer lock; no partial deletion is visible to concurrent readers.
- `delete_chunk_episode` for a chunk that has both an episode AND extracted entities deletes only the episode; the entities remain (they may be connected to other episodes). Callers must understand this orphan-entity outcome — it must be documented in the method's interface description.
- `clear_all` invoked while another write is in progress serializes via the writer lock (matching Python's serialization behaviour). It does not fail with a busy error.
- `clear_all` invoked while a WAL replay is in progress must coordinate with replay state or fail explicitly; the service must not be left in a half-cleared state.
- `clear_all` on a DB that was never opened (cold start before initialize) must not crash; it is treated as a no-op success.
- `source_file` containing path separators: the matching rule is exact equality OR the `source_description` starts with `source_file + ":"`. Paths are NOT normalized — this matches Python convention. Callers are responsible for supplying paths in the same form they were stored.

## Requirements *(mandatory)*

### Functional Requirements

#### Common

- **FR-001**: All three methods are WRITE methods — they MUST acquire the writer lock (ADR-042 reader/writer split) before performing any mutation.
- **FR-002**: All three methods MUST return errors as JSON-RPC error objects; they must never crash the daemon or leave the graph in a partially-deleted state.
- **FR-003**: All three methods MUST be idempotent — a second call with the same params on an already-cleared target returns success with zero counts, never an error.

#### delete_by_source

- **FR-004**: `knowledge_delete_by_source` accepts `source_file` (string, required) and `group_ids` (optional list of strings).
- **FR-005**: An episode matches when its `source_description` equals `source_file` exactly OR starts with `source_file + ":"` (the chunk-id prefix convention from `process_chunk`).
- **FR-006**: Response shape is `{success: true, source_file: <string>, deleted_count: <int>, deleted_uuids: [<uuid>, ...]}`.
- **FR-007**: When `group_ids` is provided, only episodes in those groups are considered for deletion.

#### delete_chunk_episode

- **FR-008**: `knowledge_delete_chunk_episode` accepts `chunk_id` (string, required) and `group_ids` (optional list of strings).
- **FR-009**: All episodes whose chunk identifier matches `chunk_id` are deleted — including all revisions accumulated under the append-on-revision contract from Tier 1a.
- **FR-010**: Response shape is `{success: true, chunk_id: <string>, deleted_count: <int>, deleted_uuids: [<uuid>, ...]}`.
- **FR-011**: Orphan entities (entities connected only to the deleted episodes) are NOT automatically removed — this matches Python behaviour. The method's interface description must document this outcome.

#### clear_all

- **FR-012**: `knowledge_clear_all` accepts `confirm` (bool, required). The method MUST reject any value that is not exactly `true`.
- **FR-013**: When `confirm` is not exactly `true`, the service returns a structured error with the message `"Must set 'confirm' to true to clear graph"` and performs NO deletion.
- **FR-014**: When `confirm: true`, the service executes the following sequence under the writer lock:
  1. Close the database connection.
  2. Delete the database files (or directory) on disk. Must handle both single-file and directory-of-files storage layouts (matches Python's `is_dir()` / `is_file()` branch).
  3. Delete the WAL directory if present. WAL directory location is `db_path/../wal` (sibling of the DB directory, per Python convention). If the Rust layout differs, the Research stage must document the actual location and both sides must align.
  4. Reinitialize a fresh empty database.
  5. Return `{success: true, message: <string>}`.
- **FR-015**: If reinitialization fails after the delete phase, the response is an error and a recovery hint is included in the message. The service MUST NOT return success if it cannot serve subsequent requests.
- **FR-016**: After a successful `clear_all`, subsequent calls to `knowledge_status` MUST report zero entities, edges, and episodes, and WAL as absent or empty.

### Key Entities

- **Episode**: A graph node representing a processed text chunk. Identified by a `source_description` (used by `delete_by_source`) and a `chunk_id` field (used by `delete_chunk_episode`). Multiple revisions of the same chunk may exist as separate episode rows.
- **WAL directory**: A sibling directory to the DB directory containing write-ahead log files. Must be deleted and not just the DB during `clear_all`, so that the next WAL replay does not reconstruct the old graph.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Unmodified Python clients (`reader_server.py`, `writer_server.py`) can call all three methods against liminis-graph without code changes; responses parse correctly.
- **SC-002**: After `delete_by_source` for a known file, no entity in the graph retains any edge whose endpoint mentions an episode from that file (verified via count query).
- **SC-003**: After `delete_chunk_episode` for a chunk with 3 revisions, all 3 episode rows are gone (verified via `knowledge_status` episode_count delta).
- **SC-004**: After `clear_all` with `confirm: true`, `knowledge_status` reports all-zero counts and the next `knowledge_process_chunk` call succeeds against the fresh DB.
- **SC-005**: `clear_all` without `confirm` does not modify the database — verified by `knowledge_status` counts being unchanged before and after the rejected call.
- **SC-006**: A concurrent `knowledge_process_chunk` issued during a `delete_by_source` does NOT observe a partial deletion — either the delete completes first and the chunk is added to the cleaned graph, or the chunk is added first and the delete picks it up correctly (verified by writer-lock serialization guarantee).

## Assumptions

- The writer lock from ADR-042 is the correct synchronization primitive for all three methods. Reads may continue during deletes per the reader/writer split.
- LadybugDB's storage layout is a directory of files. The `clear_all` implementation must handle both directory and single-file layouts to match Python's behaviour.
- The WAL directory location is `db_path/../wal` (sibling of the DB directory), matching Python's convention. If the Rust path layout differs, the Research stage must document the actual location and align both sides.
- LLM cost is not tracked for these methods — they make no LLM calls. Response shapes do NOT include cost fields.
- Orphan entities after `delete_chunk_episode` are an accepted trade-off. Cleaning them up belongs in a future entity-GC method.
- `clear_all` concurrent with another write serializes (waits for the writer lock); it does not fail fast. This matches Python's serialization behaviour.

## Out of Scope

- Garbage-collection of orphan entities after `delete_chunk_episode` (separate future spec).
- Soft-delete or undo (Python is hard-delete only; parity requires the same).
- Selective `clear_all` (e.g., "clear only group X") — this would be a different method.
- Backup-before-clear — caller's responsibility; `clear_all` is irreversible by design once `confirm: true` is sent.
- Cross-file move detection — if a user renames `a.md` to `b.md`, this spec offers no migration path; the caller must `delete_by_source(a.md)` then re-process.

## Source References

- `graphiti_service.py` lines 1872–1960 and 2517–2540 — Python reference implementations
- `docs/adr/0042-reader-writer-split.md` — writer lock specification
- Issue #26 (Tier 1a) — IPC handler-dispatch pattern and error-shape conventions (blocking dependency)
- Constitution Principle I (IPC Parity) and Principle IV (WAL Is Authoritative)
