# Feature Specification: Native MCP-over-stdio transport for the liminis-context-graph binary

**Feature Branch**: `fabrik/issue-195`
**Created**: 2026-07-21
**Status**: Draft
**Input**: User description: "Add a native Rust MCP-over-stdio transport (`--mcp-stdio`, via `rmcp`) to the `liminis-context-graph` binary so any MCP client can query and mutate the knowledge graph directly, with no Electron/Node dependency. Executes the `--mcp-stdio`/`rmcp` item explicitly deferred in `specs/120-oss-launch-scaffolding-license`."

## Background

The OSS `liminis-context-graph` binary today exposes its graph only over a Unix-socket JSON-RPC 2.0 protocol. Any Model Context Protocol (MCP) client (Claude Code, Claude Desktop, other agents) cannot talk to it directly — MCP wrapping currently lives only in the closed Liminis Electron app, as thin TypeScript proxy servers (`knowledge-reader`, `knowledge-writer`) built on the Claude Agent SDK and reachable only from inside that app.

This feature adds a **native Rust MCP-over-stdio transport** to the binary so any MCP client can use the graph with no Electron/Node dependency. It executes the `--mcp-stdio` / `rmcp` item explicitly deferred in the OSS-launch scaffolding spec (`specs/120-oss-launch-scaffolding-license`, which lists it as: *"MCP-over-stdio transport (`rmcp` integration, `--mcp-stdio` flag, tool wiring) — Fabrik issue, separate"*).

The MCP shell is a new transport in `crates/service`; the core dispatch (`crates/core/src/handlers.rs`) is untouched. The existing app path (direct pooled socket) is unchanged — this is an additional external-facing surface, not a replacement for the TS proxies.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Zero-dependency standalone MCP server (Priority: P1)

A developer with a `.lcg` workspace and no Liminis app running starts `liminis-context-graph --mcp-stdio` and points any MCP client at it. The client discovers the graph's tools and can both query and mutate the graph — no Electron app, no Node, no custom JSON-RPC client required.

**Why this priority**: This is the core value proposition of the issue — it's what makes the OSS binary usable by the broader MCP ecosystem at all. Without this, nothing else in the issue matters.

**Independent Test**: Run `liminis-context-graph --mcp-stdio` in a workspace with a `.lcg` graph, connect any MCP client, call `tools/list`, then call one read tool and one write tool and confirm both succeed with results equivalent to the same operations over the existing Unix-socket protocol.

**Acceptance Scenarios**:

1. **Given** a workspace with a `.lcg` graph and no other process holding the DB, **When** a user runs `liminis-context-graph --mcp-stdio`, **Then** an MCP server starts over stdin/stdout and stays available for the life of the client connection.
2. **Given** the MCP server is running, **When** the client calls `tools/list`, **Then** it receives a list of tools, each with a name, description, and JSON input schema, derived from the existing `knowledge_*` dispatch methods.
3. **Given** the MCP server is running, **When** the client calls `tools/call` for a read tool (e.g. entity search) and a write tool (e.g. add episode), **Then** each executes against the graph and returns a structured result with semantics identical to the equivalent Unix-socket JSON-RPC method.

---

### User Story 2 - Attach to a running app instance without lock conflicts (Priority: P2)

An operator who already has the Liminis app (or another process) running the socket service wants MCP access to the same graph without disrupting it. They start `liminis-context-graph --mcp-stdio --connect .lcg/service.sock`, and the MCP process forwards calls to the already-running service instead of opening the database itself.

**Why this priority**: Without this, adding MCP access to a workspace that already has the app open would require killing the app or hitting lbug's single-writer lock conflict — a real deployment blocker for anyone running both side by side.

**Independent Test**: With a socket service already holding the DB open, run `liminis-context-graph --mcp-stdio --connect .lcg/service.sock`, issue the same read query through both the MCP tool and directly over the socket, and confirm equivalent results with no lock-conflict error from either process.

**Acceptance Scenarios**:

1. **Given** a running socket service holding the `.lcg` DB open, **When** a user starts `liminis-context-graph --mcp-stdio --connect <socket-path>`, **Then** the MCP process does not attempt to open the DB itself and instead forwards `tools/call` invocations as JSON-RPC over the given socket.
2. **Given** attached mode is running alongside the socket service, **When** the same read query is issued through the MCP tool and directly over the socket, **Then** both return equivalent results and neither process reports a lock conflict.

---

### User Story 3 - Restrict the advertised tool surface by scope (Priority: P2)

