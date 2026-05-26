# Feature Specification: Application WAL Not Written After Recreate — Regression of #74

**Feature Branch**: `fabrik/issue-100`
**Created**: 2026-05-26
**Status**: Draft
**Input**: Live observation 2026-05-26 — after running Recreate from the liminis-app UI and re-ingesting all 46 episodes of demo-notebook, `knowledge_status` reports `wal: {exists: false, file_count: 0, byte_size: 0}` despite the DB growing to ~3 MB. Zero application WAL JSONL files written to disk over the entire ingestion. `knowledge_rebuild_from_wal` against this workspace would have nothing to replay.

## Background

liminis-graph#74 wired the application WAL writer into the production write paths. Verified working end-to-end 2026-05-25 evening: a fresh ingestion populated `.graphiti/wal/` with JSONL files containing every mutation. liminis-graph#66/#68 made the WalWriter self-healing via `create_dir_all` in `flush_pending`, so if the WAL directory disappears at runtime, the next flush recreates it transparently.

Today's behavior contradicts both: after a Recreate (which deletes `.graphiti/wal/` per liminis-graph#61's `preserve_wal: false` semantics, executed by the now-correct UI handler per liminis#824), no application WAL files reappear during the subsequent re-ingestion. The self-healing `flush_pending` evidently isn't running, or the WalWriter wasn't re-initialized to know about the WAL directory, or `GRAPHITI_WAL_DIR` is mis-resolved post-Recreate.

### Reproduction

1. Start liminis-app against a workspace, ingest some content, verify `.graphiti/wal/` (or `.lcg/wal/`) contains JSONL files.
2. Click Recreate in the Knowledge Graph panel.
3. Wait for the recreate confirmation.
4. Re-ingest the same content.
5. Inspect `knowledge_status` and the workspace `.../wal/` directory.

**Expected**: WAL directory exists, contains JSONL files, `wal.exists: true` with a non-zero `byte_size`.  
**Actual**: WAL directory absent, `wal.exists: false`, no files.

### Why this matters

