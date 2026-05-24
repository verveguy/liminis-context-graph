# Feature Specification: Tier 1a Service Handshake Methods

**Feature Branch**: `fabrik/issue-26`
**Created**: 2026-05-22
**Status**: Draft
**Input**: Issue #26 — "Tier 1a: service handshake methods (health_check, knowledge_status, knowledge_process_chunk)"

## Background

liminis-graph is a Rust replacement for the upstream Python graphiti-core service daemon. The Python `reader_server.py` and `writer_server.py` clients connect to the daemon over a Unix socket (JSON-RPC 2.0) and call a fixed set of IPC methods. Without an exact wire-compatible implementation of these methods in Rust, the Python clients cannot switch to the Rust backend.

Tier 1a covers the three methods every client calls before doing any substantive work:

1. **`health_check`** — liveness probe called at startup to distinguish "daemon not running" from "daemon slow to start." Without it, the ContextGraphPanel in liminis-app renders as "service down" during a slow boot.
2. **`knowledge_status`** — polled every few seconds by liminis-app to drive the ContextGraphPanel (entity/edge/episode counts, WAL state, DB path, embedding model). It is the highest-traffic method in the entire IPC surface (31 call sites in the Python client layer).
3. **`knowledge_process_chunk`** — the primary ingest method called by the indexing queue. The Rust service already has an equivalent handler (`knowledge_add_episode`) with different parameter names; this is a thin translation alias with a richer response shape.

The JSON-RPC 2.0 transport over Unix socket is already implemented. This feature adds three new dispatch handlers and any supporting DB query methods, without touching the transport layer.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Client Connects and Verifies Liveness (Priority: P1)

A liminis client opens a Unix-socket connection and calls `health_check` before forwarding any tool calls. If the daemon is up and its DB is queryable, the client proceeds. If the daemon is up but DB initialisation failed, the client surfaces an actionable error rather than hanging or misreporting "service down."

**Why this priority**: Without this method, slow daemon startup is indistinguishable from a crashed daemon. ContextGraphPanel renders erroneously and the user has no recovery path.

**Independent Test**: Start liminis-graph against a temp LadybugDB, send a JSON-RPC `health_check` request over the Unix socket, and assert the response is `{"ok": true}` within 10 ms of the request being dispatched on a warm service.

**Acceptance Scenarios**:

1. **Given** a running liminis-graph with a healthy, queryable DB, **When** a client sends `{"jsonrpc":"2.0","id":1,"method":"health_check","params":{}}`, **Then** the service replies `{"jsonrpc":"2.0","id":1,"result":{"ok":true}}` within 10 ms (warm path).
2. **Given** liminis-graph started but DB open failed (e.g., permission denied, corrupt file), **When** a client sends `health_check`, **Then** the service replies with a JSON-RPC error object (`{"code": <non-zero>, "message": "<subsystem>: <reason>"}`) naming the failed subsystem — not `{"ok": true}`.
3. **Given** a client sends `health_check` while the DB is still initialising (race at startup), **When** dispatched, **Then** the service returns a not-ready JSON-RPC error immediately rather than blocking the caller.
4. **Given** the DB path's filesystem permissions are revoked after startup, **When** `health_check` is called, **Then** service returns a JSON-RPC error with a permission error message and does not crash the daemon.

---

### User Story 2 — ContextGraphPanel Renders Live Service State (Priority: P1)

liminis-app polls `knowledge_status` every few seconds to display entity/edge/episode counts, WAL state, DB path, and embedding model in the ContextGraphPanel. The renderer cannot switch from the Python backend without a response shape that matches what it already parses.

**Why this priority**: Highest-traffic method (31 call sites). Any shape mismatch or missing field causes ContextGraphPanel to silently mis-render or throw parse errors.

**Independent Test**: Populate a LadybugDB fixture with known counts (e.g., 50 entities, 120 edges, 30 episodes), call `knowledge_status`, and assert that every enumerated field in FR-003 matches the fixture exactly.

**Acceptance Scenarios**:

1. **Given** a DB with 50 entities, 120 relationships, and 30 episodes, **When** a client sends `knowledge_status`, **Then** the response contains `entity_count: 50`, `relationship_count: 120`, `episode_count: 30`, `context_graph_initialized: true`, `connected: true`, `initializing: false`, plus `database_path` and `embedding_model`.
2. **Given** a populated WAL directory containing files, **When** `knowledge_status` is called, **Then** the response contains a `wal` subobject with `exists: true`, `file_count: <n>`, `byte_size: <total bytes>`.
3. **Given** a WAL directory that exists but is empty (zero `.jsonl` files), **When** `knowledge_status` is called, **Then** `wal` subobject contains `exists: true`, `file_count: 0`, `byte_size: 0`.
4. **Given** a field the Rust service does not cheaply compute (e.g., LLM cost, cumulative uptime), **When** `knowledge_status` is called, **Then** that field is **absent from the JSON object** — it MUST NOT appear as `null`, `0`, `false`, or any fabricated value.
5. **Given** an active write transaction in progress, **When** `knowledge_status` is called concurrently, **Then** the response reflects counts from the pre-transaction state and returns promptly — it MUST NOT block waiting for the writer to finish.

