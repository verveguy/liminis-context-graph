# Feature Specification: Integration Architecture — In-Process MCP + Direct Socket Client + App-Bundled Binary

**Feature Branch**: `fabrik/issue-42`
**Created**: 2026-05-22
**Status**: Draft
**Input**: Issue #42 — "Integration architecture: in-process MCP + direct socket client + app-bundled binary"
**Blocked by**: #38 (list_relationships schema fix), #39 (embedder sidecar)

## Background

The current integration has three serialization bottlenecks at the wrong layer:

1. The **indexing queue** (`liminis-app/src/main/indexing-queue.ts:1481`) routes `knowledge_process_chunk` calls through `mcpDirectInvoker` → writer MCP server. Each call takes ~120s (LLM extraction). The MCP transport serializes per-server, so a backlog of N chunks blocks the writer MCP server for ~N×120s.

2. The **renderer's GraphitiPanel** polls `knowledge_status` through the same `mcpDirectInvoker` → reader MCP path. During heavy reads (e.g., agent's `search_passages`), the renderer's status poll queues at the MCP transport even though the underlying DB read would be instant.

3. **Agent quick-writes** (`knowledge_delete_by_source`, `apply_corrections`) sit behind any pending indexing-queue chunks on the same writer MCP server — potentially hours of wait during catch-up.

The latent serialization is in the MCP transport (one request at a time per server), not at the DB. With the Rust binary's `tokio::sync::RwLock` + Phase-split inside `add_episode` (only Phase D briefly holds the write lock), the DB-level serialization is appropriate. The MCP-level serialization is a bug in routing.

This is also a strategic re-framing. The knowledge graph has grown from "framework extension shipped per-workspace" to **core tech for liminis**. Bundling it into the app, owning the integration in TS, and dropping the Python middleware reflect that.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Renderer Status Polls Stay Responsive During Heavy Ingestion (Priority: P1)

The GraphitiPanel polls `knowledge_status` every few seconds. While the indexing queue processes a 100-chunk backlog (3+ hours of writes), the panel should still render fresh counts within 10ms p95.

**Why this priority**: Core UX regression — a frozen status panel signals "service down" to the user during the hours-long catch-up that follows a large ingestion job.

**Independent Test**: Start a long-running `process_chunk` simulation (e.g., 50 chunks queued). Poll `knowledge_status` every 1s from the renderer for 5 minutes. Assert p95 latency stays under 10ms and no status poll exceeds 100ms.

**Acceptance Scenarios**:

1. **Given** the indexing queue is mid-flight on a chunk, **When** the renderer requests `knowledge_status`, **Then** the response arrives in under 10ms.
2. **Given** the agent is mid-flight on a `knowledge_search_passages` call (embedding round-trip ~200ms), **When** the renderer requests `knowledge_status`, **Then** the renderer does not queue behind the agent; latency stays under 10ms.

---

### User Story 2 — Agent Quick-Writes Complete Promptly During Ingestion (Priority: P1)

The agent occasionally calls fast write methods (`knowledge_delete_by_source`, `knowledge_apply_corrections`). These must not queue behind the indexing backlog. They serialize at the Rust `write_lock` (correct), not at any MCP transport.

**Why this priority**: An agent correction that takes hours instead of seconds destroys the interactive workflow that makes liminis useful.

**Independent Test**: Queue 10 simulated chunks via the indexing path. Mid-backlog, invoke `knowledge_delete_by_source` from the agent. Assert it completes in under 1s plus one indexing-chunk's Phase-D commit window (~100ms).

**Acceptance Scenarios**:

1. **Given** the indexing queue has 10 chunks queued for processing, **When** the agent invokes a quick-write tool, **Then** the agent's write completes after at most the current chunk's Phase-D commit (~100ms wait), not after all 10 chunks finish.
2. **Given** the agent and the indexing queue both have writes in flight, **When** both complete, **Then** order is FIFO by `write_lock` acquisition, not by transport-layer arrival.