- **Recovery story broken again.** The WAL is the canonical git-trackable backup. After Recreate + ingest, the user has a populated DB but no WAL — meaning the next time `db.wal` corrupts (e.g. unclean shutdown if the clean-shutdown fix #71 is bypassed somehow), the user cannot recover via `knowledge_rebuild_from_wal`.
- **Verification regression.** We proved this worked yesterday. Something between then and now broke it. Likely candidates: liminis-graph#73 (WalWriter reset in clear_all) wasn't implemented carefully; or liminis#824 (Recreate-deletes-WAL fix) deletes the dir but the WalWriter on the running service doesn't know it has to re-init.
- **Cutover blocker.** Users running their first Recreate after upgrading will get a silently-broken backup story. They won't know until they need to Reload.

### Probable root cause hypotheses

1. **WalWriter holds a path-only struct, not a live file handle.** When the dir is deleted, the writer's path is still valid; `create_dir_all` in `flush_pending` should still recreate it. But if no `flush_pending` runs in this session — e.g. because nothing is calling into the writer at all — the file never appears. Check: is the write path post-#74's `with_chunk` plumbing actually invoking the writer?
2. **WalWriter was replaced with `None` during the clear_all path.** If liminis-graph#73 (WalWriter reset in clear_all) was implemented by setting `state.wal_writer = None` and never re-initializing it on the next write, that's the bug. The fix in #73 should re-create the writer on next use, not leave it None forever.
3. **Path resolution diverged.** Maybe `GRAPHITI_WAL_DIR` env var isn't reaching the new binary correctly, or the binary computes a different default after seeing no env var.
4. **`flush_pending` not called.** Either the writer is being log_mutation'd but never flushed, or the flush trigger (chunk-boundary `with_chunk` exit) isn't firing.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — After Recreate + Re-Ingestion, WAL Is Populated (Priority: P1)

When the user runs Recreate and re-ingests, the application WAL MUST be re-populated with JSONL mutation lines as ingestion proceeds. The WAL directory MUST be present and non-empty when ingestion completes.

**Why this priority**: This is the broken behavior; without it, recovery is impossible.

**Independent Test**: Recreate a populated workspace, re-ingest one chunk that produces measurable mutations (e.g. `knowledge_process_chunk` with a known-rich text), assert the workspace WAL directory contains at least one JSONL file with mutation lines.

**Acceptance Scenarios**:

1. **Given** a workspace that just completed a Recreate, **When** the user re-ingests N chunks, **Then** the WAL directory contains at least one JSONL file and the file contains at least one mutation line per write that produced one.
2. **Given** the running service after a Recreate, **When** any write handler executes, **Then** `knowledge_status.wal.exists` is `true` and `byte_size > 0`.
3. **Given** the WAL writer was reset during Recreate, **When** the next write occurs, **Then** the writer transparently re-initializes against the workspace WAL path (matching #66/#68's self-healing design).

---

### User Story 2 — `knowledge_rebuild_from_wal` Works After Recreate-then-Ingest (Priority: P1)

The whole point of populated WAL is replay. After Recreate-then-ingest, `knowledge_rebuild_from_wal` against an empty DB MUST reconstruct the post-ingest state.

**Why this priority**: This is the end-to-end recovery story — validating WAL population (Story 1) without confirming replay still leaves users without a verified recovery path.

**Independent Test**: Populate a WAL via post-Recreate ingestion, delete the DB, invoke `knowledge_rebuild_from_wal`, assert entity/edge counts match the pre-deletion state within rebuild tolerance.

**Acceptance Scenarios**:

1. **Given** a populated WAL produced by post-Recreate ingestion, **When** the DB is deleted and `knowledge_rebuild_from_wal` is invoked, **Then** the resulting graph state matches the pre-deletion state (entity/edge counts within rebuild tolerance).

---

### Edge Cases

- **Service restart between Recreate and re-ingestion.** The new service comes up, sees `.graphiti/wal/` (or `.lcg/wal/`) doesn't exist (it was nuked), initializes the WalWriter against the path, and creates the directory on first write. This is the "fresh workspace" path and should work — if it doesn't, that's a different bug.
- **Recreate without re-ingestion.** The WAL stays empty. No bug — there are no mutations to log.
- **Multiple Recreates in succession without intervening writes.** Each Recreate clears the WAL (no-op the second time since it's already empty). No bug, no edge concern.
- **WAL directory is on read-only storage.** Out of scope — that's an ops error, not a bug.
- **The WAL writer is reset to None by a future implementation of liminis-graph#73.** Whatever that work does, it must restore the lazy-init invariant per FR-001.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The WalWriter MUST be re-initialized (or its self-healing path MUST fire) on the first write following a Recreate (`knowledge_clear_all` with `preserve_wal: false`). The writer must NOT remain in a "no-op until restart" state after the WAL directory is deleted.
- **FR-002**: Every mutation that produces a WAL-loggable change MUST result in a JSONL line appended to the active WAL file post-Recreate, identically to behavior in a fresh-startup workspace.
- **FR-003**: A regression test MUST cover this scenario: start service → Recreate → write → assert WAL file exists. The existing tests for #74 don't cover the post-Recreate path (they're against fresh workspaces).
- **FR-004**: If the WalWriter is in an uninitialized / no-op state after Recreate, the service MUST log a warn-level message on the next write attempt — no silent failure.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After Recreate + re-ingestion of demo-notebook (~46 episodes), the workspace WAL directory (`~/.../demo-notebook/.graphiti/wal/` or `.lcg/wal/` post-schism-fix) is non-empty, with at least one JSONL file containing mutation lines.
- **SC-002**: `knowledge_status.wal.exists` returns `true` after the first post-Recreate write.
- **SC-003**: `knowledge_rebuild_from_wal` against a Recreate-then-ingest workspace reproduces the post-ingest entity/edge counts within tolerance.
- **SC-004**: New regression test in `liminis-graph-core/tests/wal_after_recreate.rs` passes; existing tests pass unchanged.
- **SC-005**: No silent failure mode: if the writer ever can't initialize, a warn-level log fires.

## Assumptions

- **A1**: The diagnosis is that the WalWriter on the running service has stale state (a `None` or a stale path) after Recreate. Confirmable by adding a debug log in `flush_pending` and observing whether it fires post-Recreate.
- **A2**: liminis-graph#66/#68's self-healing `create_dir_all` is correct in isolation — verified against fresh-startup workspaces.
- **A3**: The fix is small (re-init the writer on the post-Recreate write path), not architectural (no need to rewrite the WAL stack).
- **A4**: The integration test for FR-003 can exercise this without LLM calls — the WAL write path is independent of LLM behavior.

## Out of Scope

- Changing the IPC contract for `knowledge_status.wal` (existing fields are correct; they just need to report `true` when they should).
- Changing the Recreate UI behavior (liminis#824 handles that; this is purely server-side).
- Adding new WAL configuration (rotation, archive, etc. — separate concerns).

## Source References

- `liminis-graph-core/src/app_state.rs` — `state.wal_writer` field across the `clear_all` call
- `liminis-graph-core/src/wal.rs` — `WalWriter::flush_pending` self-healing path
- `liminis-graph-core/src/handlers.rs` — `handle_clear_all` and post-Recreate write handlers
- `liminis-graph-core/tests/wal_after_recreate.rs` — new regression test (to be created)
- **liminis-graph#74 (merged)**: wired the WAL writer into production paths — this issue is its regression
- **liminis-graph#66 / #68 (merged)**: self-healing `flush_pending` via `create_dir_all` — should cover this case but evidently isn't firing
- **liminis-graph#73 (status TBD)**: explicit WalWriter reset in clear_all — if implemented by nulling the writer permanently, this is the root cause
- **liminis#824 (in flight / merged)**: Recreate-deletes-WAL fix in the UI handler — this issue assumes that fix landed correctly