---

### User Story 3 — Client Ingests a Chunk Into the Graph (Priority: P1)

The indexing queue calls `knowledge_process_chunk` for each document chunk. The Rust service already processes episodes via `knowledge_add_episode`, but the Python client sends different parameter names and expects a richer response. This method is a thin adapter: it accepts Python's param names, delegates to the existing `add_episode` pipeline, and returns Python's expected response shape.

**Why this priority**: Without wire-compatible ingest, liminis-graph cannot replace the Python daemon for any indexing workload. The append-only episode guarantee is load-bearing for the temporal/episodic memory model — violating it corrupts query results across time.

**Independent Test**: Send a `knowledge_process_chunk` request shaped exactly as Python sends it (`chunk_text`, `chunk_id`, `source_file`, `group_id`, `reference_time`), assert the response fields match FR-006 exactly, and verify the graph contains the expected episode and at least one entity node.

**Acceptance Scenarios**:

1. **Given** an empty DB and a chunk with clear subject and predicate, **When** a client sends `knowledge_process_chunk` with Python param names, **Then** the response is `{"success": true, "chunk_id": "<original>", "source_file": "<original>", "episode_uuid": "<uuid>", "nodes_extracted": <≥1>, "edges_extracted": <≥0>, "duration_seconds": <float>}`.
2. **Given** the same chunk is sent twice with the same `chunk_id`, **When** both calls complete, **Then** two distinct `episode_uuid` values exist and two distinct episode rows are in the DB — the second call MUST NOT overwrite or delete the first episode.
3. **Given** a request with an empty `chunk_text`, **When** dispatched, **Then** the service returns a JSON-RPC error (`FR-009`) before performing any LLM call or DB write.
4. **Given** a request with a missing `chunk_id` field, **When** dispatched, **Then** the service returns a JSON-RPC error before performing any LLM call or DB write.
5. **Given** a request with a `reference_time` value that cannot be parsed as ISO 8601, **When** dispatched, **Then** the service returns a JSON-RPC error before performing any LLM call or DB write.

---

### Edge Cases

- `health_check` is called before the DB finishes opening → returns a not-ready JSON-RPC error immediately; never blocks the caller.
- `knowledge_status` is called while a write transaction holds the exclusive lock → returns counts from pre-transaction DB state without waiting; never deadlocks.
- WAL directory does not exist → `wal` subobject: `exists: false`, `file_count: 0`, `byte_size: 0`.
- WAL directory exists but is empty → `wal` subobject: `exists: true`, `file_count: 0`, `byte_size: 0`.
- DB path permission revoked after successful startup → `health_check` returns a JSON-RPC error (not `ok: false`), daemon stays running.
- `group_id` absent from `knowledge_process_chunk` params → defaults to `"liminis"` (same default as Python and all other handlers).
- `reference_time` absent → defaults to server wall-clock time at dispatch.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The service MUST accept JSON-RPC 2.0 over Unix socket for the three new methods: `health_check`, `knowledge_status`, `knowledge_process_chunk`. Transport and framing are unchanged from existing handlers.
- **FR-002**: `health_check` MUST return `{"ok": true}` when the DB is open and queryable. It MUST return a JSON-RPC error object (not `ok: false`) when the DB is unavailable, naming the failed subsystem in the `message` field. It MUST NOT block if the DB is initialising. Warm-path latency MUST be under 10 ms.
- **FR-003**: `knowledge_status` MUST include the following fields in its result object: `database_path` (string), `embedding_model` (string), `embedding_dim` (integer), `entity_count` (integer), `relationship_count` (integer), `episode_count` (integer), `wal` (object with `exists`, `file_count`, `byte_size`), `context_graph_initialized` (bool), `connected` (bool), `initializing` (bool). The field `last_index_time` (string, ISO 8601) MUST be included when at least one episode exists, and MUST be `null` when no episodes exist.
- **FR-004**: Any Python `knowledge_status` response field that the Rust service cannot cheaply compute MUST be **omitted entirely** from the response — not set to `null`, `0`, `false`, or any placeholder. Deferred fields MUST be enumerated in the release notes for this issue with a follow-up tracking issue reference.
- **FR-005**: `knowledge_process_chunk` MUST accept the following parameter names matching Python: `chunk_text` (string, required), `chunk_id` (string, required), `source_file` (string, required), `group_id` (string, optional, default `"liminis"`), `reference_time` (ISO 8601 string, optional, default now).
- **FR-006**: `knowledge_process_chunk` MUST return a result object with the following fields matching Python: `success` (bool), `chunk_id` (string), `source_file` (string), `episode_uuid` (string), `nodes_extracted` (integer), `edges_extracted` (integer), `duration_seconds` (float).
- **FR-007**: `knowledge_process_chunk` MUST NOT delete or replace any existing episode with the same `chunk_id`. Re-sending the same chunk appends a new episode. This matches the Python service's explicit guarantee and is load-bearing for the temporal/episodic memory model.
- **FR-008**: All three methods MUST return errors as JSON-RPC error objects. None of them may crash the daemon or leave a partial DB write visible to subsequent readers.
- **FR-009**: `knowledge_process_chunk` MUST reject requests with empty `chunk_text`, absent `chunk_id`, or unparseable `reference_time` before making any LLM call or DB write. The rejection MUST be a JSON-RPC error object.
- **FR-010**: Unmodified `reader_server.py` and `writer_server.py` MUST be able to call all three methods against the Rust daemon and receive responses they can parse without error.