---

### User Story 3 — Indexing Queue Preserves Single-Chunk-at-a-Time Semantics Without MCP Serialization (Priority: P1)

Today the MCP transport implicitly limits the indexing queue to one chunk in flight. The new direct path must explicitly enforce that — the queue awaits each chunk's completion before sending the next, so the Rust binary's `write_lock` queue doesn't grow unbounded.

**Why this priority**: Removing the implicit MCP serialization without adding an explicit await would flood the Rust binary's write queue, exhaust memory, and cause out-of-order episode commits.

**Independent Test**: Drive the indexing queue with 5 chunks. Assert only one chunk is in the Rust binary's pending-write queue at any moment.

**Acceptance Scenarios**:

1. **Given** 5 chunks queued in the indexing queue, **When** processing runs, **Then** `processChunk(...)` is awaited per chunk, not fired-and-forgotten.
2. **Given** a `process_chunk` call fails mid-flight, **When** the queue handles the error, **Then** the queue retries or marks failed per existing policy — no chunks are skipped silently.

---

### User Story 4 — Agent Tool Calls Keep Going Through MCP with Permission Scoping Intact (Priority: P1)

The Claude Agent SDK still sees two MCP server names (`knowledge-reader`, `knowledge-writer`) for `.claude/settings.json` permission rules. The implementation moves from Python subprocess + stdio to in-process TS provider, but the SDK-facing surface is unchanged.

**Why this priority**: Permission scoping is a security boundary; breaking it would allow the agent to invoke write tools without user-granted permission.

**Independent Test**: With the new in-process MCP providers loaded, the agent in a `.claude/settings.json`-restricted session can still be allowed/denied `knowledge_*` tool calls by server name.

**Acceptance Scenarios**:

1. **Given** the in-process MCP providers are registered, **When** the agent invokes `knowledge_find_entities`, **Then** the call resolves through the in-process provider with the same parameter and response shape as the prior Python implementation.
2. **Given** `.claude/settings.json` denies `knowledge-writer`, **When** the agent attempts `knowledge_process_chunk`, **Then** the SDK rejects the call before it reaches the in-process provider.
3. **Given** the in-process MCP providers are registered, **When** the agent invokes `knowledge_rebuild_from_wal`, **Then** the call is rejected (the method is not exposed as an MCP tool — it is admin-only via the direct client).

---

### User Story 5 — Knowledge Graph Ships in the App Bundle, Not Per-Workspace (Priority: P1)

liminis-app bundles the `liminis-graph` binary and the embedder sidecar in app resources. Workspaces stop shipping `.claude/skills/knowledge-graph/scripts/*.py`. Workspaces hold data (`.graphiti/db`, `.graphiti/wal`); the app owns the implementation.

**Why this priority**: Strategic ownership shift — knowledge graph is core tech, not a workspace extension. Per-workspace shipping creates version skew, deployment complexity, and an impossible upgrade path.

**Independent Test**: Onboard a fresh workspace (no `.claude/skills/knowledge-graph/` present). The app spawns liminis-graph + embedder from bundled resources. The workspace gets a populated `.graphiti/db` on first ingestion.

**Acceptance Scenarios**:

1. **Given** a fresh workspace with no `.claude/skills/knowledge-graph/`, **When** liminis-app opens it, **Then** the knowledge graph still works — binary spawned from app bundle, embedder sidecar spawned from app bundle.
2. **Given** an existing workspace with the deprecated `.claude/skills/knowledge-graph/scripts/`, **When** liminis-app opens it, **Then** liminis-app ignores those scripts and uses bundled resources (workspace scripts are deprecated but harmless). A one-time deprecation log line is emitted.
3. **Given** the bundled binary is missing or corrupt, **When** the app starts the graphiti service, **Then** the error surfaces clearly to the user with a recovery hint, not a silent failure.

---

### Edge Cases

