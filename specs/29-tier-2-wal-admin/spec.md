# Feature Specification: Tier 2 WAL Admin — prepare_checkpoint, rebuild_from_wal, rebuild_status

**Feature Branch**: `fabrik/issue-29`
**Created**: 2026-05-22
**Status**: Draft
**Input**: Issue #29 — "Tier 2: WAL admin (prepare_checkpoint, rebuild_from_wal, rebuild_status)"

## Background

Tier 2 of the liminis-graph ↔ liminis integration exposes three admin methods covering the WAL durability surface: rotate before git checkpoint, rebuild graph from WAL for recovery, and poll background rebuild progress.

**Python reference implementations**:
- `knowledge_rebuild_from_wal` — `graphiti_service.py` lines 2079–2180 (non-streaming) + 2200+ (streaming)
- `knowledge_rebuild_status` — `graphiti_service.py` lines 2180–2200
- `knowledge_prepare_checkpoint` — **not implemented in Python**; liminis-app calls it at `workspace-checkpoint.ts:51-57` and silently catches the resulting error. liminis-graph will be the first backend to implement it.

liminis-graph already has the building blocks: `WalReplayer` in `replay.rs`, the WAL appender in `wal.rs`, and WAL embedding enrichment (landed 2026-04-23). This issue wires those into the IPC handler-dispatch surface established by Tier 1a (#26).

**Why this is needed**: The Python service has a race condition where a half-written WAL chunk can end up committed to git during a workspace checkpoint, because `prepare_checkpoint` silently fails. `rebuild_from_wal` exists in Python but lacks progress streaming. `rebuild_status` provides a polling path for non-streaming clients. Together these three methods complete the WAL durability surface.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — User creates a workspace git checkpoint (Priority: P1)

Before liminis-app runs `git commit` to checkpoint the workspace, it asks the graph service to flush/rotate the WAL so the committed WAL files form a consistent, fully recoverable snapshot. liminis-graph will be the first backend to honour this call, preventing the race where a half-written WAL chunk ends up committed.

**Why this priority**: New capability that closes a correctness gap. The Python service silently fails today; liminis-app catches the error and proceeds anyway.

**Independent Test**: Spin up liminis-graph against a workspace, run `knowledge_process_chunk` to produce WAL activity without letting the appender close, call `knowledge_prepare_checkpoint`, then inspect the WAL directory — every chunk MUST have been flushed to a closed `.jsonl` file with a complete final newline; no half-written chunks in any `.jsonl`.

**Acceptance Scenarios**:

1. **Given** the service has appended N WAL entries to the currently-open WAL file, **When** client sends `knowledge_prepare_checkpoint`, **Then** the current WAL file is closed (flushed + fsynced + renamed if appropriate), the service starts a fresh WAL file for subsequent writes, and the response is `{success: true, files_flushed: <int>, files_total: <int>}`.
2. **Given** no WAL writes have happened since the last rotation, **When** the call is made, **Then** `{success: true, files_flushed: 0, files_total: <int>}` — idempotent, not an error.
3. **Given** a write is currently in flight, **When** the call is made, **Then** the call waits for the writer to release (via writer lock), then flushes — does not interrupt the in-flight write.
4. **Given** the WAL directory does not exist yet (cold start), **When** the call is made, **Then** `{success: true, files_flushed: 0, files_total: 0}`.

---

### User Story 2 — User rebuilds graph from WAL (Priority: P1)

When the DB is corrupted or the user wants to re-apply WAL changes from scratch, liminis-app calls `knowledge_rebuild_from_wal`. Two variants exist: streaming (with `_progress_token`) and non-streaming (without). The streaming variant is the only method in the entire IPC surface that emits progress lines before the terminal response.

**Why this priority**: Core recovery path. Without it, WAL durability is write-only — there's no way to recover a corrupted DB without external tooling.

**Independent Test**: Populate a WAL with N known mutation lines across M files. Delete the DB. Open a socket and send `knowledge_rebuild_from_wal` with a `_progress_token`. Assert M+1 or more progress lines arrive before the terminal response; assert the terminal response is `{success: true, mutations_replayed: N, wal_files_processed: M, ...}`; assert the rebuilt DB has the expected entity/edge/episode counts.

**Acceptance Scenarios (non-streaming)**:

1. **Given** a populated WAL and an empty DB, **When** client sends `knowledge_rebuild_from_wal` WITHOUT `_progress_token`, **Then** the service starts a background rebuild and returns immediately with `{success: true, job_id: <uuid>, status: "running"}`.
2. **Given** a rebuild is already running, **When** a second non-dry-run call comes in, **Then** returns the existing `job_id` — does not start a second rebuild.
3. **Given** active writes are in progress and `dry_run` is false, **When** called, **Then** returns `{success: false, error: "Service is busy: <N> write operation(s) in progress..."}`.
4. **Given** the WAL directory is empty or missing, **When** called, **Then** `{success: false, error: "No WAL files found at <path>"}`.
5. **Given** `dry_run: true`, **When** called, **Then** the WAL is parsed and counted but no mutations applied; response is `{success: true, mutations_replayed: <count>, indexes_created: 0, wal_files_processed: <count>, dry_run: true}`.
6. **Given** `from_seq: N`, **When** called, **Then** only WAL entries with seq ≥ N are applied.
7. **Given** an invalid `from_seq` (bool, negative number, or non-integer), **When** called, **Then** returns a structured error before any work is performed.

**Acceptance Scenarios (streaming)**:

1. **Given** a populated WAL and an empty DB, **When** client sends `knowledge_rebuild_from_wal` WITH `_progress_token`, **Then** the service streams `{type: "progress", message, mutations_replayed_so_far, files_processed_so_far}` lines as it works, then sends a terminal `{success: true, mutations_replayed, wal_files_processed, indexes_created, ...}`.
2. **Given** `dry_run: true` with `_progress_token`, **When** called, **Then** progress lines are emitted BUT no mutations applied; terminal response includes `dry_run: true`.
3. **Given** the client disconnects mid-stream, **When** the service detects the broken pipe, **Then** it aborts the rebuild cleanly; the DB may be in a partial state (documented behaviour, matches Python).

---

### User Story 3 — User polls rebuild progress (Priority: P1)

For the non-streaming rebuild flow, liminis-app polls `knowledge_rebuild_status` with the job_id to render a progress bar. Required for non-streaming clients (e.g., raw `curl`-style probes) that cannot consume the streaming variant.

**Why this priority**: Completes the non-streaming rebuild flow. Without it, callers cannot know when the background job finishes.

**Independent Test**: Start a rebuild via non-streaming `rebuild_from_wal`. Poll `rebuild_status` with the returned job_id while the rebuild runs; assert each poll returns monotonically increasing `mutations_replayed`. After completion, assert `status` flips to `"completed"` or `"failed"` and the `result` or `error` field is populated.

**Acceptance Scenarios**:

1. **Given** a running rebuild with known job_id, **When** client sends `knowledge_rebuild_status` with that job_id, **Then** response is `{job_id, status: "running", mutations_replayed: <current>, wal_files_processed: <so_far>, start_time, elapsed_seconds, error: null, result: null}`.
2. **Given** the rebuild completed successfully, **When** polled, **Then** `status: "completed"`, `result` is populated with the final replay stats.
3. **Given** the rebuild errored, **When** polled, **Then** `status: "failed"`, `error` contains the error message.
4. **Given** an unknown `job_id`, **When** polled, **Then** `{status: "not_found"}` — not an error.

---

### Edge Cases

- `prepare_checkpoint` called concurrently with active `process_chunk` calls → serialized via the WAL appender's internal lock; checkpoint waits for the in-flight chunk's WAL line to complete before rotating.
- Two `prepare_checkpoint` calls in flight simultaneously → idempotent; the second sees zero unflushed entries and returns `files_flushed: 0`.
- `rebuild_from_wal` interrupted mid-replay (process killed) → on next service start, the job state is gone (in-memory); the DB is in a partial state. Callers must retry. This matches Python's behaviour. Persisting job status across restarts is future work.
- `_progress_token` is provided but streaming fails halfway (e.g., client disconnects) → service detects the broken pipe, aborts the rebuild cleanly, and leaves the DB in whatever state was reached (matches Python).
- `from_seq` exceeds the highest seq in the WAL → `{success: true, mutations_replayed: 0, ...}` — empty replay, not an error.
- `prepare_checkpoint` invoked on a service that has never opened a WAL appender → no-op success; must not crash.

## Requirements *(mandatory)*

### Functional Requirements

#### Common
- **FR-001**: All three methods are registered in the handler-dispatch table from Tier 1a (#26). `rebuild_from_wal` and `prepare_checkpoint` acquire the writer lock. `rebuild_status` is read-only and does not.
- **FR-002**: All three methods return errors as JSON-RPC error objects, never crash the daemon, and never leave the WAL or DB in a half-mutated state visible to clients.

#### prepare_checkpoint
- **FR-003**: `knowledge_prepare_checkpoint` accepts no required parameters (empty object is acceptable).
- **FR-004**: The handler MUST flush any unflushed WAL writes to disk, close the currently-open WAL file, and ensure a fresh WAL file will be opened on the next write. Crash-safety guarantee: after a successful response, every WAL line written before the call is durable on disk.
- **FR-005**: Response: `{success: true, files_flushed: <int>, files_total: <int>}`. `files_flushed` is the count of WAL files closed/rotated by this call (normally 0 or 1); `files_total` is the count of `.jsonl` files in the WAL dir after rotation.
- **FR-006**: If a write is in flight, the call MUST wait via the writer lock; MUST NOT abort the write.
- **FR-007**: Idempotent — repeated calls with no intervening writes return `{success: true, files_flushed: 0, ...}`.
- **FR-008**: If the WAL directory does not exist, the response is `{success: true, files_flushed: 0, files_total: 0}` — do not error.

#### rebuild_from_wal — non-streaming variant
- **FR-009**: `knowledge_rebuild_from_wal` (without `_progress_token`) accepts: `dry_run` (bool, default false), `from_seq` (non-negative integer, default 0).
- **FR-010**: Starts a background task and returns immediately with `{success: true, job_id: <uuid>, status: "running"}`.
- **FR-011**: If a rebuild is already running, the call returns the existing `job_id` — does not start a second.
- **FR-012**: If `_active_writes > 0` and `dry_run` is false, returns `{success: false, error: "Service is busy: <N> write operation(s) in progress..."}`.
- **FR-013**: If the WAL directory is missing or empty, returns `{success: false, error: "No WAL files found at <path>"}`.
- **FR-014**: `from_seq` must validate as a non-negative integer (rejecting booleans and negative numbers); invalid values return a structured error before any work is performed.
- **FR-015**: `dry_run: true` parses and counts mutations WITHOUT applying them; response includes `dry_run: true` and `mutations_replayed: <count>`. The DB is unchanged.

#### rebuild_from_wal — streaming variant
- **FR-016**: When `_progress_token` is present in params, the service uses the streaming path: emit one or more `{type: "progress", message, mutations_replayed_so_far, files_processed_so_far}` lines on the socket before the terminal response.
- **FR-017**: The terminal response on the streaming variant is the full result: `{success: true, mutations_replayed, wal_files_processed, indexes_created, ...}` or `{success: false, error: ...}`.
- **FR-018**: Progress lines MUST be emitted at least once per WAL file processed AND at least once per 1000 mutations within a file. Exact cadence is implementation choice within those bounds.
- **FR-019**: Each progress line is a single `\n`-terminated JSON object on the socket. The terminal response uses the standard JSON-RPC framing.
- **FR-020**: If the client disconnects mid-stream, the service detects the broken pipe and aborts the rebuild cleanly. The DB may be in a partial state; this is documented behaviour consistent with the Python implementation.

#### rebuild_status
- **FR-021**: `knowledge_rebuild_status` accepts `job_id` (string, required).
- **FR-022**: Response when job exists: `{job_id, status: "running"|"completed"|"failed", mutations_replayed, wal_files_processed, start_time (ISO-8601), elapsed_seconds, error: null|string, result: null|object}`.
- **FR-023**: Response when job is unknown: `{status: "not_found"}`. Not an error condition.
- **FR-024**: Job state is in-memory only — not guaranteed to survive process restarts. This is documented; persisting job state is future work.

### Key Entities

- **WAL file** (`.jsonl`): A single append-only log file in `{workspace_root}/.lcg/wal/`. The WAL directory layout must match the Python implementation; if liminis-graph uses a different path, that path must be documented and both sides aligned.
- **Job** (`rebuild_status` job): In-memory record of a `rebuild_from_wal` background task, keyed by a UUID assigned at creation time.
- **Progress line**: A `\n`-terminated JSON object emitted on the socket during a streaming rebuild, before the terminal response.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Unmodified Python clients (`reader_server.py`, `writer_server.py`) and the app-side admin invoker (`mcp-direct-invoker.ts:906`) can call all three methods against liminis-graph without code changes. The streaming variant works end-to-end: progress lines parsed, terminal response parsed.
- **SC-002**: After `prepare_checkpoint`, a `git status` of the WAL directory shows all `.jsonl` files as clean after a 1000-chunk ingest stress test (no uncommitted partial writes).
- **SC-003**: A rebuild of a 10k-line WAL produces a DB whose entity/edge/episode counts match the live counts measured before the rebuild (within ±1 for cases where the WAL was rotated mid-write).
- **SC-004**: `rebuild_from_wal` with `dry_run: true` on a 10k-line WAL completes in under 10× the time of a comparable non-mutating replay AND leaves the DB unchanged (verified via byte-equal hashes of DB files before and after).
- **SC-005**: During a streaming rebuild on a 10k-line WAL, at least 5 progress lines are emitted to the client.
- **SC-006**: `rebuild_status` returns `"not_found"` for a freshly-generated UUID that was never used as a job_id.
- **SC-007**: `prepare_checkpoint` called during a `process_chunk` blocks until the chunk finishes writing its WAL line, then rotates; the rotated file ends with that chunk's line (no truncation).

## Assumptions

- `WalReplayer` in `liminis-graph-core/src/replay.rs` is feature-complete for full WAL replay (lexicographic file order, skip unknown ops, tolerate truncated final line) — confirmed by existing `wal_replay.rs` tests.
- The WAL appender in `wal.rs` either already exposes a flush/rotate operation suitable for `prepare_checkpoint`, or adding one is in scope for this issue.
- The writer lock from ADR-042 serializes writes; `prepare_checkpoint` and non-dry-run `rebuild_from_wal` both take it. `rebuild_status` does not.
- The streaming IPC framing (`{type: "progress", ...}` lines before terminal response) does not require backward-compatibility handling — both streaming and non-streaming variants are introduced together, and non-streaming clients simply use the non-streaming variant.
- WAL directory layout matches Python: `{workspace_root}/.lcg/wal/*.jsonl`. If liminis-graph uses a different path, Research/Plan stages must document the actual path and determine how to align.
- The handler-dispatch pattern and JSON-RPC error shape from Tier 1a (#26) are available on the branch before implementation begins.

## Out of Scope

- Persisting `rebuild_from_wal` job state across service restarts (future spec).
- Cancelling an in-flight rebuild from another client (future spec).
- Incremental WAL compaction or rewriting old WAL files for size savings.
- A `wal_dump` method that exports WAL as a portable format — `prepare_checkpoint` rotates in place; downstream dumping is a workflow concern for liminis-app's git checkpoint flow.
- Concurrent rebuilds on different group_ids — not supported in Python and not supported here.

## Source References

- `liminis-graph-core/src/replay.rs` — `WalReplayer`
- `liminis-graph-core/src/wal.rs` — WAL appender
- `liminis-graph-core/tests/wal_replay.rs` — existing replay tests
- `graphiti_service.py:2079-2200+` — Python reference for `rebuild_from_wal` and `rebuild_status`
- `workspace-checkpoint.ts:51-57` — liminis-app call site for `prepare_checkpoint`
- `mcp-direct-invoker.ts:906` — app-side admin invoker
- Issue #26 — Tier 1a: handler-dispatch pattern and JSON-RPC error shape (dependency)