An operator handing MCP access to a less-trusted client (e.g. a read-only monitoring agent) wants to advertise only a subset of tools, so the client cannot see or call mutation, arbitrary-query, or admin operations even if it tried.

**Why this priority**: Capability separation is a real operational need once MCP access is handed to a third party or a less-trusted agent — without it, every MCP client gets the full read/write/cypher/admin surface by default, which is an unnecessary blast radius.

**Independent Test**: Start the server with `--scope=read` and confirm `tools/list` contains only query/status tools and that attempting to invoke a write or cypher tool is rejected cleanly (not silently ignored, not a crash). Repeat for `--scope=admin`, `--scope=cypher`, and `--scope=read,admin`.

**Acceptance Scenarios**:

1. **Given** the server is started with `--scope=read`, **When** the client calls `tools/list`, **Then** only query/status tools are advertised, and a `tools/call` attempt against a write, cypher, or admin tool name is rejected with a clear error.
2. **Given** the server is started with `--scope=admin`, **When** the client calls `tools/list`, **Then** the WAL/lifecycle/recovery/index-maintenance tools (`knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_rebuild_from_wal`, `knowledge_recover`, `knowledge_recover_full`, `knowledge_close`, `knowledge_build_indices`) are advertised.
3. **Given** the server is started with `--scope=cypher`, **When** the client calls `tools/list`, **Then** only `knowledge_query_cypher` is advertised.
4. **Given** the server is started with `--scope=read,admin`, **When** the client calls `tools/list`, **Then** the union of both scopes' tools is advertised.
5. **Given** the server is started in **attached** mode (`--connect`) with admin scope active and `--allow-remote-close` NOT passed, **When** the client calls `tools/list`, **Then** `knowledge_close` is omitted from the list entirely — it is not merely rejected on call, it does not appear.
6. **Given** the server is started in **attached** mode with admin scope active and `--allow-remote-close` IS passed, **When** the client calls `tools/list` and then calls `knowledge_close`, **Then** the tool is advertised and, when called, forwards the shutdown to the remote service.
7. **Given** the server is started in **standalone** mode (no `--connect`) with admin scope active, **When** the client calls `tools/list` and then calls `knowledge_close`, **Then** the tool is always advertised and, when called, shuts down only the MCP process's own DB connection — this is unaffected by `--allow-remote-close`.

---

### User Story 4 - See progress on long-running operations (Priority: P3)

A user triggers a rebuild or backfill operation through an MCP client. Instead of the client appearing to hang for the duration of the operation, it receives progress notifications and can show the user that work is proceeding.

**Why this priority**: Lower priority than correctness and safety, but important for usability — these operations can take a long time on large graphs, and a silently blocking MCP call is easy to mistake for a hang.

**Independent Test**: Trigger `knowledge_rebuild_from_wal` (or `knowledge_canonicalize_relations` / `knowledge_backfill_relation_types`) via MCP with a progress token attached, and confirm at least one progress notification arrives before the terminal result, in both standalone and attached modes.

**Acceptance Scenarios**:

1. **Given** a client calls a streaming tool (rebuild, canonicalize, or backfill) with a progress token, **When** the operation is running, **Then** the client receives one or more MCP progress notifications before the terminal result.
2. **Given** the server is in attached mode, **When** the same streaming tool is called, **Then** progress notifications are bridged from the socket's `{"type":"progress"}` messages to MCP notifications identically to standalone mode.

---

### Edge Cases