- **Connection pool exhaustion**: if all socket connections to the Rust binary are busy when a new call arrives, the call queues locally with a configurable timeout (default 30s); if the timeout expires, the call rejects with a typed `ConnectionPoolTimeout` error. No new connections are opened beyond the defined pool size.
- **Pipelining at the socket**: a writer connection sends one request at a time (pipeline depth = 1). The caller awaits the response before sending the next request. This preserves back-pressure and prevents the Rust binary's `write_lock` queue from growing unbounded.
- **Reader connection during write commit**: tokio's RwLock is write-preferring under contention; a queued writer blocks new readers. Separate reader connections per high-frequency caller (renderer-read, agent-read) limit worst-case reader latency to one Phase-D commit window (~100ms).
- **Lifecycle restart**: when liminis-graph crashes and restarts, the TS client detects the broken socket, rejects all in-flight requests with a typed `ServiceRestarted` error, and reconnects all pool connections before serving subsequent calls.
- **MCP provider error vs. direct client error**: errors from the in-process MCP provider propagate as MCP tool errors (visible to agent via SDK error surface); errors from the direct client propagate as TS exceptions (caught by renderer/queue). Both originate from the same socket errors but are surfaced differently.
- **Workspace migration**: existing workspaces with `.claude/skills/knowledge-graph/scripts/` are tolerated at runtime. The app emits a one-time deprecation log line per workspace open and ignores the scripts. File removal is left to the user.
- **Bundle size impact**: liminis-graph binary ~28MB. Embedder sidecar + model weights (per #39) could add 100–500MB. Accepted for a desktop app bundle; the embedder model is not lazy-downloaded (bundled for offline use). Revisit if bundle size becomes a distribution problem.
- **`knowledge_rebuild_from_wal` streaming**: this is an admin-only operation. It is not exposed as an MCP tool. It is only callable via `GraphitiSocketClient.rebuildFromWal(progressCallback)` on the `lifecycle` connection.

## Requirements *(mandatory)*

### Functional Requirements

#### `GraphitiSocketClient` (TS, in liminis-app/src/main)

- **FR-001**: New module `graphiti-socket-client.ts` providing a typed wrapper around the JSON-RPC-over-Unix-socket protocol the Rust binary speaks.
- **FR-002**: Exposes one TS method per Rust IPC method, with TS types matching the Rust response shapes. Examples: `client.findEntities(query, opts)`, `client.processChunk(args)`, `client.status()`.
- **FR-003**: Manages an internal connection pool with separate connections per logical subsystem: `indexing-write`, `agent-write`, `agent-read`, `renderer-read`, `renderer-write`, `lifecycle`. Each connection is a single tokio task on the Rust side.
- **FR-004**: Provides reconnection on socket-level errors (Rust binary restart). In-flight requests reject with a typed `ServiceRestarted` error; subsequent calls reconnect.
- **FR-005**: All methods return Promises. No fire-and-forget — callers must await for back-pressure.
- **FR-006**: `GraphitiSocketClient.rebuildFromWal(progressCallback)` accepts a callback, parses `{type: "progress", ...}` lines off the socket, invokes the callback per line, and resolves with the terminal response. This method runs on the `lifecycle` connection and is not exposed as an MCP tool.
- **FR-007**: When a connection pool slot is busy, new callers queue locally with a 30s timeout. Timeout expiry rejects with a typed `ConnectionPoolTimeout` error. No additional socket connections are opened beyond the defined pool.

#### In-Process MCP Providers (TS, in liminis-app)

- **FR-008**: Two MCP providers registered via Claude Agent SDK's in-process MCP API: `knowledge-reader` (exposes read tools) and `knowledge-writer` (exposes write tools). Tool surfaces match what the Python `reader_server.py` and `writer_server.py` currently expose, with the exception that `knowledge_rebuild_from_wal` is not exposed (admin-only, direct client only).
- **FR-009**: Each provider's tool handlers call `GraphitiSocketClient` methods on dedicated `agent-read` and `agent-write` connections. No shared in-flight state across providers.
- **FR-010**: Tool schemas (name, description, input schema) live in TS alongside the providers — not in workspace-shipped Python files.
- **FR-011**: Permission scoping by MCP server name preserved (`.claude/settings.json` rules continue to work).
- **FR-012**: `mcpDirectInvoker` no longer handles `knowledge-*` calls. Audit `liminis-app/src/main/` for any remaining `mcpDirectInvoker.callTool(KNOWLEDGE_*, ...)` call sites and migrate each to `graphitiClient.*` direct calls.

#### Indexing Queue Refactor

- **FR-013**: `liminis-app/src/main/indexing-queue.ts:1481` migrates from `mcpDirectInvoker.callTool('knowledge-writer', 'knowledge_process_chunk', args)` to `graphitiClient.processChunk(args)` on the `indexing-write` connection.
- **FR-014**: The queue's existing one-chunk-at-a-time semantics are preserved: each `processChunk` call is awaited before the next chunk is sent (no pipelining at the socket).
- **FR-015**: Existing retry / error / abort policies in the indexing queue are unchanged — only the call path changes.

#### Renderer IPC Refactor

- **FR-016**: All handlers in `liminis-app/src/main/ipc/graphiti-handlers.ts` migrate from `mcpDirectInvoker.callTool(KNOWLEDGE_*, ...)` to `graphitiClient.*` on `renderer-read` / `renderer-write` connections.
- **FR-017**: GraphitiPanel polling behavior is unchanged from the renderer's perspective.

#### Lifecycle Refactor

- **FR-018**: `graphiti-service-lifecycle.ts` health-probes use `graphitiClient.healthCheck()` on the `lifecycle` connection rather than spawning probe subprocesses or mcpDirectInvoker calls.
- **FR-019**: `prepare_checkpoint` (called from `workspace-checkpoint.ts`) uses `graphitiClient.prepareCheckpoint()` direct, not via MCP.

#### App Bundling

- **FR-020**: `liminis-graph` Rust binary is built and copied into `liminis-app/resources/` (or platform-equivalent) during `pnpm build`. Path resolves at runtime via `process.resourcesPath`.
- **FR-021**: Embedder sidecar (per #39) is similarly bundled and spawned by lifecycle when `backend = rust`.
- **FR-022**: `graphiti-service-lifecycle.ts` resolves the binary path from app resources, not from a workspace-relative `.claude/skills/` path.
- **FR-023**: Existing workspaces with `.claude/skills/knowledge-graph/scripts/` are tolerated at runtime. The app ignores those scripts and emits a one-time deprecation log line per workspace open.

#### Framework Boundary

- **FR-024**: `liminis-framework` no longer ships `graphiti_service.py`, `reader_server.py`, `writer_server.py`, `service_protocol.py`, `service_client.py`, or `common.py` from the knowledge-graph skill. The deprecation happens in a follow-up release after the cutover is validated.
- **FR-025**: The cut-release flow (`release_process.md`) shrinks from 4 repos to 3: liminis-app (which embeds liminis-graph binary), remarkable, clipper. The framework still exists but no longer owns knowledge-graph.

## Pre-Implementation Spikes (Research Stage)

These must be completed and filed as findings before the Plan stage:

1. **In-process MCP support in Claude Agent SDK (TS)**: Verify the SDK supports in-process MCP providers with the same tool-call semantics as stdio MCP. Confirm permission scoping by server name works for in-process providers. If any capability gap exists, document the workaround.
2. **`write_lock` scope in `episode::add_episode`**: Trace the actual lock scope in `liminis-graph-core/src/episode.rs`. Confirm only Phase D holds the write_lock. If the lock is wider, file a separate issue to narrow it before this refactor lands.
3. **Empirical "status during ingest" baseline**: Run the SC-001 scenario against the current Python stack to establish a baseline. The refactor should improve on it; the baseline number is required to validate SC-001.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Under a 50-chunk indexing backlog, p95 latency of `knowledge_status` from the renderer stays under 10ms (measured by a renderer-side timing wrapper).
- **SC-002**: An agent quick-write (`knowledge_delete_by_source`) invoked during a 50-chunk indexing backlog completes in under 1 second plus one Phase-D commit window (~100ms tolerance).
- **SC-003**: Total wall-time to process 10 chunks via the indexing queue is no worse than the Python baseline (within 10% — the LLM call dominates; transport savings should be invisible).
- **SC-004**: Agent tool calls through the in-process MCP providers behave identically to the current Python implementation: same tool names, parameters, response shapes, error shapes.
- **SC-005**: `.claude/settings.json` permission rules work unchanged after the refactor.
- **SC-006**: Onboarding a fresh workspace (no `.claude/skills/knowledge-graph/`) produces a working knowledge graph using bundled resources only.
- **SC-007**: The cut-release of liminis-app embeds the liminis-graph binary; the installed app works against an existing demo-notebook workspace without any framework-shipped Python scripts present.
- **SC-008**: No `mcpDirectInvoker.callTool(KNOWLEDGE_*, ...)` call sites remain in `liminis-app/src/`.

## Assumptions

- Claude Agent SDK's in-process MCP API exists and is stable in the TS SDK. (Verify in spike 1.)
- tokio::sync::RwLock semantics in liminis-graph give the right read/write isolation — multiple concurrent readers, one writer, brief Phase-D-only hold. (Verify in spike 2.)
- The Rust binary's main loop handles multiple concurrent connections without contention beyond the DB write_lock. (Already confirmed — per-connection tokio::spawn pattern in `main.rs:54-58`.)
- Bundle-with-app is the right ownership boundary for the knowledge graph (strategic decision).
- liminis-framework can lose the knowledge-graph skill without losing its reason to exist. If false, the deprecation step in FR-024 should be deferred or reshaped.
- `knowledge_rebuild_from_wal` is an admin operation, not an agent tool. It is not exposed via MCP; access is exclusively via `GraphitiSocketClient` on the `lifecycle` connection.
- A desktop app bundle of 200–500MB additional size (binary + embedder + model weights) is acceptable. Lazy model download is not required for v1.

## Out of Scope

- Removal of the Python `graphiti_service.py`, `reader_server.py`, `writer_server.py` from liminis-framework. (Deprecate first; remove in a follow-up release after one or more workspaces have validated the bundled path.)
- ANE/MLX-accelerated embedder — start with CPU sentence-transformers (Python parity); ANE follow-up is separate.
- Reorganization of liminis-framework's other skills (only the knowledge-graph skill is affected).
- Permission rule changes in `.claude/settings.json` — the existing scoping is preserved.
- Renderer UI changes — the GraphitiPanel and corrections UI consume the new direct client transparently.
- Cross-workspace concurrent access to one liminis-graph instance — out of scope; one instance per workspace as today.

## Source References

- `liminis-app/src/main/indexing-queue.ts:1481` — `mcpDirectInvoker.callTool` call site for `knowledge_process_chunk`
- `liminis-app/src/main/ipc/graphiti-handlers.ts` — renderer IPC handlers to migrate
- `liminis-app/src/main/graphiti-service-lifecycle.ts` — lifecycle manager; binary path resolution to migrate
- `liminis-app/src/main/workspace-checkpoint.ts` — `prepare_checkpoint` call site
- `liminis-graph-core/src/episode.rs` — `add_episode` pipeline; Phase-D write_lock scope (verify in spike 2)
- `liminis-graph/src/main.rs:54-58` — per-connection tokio::spawn pattern
- Python reference: `liminis-framework/framework/src/skills/knowledge-graph/scripts/reader_server.py`, `writer_server.py`