### Key Entities

- **health_check result**: `{"ok": true}` on success; JSON-RPC error object on failure.
- **knowledge_status result**: Subset of Python's response shape. Required fields: `database_path`, `embedding_model`, `embedding_dim`, `entity_count`, `relationship_count`, `episode_count`, `wal`, `context_graph_initialized`, `connected`, `initializing`. `last_index_time` (ISO 8601 string) is included when episodes exist, `null` otherwise. `index_created_at` is deferred (tracked in issue #47). All other Python fields omitted unless cheaply computable.
- **WAL subobject**: `{"exists": bool, "file_count": int, "byte_size": int}`. Populated by scanning `LCG_WAL_DIR` (or equivalent configured path) at request time.
- **knowledge_process_chunk params**: `chunk_text` → `episode_body`, `chunk_id` → episode `name` and `source_description` (matching Python's behavior; `source_file` is validated and returned in the response but not stored in the episodic node), `source` defaults to `"text"`, `group_id` → `group_id`, `reference_time` → `reference_time`.
- **knowledge_process_chunk result**: `{"success": true, "chunk_id": str, "source_file": str, "episode_uuid": str, "nodes_extracted": int, "edges_extracted": int, "duration_seconds": float}`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Unmodified `reader_server.py` and `writer_server.py` complete the startup handshake against liminis-graph and successfully call all three methods end-to-end against a real LadybugDB without error.
- **SC-002**: `health_check` p95 latency < 10 ms over 100 sequential calls on a warm service with an open DB.
- **SC-003**: `knowledge_status` entity/edge/episode counts match fixture values exactly across five DB sizes: empty, 1, 10, 100, and 1000 rows per table.
- **SC-004**: `knowledge_process_chunk` of a chunk previously processed by the Python service produces entity and edge counts within the extraction-eval framework's tolerance (LLM nondeterminism applies; byte-identical output is not required).
- **SC-005**: Sending the same `chunk_id` twice produces two distinct `episode_uuid` values and two distinct episode rows in the DB.
- **SC-006**: Every field present in the Python `knowledge_status` response either (a) appears in the Rust response with equivalent meaning, or (b) is explicitly listed in the release notes for this issue as deferred, with a tracking issue reference.

## Assumptions

- JSON-RPC 2.0 framing over Unix socket is already implemented in the `handlers.rs` / `main.rs` IPC layer. This feature adds handler functions and DB query methods; it does not modify the transport.
- The existing `episode::add_episode` function is functionally equivalent to the Python graphiti-core `add_episode` pipeline (embed → LLM extract → dedup → write). `knowledge_process_chunk` delegates to it.
- LadybugDB supports `COUNT(*)` on entity, edge, and episode tables with latency acceptable for interactive polling (< 100 ms on a 10 k-entity DB). Any count that cannot meet this bar is deferred and documented.
- The Python `knowledge_status` field set (as returned by `graphiti_service.py`) is the canonical contract for field names, types, and semantics.
- LLM cost fields present in Python's `knowledge_status` response are **out of scope**; they will be listed as deferred in the release notes.
- The `source` parameter to `add_episode` defaults to `"text"` for all `knowledge_process_chunk` calls (chunks are always plain text; the Python service uses the same default).
- `group_id` default `"liminis"` matches the Python service and the existing `DEFAULT_GROUP_ID` constant in `handlers.rs`.
- The WAL directory path is available to the service at runtime via environment variable (same mechanism used by `Db::open_or_rebuild`).
- `AppState` does not currently store DB-open status explicitly; `health_check` will perform a cheap probe query (e.g., `RETURN 1`) to verify the DB is queryable.

## Out of Scope

- LLM cost tracking — deferred to a separate follow-up issue.
- `knowledge_index_document` and `knowledge_process_document` — document chunking belongs in liminis-app; liminis-app will be updated separately to always pre-chunk before calling `knowledge_process_chunk`.
- Any UI changes in liminis-app.
- Modifications to the JSON-RPC transport, socket path handling, or connection lifecycle.
- Changes to existing handlers (`knowledge_add_episode`, `knowledge_find_entities`, etc.).

## Source References

- `liminis-graph-core/src/handlers.rs` — existing dispatch table; new handlers added here
- `liminis-graph-core/src/episode.rs` — `add_episode` pipeline delegated to by `knowledge_process_chunk`
- `liminis-graph-core/src/app_state.rs` — `AppState` (holds `db`, `embedder`, `write_lock`)
- `liminis-graph-core/src/db.rs` — `Db::open`, `Conn` — count queries and health probe added here
- `liminis-graph/src/main.rs` — IPC server event loop (no changes expected)
- Python reference: `liminis-framework/framework/src/skills/knowledge-graph/scripts/graphiti_service.py`