- What happens when `--mcp-stdio` is combined with a `.lcg` workspace that is missing, corrupt, or in degraded mode? (The existing degraded-mode guard that restricts methods when the DB is unavailable must still apply — see FR-008.)
- What happens when `--connect <path>` points at a socket that doesn't exist or has no listener? (Should fail fast with a clear error, not hang.)
- What happens when `--scope` is given an unrecognized scope name (e.g. `--scope=bogus`)?
- What happens when `tools/call` is invoked with missing or malformed required arguments for a tool's input schema?
- What happens when a progress-token operation's MCP client disconnects mid-stream? (No requirement to make the underlying operation cancellable if it isn't already — but the transport must not crash or leak the operation.)
- What happens when `--allow-remote-close` is passed in standalone mode (no `--connect`)? It has no effect — `knowledge_close` is already always advertised under `admin` scope in standalone mode regardless of the flag; `--allow-remote-close` only changes behavior in attached mode (FR-005).
- What happens when a core JSON-RPC error occurs mid-call (parse error, generic `-32000`, DB-unavailable `-32001`, degraded-mode rejection)? Each must surface as a well-formed MCP tool error (FR-008), never an opaque transport failure or crash.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The binary MUST accept a `--mcp-stdio` flag that starts an MCP server over stdin/stdout using `rmcp` (the official Rust MCP SDK), instead of binding the Unix socket.
- **FR-002**: `tools/list` MUST be derived from the existing `knowledge_*` dispatch methods in `crates/core/src/handlers.rs`. Tool names, descriptions, and JSON input schemas MUST be defined once, in Rust, with no hand-maintained second schema. Descriptions should be lifted from the app's current zod tool defs so the OSS surface matches what the app ships (see Assumptions for a fallback if that source is not accessible).
- **FR-003**: Each `tools/call` MUST translate its arguments into an `IpcRequest` and invoke the existing `handle(method, params)` core dispatch, returning the result as MCP tool output. No graph logic may be duplicated in the transport shell.
- **FR-004**: A `--scope` flag (default `all`) MUST select which subset of tools is advertised. Scopes are additive and composable (e.g. `--scope=read,admin`). `cypher` is a dedicated scope for `knowledge_query_cypher` — an "arbitrary query/mutation" power scope, kept separate from `read`, `write`, and `admin` because raw Cypher can bypass the invariants (WAL ordering, embeddings) that structured writes maintain; operators must opt into it explicitly (or via `all`), it is never implicitly bundled into another scope. `knowledge_build_indices` is operational index maintenance and belongs in `admin`, alongside WAL/checkpoint/recovery — `write` is reserved for content mutations only. The full method-to-scope mapping is:

  | Scope | Methods |
  |---|---|
  | `read` | `knowledge_status`, `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_get_episodes`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`, `knowledge_search_passages`, `knowledge_list_entities`, `knowledge_list_relationships`, `knowledge_get_entity_neighbors`, `knowledge_get_entities_by_source`, `knowledge_rebuild_status`, `knowledge_validate_corrections` |
  | `write` | `knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_delete_episode`, `knowledge_delete_by_source`, `knowledge_delete_chunk_episode`, `knowledge_clear_all`, `knowledge_apply_corrections`, `knowledge_merge_entities`, `knowledge_reprocess_entity_types`, `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types` |
  | `cypher` | `knowledge_query_cypher` |
  | `admin` | `knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_rebuild_from_wal`, `knowledge_recover`, `knowledge_recover_full`, `knowledge_close`, `knowledge_build_indices` |
  | `all` | every scope |

- **FR-005**: `knowledge_close` triggers graceful service shutdown.
  - In **standalone** mode, `knowledge_close` shuts down only the MCP process's own DB connection; it is always advertised under `admin` scope, and `--allow-remote-close` has no effect on standalone behavior.
  - In **attached** (`--connect`) mode, `knowledge_close` would shut down the *running* remote service — including the Liminis app's — which is a footgun. Without `--allow-remote-close`, `knowledge_close` MUST be omitted entirely from `tools/list` in attached mode (not merely rejected when called). With `--allow-remote-close` passed, it MUST be advertised and, when called, forward the close to the remote service.
  This behavior must be documented (FR-009).
- **FR-006**: The system MUST support two DB-access modes:
  - **Standalone (default)**: with no `--connect`, the MCP process opens the `.lcg` database directly, reusing the same startup and self-recovery path as the socket service (ADR-0009). Zero-dependency for OSS users with no app running.
  - **Attached**: with `--connect <socket-path>`, the MCP process does not open the DB; it forwards each `tools/call` as JSON-RPC over the given Unix socket to a running service, so it can coexist with the Liminis app without contending for lbug's single-writer lock.
- **FR-007**: Streaming operations (`knowledge_rebuild_from_wal`, `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types`) that emit `{"type":"progress"}` on the socket MUST be bridged to MCP progress-token notifications, in both DB-access modes.
- **FR-008**: Core JSON-RPC errors (parse errors, generic `-32000`, DB-unavailable `-32001`, degraded-mode rejections) MUST be surfaced as well-formed MCP tool errors, never opaque failures. The existing degraded-mode guard that restricts methods when the DB is unavailable must be respected identically by the MCP layer.
- **FR-009**: README / usage docs MUST document `--mcp-stdio`, `--scope` (including the `admin` and `cypher` scopes), `--connect`, and `--allow-remote-close`, including an example MCP client config stanza and explicit notes on both footguns: the admin-scope / remote-close guard (FR-005), and the `cypher` scope's ability to bypass the invariants that structured writes maintain (FR-004).

### Key Entities

- **Tool**: an MCP tool derived from a `knowledge_*` dispatch method — has a name, description, JSON input schema, and scope-bucket membership (see FR-004's table).
- **Scope**: one of `read`, `write`, `cypher`, `admin`, or the composite `all`; controls which tools are advertised in `tools/list`. Scopes are additive. `cypher` is a distinct power scope for the arbitrary-query/mutation escape hatch (`knowledge_query_cypher`), never implicitly bundled into `read`, `write`, or `admin`.
- **DB-access mode**: either `standalone` (this process opens `.lcg` directly) or `attached` (this process forwards calls over a Unix socket to a service that already has the DB open).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `liminis-context-graph --mcp-stdio` starts an MCP server that an MCP client can connect to, list tools from, and successfully call read and write tools against a real `.lcg` workspace.
- **SC-002**: `--connect <sock>` mode produces byte-for-byte equivalent tool results to standalone mode for the same read query, while a socket service holds the DB open — no lock conflict.
- **SC-003**: `--scope=read` advertises only query/status tools; attempting a write or cypher tool under read scope is rejected cleanly. `--scope=admin` advertises the WAL/lifecycle/recovery/index-maintenance tools (including `knowledge_build_indices`). `--scope=cypher` advertises only `knowledge_query_cypher`. `--scope=read,admin` advertises the union of both sets.
- **SC-004**: In attached (`--connect`) mode with admin scope, `knowledge_close` is omitted from `tools/list` unless `--allow-remote-close` is passed, in which case it is advertised and callable against the remote service. In standalone mode, `knowledge_close` is always advertised under `admin` scope and shuts down the MCP process cleanly, unaffected by `--allow-remote-close`.
- **SC-005**: A rebuild triggered via MCP surfaces at least one progress notification before the terminal result.
- **SC-006**: The tool set advertised over MCP is a superset of the app's current `knowledge-reader` + `knowledge-writer` tools (excluding the dead Python-only tools that Rust never implemented), verified against the zod defs (or, per the Assumptions fallback, against this spec's FR list if the app repo is not accessible to the verifying stage).
- **SC-007**: No regression to the existing socket-service path — existing IPC/parity tests still pass.

## Assumptions

- `rmcp` (the official Rust MCP SDK) is available as a crates.io dependency compatible with this project's async runtime (tokio) and MSRV; it is not yet a dependency of this workspace.
- Standalone mode reuses the existing degraded-mode startup and self-recovery path (ADR-0009) unchanged — no new recovery logic is introduced by this feature.
- The referenced TypeScript tool definitions (`liminis-app/src/main/mcp-providers/knowledge-reader-provider.ts`, `knowledge-writer-provider.ts`) live in a separate, closed-source repository, not in this OSS checkout. If those files are not reachable from the Fabrik Research/Plan/Implement execution environment, tool descriptions and schemas should instead be authored directly from this spec's FR list and from `handlers.rs` behavior, and SC-006's "verified against the zod defs" comparison becomes a manual, human-driven verification step rather than one the pipeline can automate.
- Only the stdio MCP transport is in scope; no HTTP/SSE transport.
- One MCP client connects per process invocation — stdio is inherently single-stream, so this is a single-client transport by nature, matching existing MCP stdio server conventions elsewhere.
- The existing 26 `knowledge_*` dispatch methods in `crates/core/src/handlers.rs` are the complete and final set this issue exposes; no new graph operations are introduced.

## Out of Scope

- Any change to the Unix-socket JSON-RPC protocol or the core dispatch in `handlers.rs`.
- Any change to the Liminis app's MCP providers — the app keeps its direct pooled socket (better concurrency; MCP stdio is single-stream). The TS proxies are not removed.
- An HTTP/SSE MCP transport (stdio only for this issue).
- New graph operations — this exposes the existing method set, nothing more.

## Source References

- `specs/120-oss-launch-scaffolding-license/spec.md` — the OSS-launch scaffolding spec that explicitly defers this work.
- `crates/core/src/handlers.rs` — core dispatch; the 26 `knowledge_*` methods this issue exposes were enumerated directly from this file during specification.
- `crates/service/src/main.rs` — existing CLI flag parsing pattern (manual argv scanning, no `clap` dependency today) and socket-service bootstrap that standalone MCP mode should mirror.
- `liminis-app/src/main/mcp-providers/knowledge-reader-provider.ts`, `knowledge-writer-provider.ts` — external repo; reference for tool names/descriptions/schemas (see Assumptions on accessibility).
- ADR-0009 — degraded-mode startup & recovery, reused unchanged by standalone mode.
