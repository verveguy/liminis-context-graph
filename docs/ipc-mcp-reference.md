---
layout: default
title: IPC & MCP Reference
---

# IPC & MCP Reference

`liminis-context-graph` serves the same graph over **two transport surfaces**, both routed
through the same core dispatch in
[`crates/core/src/handlers.rs`](https://github.com/verveguy/liminis-context-graph/blob/main/crates/core/src/handlers.rs) —
no graph logic is duplicated between them:

- **JSON-RPC 2.0 over a Unix domain socket** (default). Newline-delimited requests/responses over `.lcg/service.sock`.
- **[Model Context Protocol](https://modelcontextprotocol.io) over stdin/stdout** (`--mcp-stdio`). Any MCP client — Claude Code, Claude Desktop, other agents — can query and mutate the graph directly.

## IPC methods (35)

The socket dispatch handles **35 methods**: 34 `knowledge_*` methods plus `health_check`.
`health_check` is the one method not prefixed `knowledge_*`, and it is the reason the IPC
surface (35) and the MCP tool registry (34, below) differ by exactly one — `health_check` is
not exposed as an MCP tool.

| Category | Methods |
|----------|---------|
| Health | `health_check` |
| Status | `knowledge_status`, `knowledge_rebuild_status` |
| Ingestion | `knowledge_process_chunk`, `knowledge_add_episode` |
| Search | `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_search_passages`, `knowledge_query_cypher` |
| Graph reads | `knowledge_get_episodes`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`, `knowledge_list_entities`, `knowledge_list_relationships`, `knowledge_get_entity_neighbors`, `knowledge_get_entities_by_source` |
| Deletion | `knowledge_delete_episode`, `knowledge_delete_by_source`, `knowledge_delete_chunk_episode`, `knowledge_clear_all` |
| Curation | `knowledge_merge_entities`, `knowledge_validate_corrections`, `knowledge_apply_corrections`, `knowledge_reprocess_entity_types` |
| Relation typing | `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types` (deprecated), `knowledge_reprocess_relation_types` |
| WAL administration | `knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_rebuild_from_wal`, `knowledge_build_indices` |
| Recovery / lifecycle | `knowledge_recover`, `knowledge_recover_full`, `knowledge_close` |

For request/response shapes and parameter details, the dispatch `match` arms in
[`handlers.rs`](https://github.com/verveguy/liminis-context-graph/blob/main/crates/core/src/handlers.rs)
and their handler functions are the source of truth — this page is the method index, not a
copy of each handler's parameter parsing.

Every long-running method above (the five WAL/recovery/reclassification operations most likely
to run for a while) accepts a `_progress_token` and streams `{"type":"progress",...}` frames
before the terminal result — see [Progress notifications](#progress-notifications) below.

## MCP-over-stdio transport

`liminis-context-graph --mcp-stdio` starts a native [Model Context Protocol](https://modelcontextprotocol.io)
server over stdin/stdout, using the official Rust SDK (`rmcp`). Any MCP client (Claude Code,
Claude Desktop, other agents) can query and mutate the knowledge graph directly — no Electron
app, no Node, no custom JSON-RPC client required. This is an *additional* external-facing
surface; the Unix-socket JSON-RPC protocol above is unchanged.

Every MCP tool is derived from the `knowledge_*` dispatch methods above — tool names match the
IPC method names verbatim, and each `tools/call` is translated into an `IpcRequest` and routed
straight through the same core dispatch the socket service uses. Tool descriptions and JSON
schemas are maintained in the
[`ToolSpec` registry in `crates/service/src/mcp/tools.rs`](https://github.com/verveguy/liminis-context-graph/blob/main/crates/service/src/mcp/tools.rs) —
that file is the canonical source for per-tool descriptions; they are not duplicated here.

### Flags

| Flag | Description |
|------|-------------|
| `--mcp-stdio` | Starts the MCP server over stdin/stdout instead of binding the Unix socket. |
| `--scope=<list>` | Comma-separated list of scopes to advertise in `tools/list` (default `all`). See [Scopes](#scopes) below. |
| `--connect <path>` | Attached mode: forward every `tools/call` as JSON-RPC over the given Unix socket to an already-running service, instead of opening the database directly. |
| `--allow-remote-close` | Attached mode only: advertise and allow `knowledge_close`, forwarding the shutdown to the remote service. No effect in standalone mode (no `--connect`). |

### DB-access modes

- **Standalone (default, no `--connect`)**: the MCP process opens the `.lcg` database directly,
  reusing the same startup and self-recovery path as the socket service ([ADR 0009](adr/0009-degraded-mode-startup-recovery.md)).
  Zero-dependency — works with no other process running.
- **Attached (`--connect <socket-path>`)**: the MCP process never opens the database; it
  forwards each call over the given socket to a service that already has it open. Use this to
  add MCP access to a workspace where another socket-service instance is already running,
  without contending for lbug's single-writer lock.
  - **Idle timeout.** `LCG_ATTACHED_CALL_TIMEOUT_MS` (default 30s) is a **per-read-line** idle
    timeout, not a whole-call timeout: it resets on every line read off the socket, including
    `{"type":"progress"}` lines. A call that keeps emitting progress is never bounded by it, no
    matter how long the call runs in total — only genuine silence (no output at all for the full
    timeout window) trips it. If the remote stops responding mid-call (e.g. it crashes), the
    attached client fails that call with a clean timeout error rather than blocking forever.
  - **Reconnect and retry.** If the connection to the remote breaks, the client transparently
    re-dials the same socket path rather than staying wedged. If the break is detected while
    writing the outgoing request — treated as safe to retry, since the write failing is the
    client's best available signal that the request didn't get through — the client
    automatically retries that request exactly once over the freshly-dialed connection. If the
    break is detected only after the request was fully written — while waiting for or reading
    the response — the call is **not** retried automatically, since the remote's execution
    status is unknown and blind retry could double-apply a non-idempotent write (e.g.
    `knowledge_add_episode`); that call fails with a clear "connection lost mid-call" error, but
    the connection is marked dead so the *next* call reconnects fresh. If a reconnect attempt
    itself fails (no listener at that path), the call fails with a clear, descriptive error —
    never a hang — and a later call will try reconnecting again. See
    [ADR-0040](adr/0040-attached-mode-reconnect-retry-boundary.md) for the full rationale.

### Scopes

Scopes are additive and composable (e.g. `--scope=read,admin`). `tools/list` advertises the
union of all active scopes.

| Scope | Methods |
|-------|---------|
| `read` | `knowledge_status`, `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_get_episodes`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`, `knowledge_search_passages`, `knowledge_list_entities`, `knowledge_list_relationships`, `knowledge_get_entity_neighbors`, `knowledge_get_entities_by_source`, `knowledge_rebuild_status`, `knowledge_validate_corrections` |
| `write` | `knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_delete_episode`, `knowledge_delete_by_source`, `knowledge_delete_chunk_episode`, `knowledge_clear_all`, `knowledge_apply_corrections`, `knowledge_merge_entities`, `knowledge_reprocess_entity_types`, `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types`, `knowledge_reprocess_relation_types` |
| `cypher` | `knowledge_query_cypher` |
| `admin` | `knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_rebuild_from_wal`, `knowledge_recover`, `knowledge_recover_full`, `knowledge_close`, `knowledge_build_indices` |
| `all` | every scope above (default) |

**`cypher` is a power scope, not bundled into anything else.** `knowledge_query_cypher` executes
raw Cypher with no param interpolation or value coercion — despite being a "query" method, it can
perform arbitrary mutations, and it bypasses the WAL-ordering and embedding invariants that the
structured write tools maintain. It is never implicitly included in `read`, `write`, or `admin`;
operators must opt in explicitly (or via `all`).

**`knowledge_close` in attached mode is a footgun without `--allow-remote-close`.** In
**standalone** mode, `knowledge_close` is always advertised under `admin` scope and shuts down
only this MCP process's own DB connection. In **attached** mode, calling it would shut down the
*running remote service*. Without `--allow-remote-close`, `knowledge_close` is omitted from
`tools/list` entirely in attached mode (not merely rejected when called). Pass
`--allow-remote-close` only when you specifically intend this MCP connection to be able to stop
the remote service.

**Recovery and export live under `admin`.** `knowledge_rebuild_from_wal` (rebuild the graph from
the WAL), `knowledge_dump_wal` (snapshot/export the graph into a fresh compacted WAL directory),
and `knowledge_recover` / `knowledge_recover_full` are all `admin`-scope tools — an attached client
only sees them when launched with `--scope=admin` (or `all`). If a mutation goes wrong, this is the
recovery path. See [Operations](operations.md) for the recovery model in full. Note the WAL replays
**forward-only**, so take periodic `knowledge_dump_wal` snapshots if you want restore points
before large or destructive operations.

**`knowledge_rebuild_from_wal` refuses to run against a non-empty database, unless you ask it
not to.** A `from_seq: 0` (default) full rebuild against a database that already contains data
fails fast with an explicit error rather than silently emitting a duplicate-primary-key failure
for every existing `Entity`/`Episodic`/`RelatesToNode_` row — the native write path uses `CREATE`,
not `MERGE`, for those labels. Pass `force_clear: true` to have the call clear the database itself
before replaying (the same DB-file-delete-and-reopen behavior `knowledge_recover`'s
`rebuild_from_workspace_wal` strategy uses), or clear it yourself first with `knowledge_clear_all`.
`dry_run: true` always fails fast on a non-empty database regardless of `force_clear`, since a dry
run must never mutate the database — this lets a preview surface the problem before you commit to
a real rebuild. None of this applies to an incremental `from_seq > 0` resume, which intentionally
targets a database that already has state.

### Relation typing (`canonicalize_relations`, `backfill_relation_types`, `reprocess_relation_types`)

Three tools populate an edge's `relation_type`, with different tradeoffs:

**`knowledge_canonicalize_relations`** maps each edge's **existing raw predicate** onto your
ontology's declared `relation_types`. Its behavior has three caveats worth knowing before you
rely on it:

- **The primary pass is lexical, over the predicate — not the `fact`.** It matches the edge's
  predicate / current `relation_type` against ontology type names, aliases, and keywords. It does
  **not** read the edge's `fact` sentence, so an edge whose `relation_type` was cleared cannot be
  re-mapped from its fact by this pass.
- **`embedding_threshold` tunes only the fallback promoter** (default `0.7`). The fallback embeds
  each residual edge's `fact` against the ontology types' *descriptions* and force-assigns the single
  nearest type at or above the threshold. Lowering it types more edges, but by nearest-neighbor
  force-fit with **no abstention** — an idiosyncratic fact (e.g. "*X is affiliated with Y*") can land
  on a wrong type (e.g. `HOLDS`).
- **Re-runs are only partly idempotent, and clearing `relation_type` can't be undone by
  canonicalize.** A re-run skips an edge only when it's already at its target — a `Mapped` edge
  already equal to the canonical type, or a residual edge already `UNCLASSIFIED`; an edge whose
  classification *changes* is overwritten (including a previously-assigned type), while
  arrow-named "noise" edges keep any existing predicate. Critically, canonicalize's only input is
  the edge's existing predicate / `relation_type` — if you **null that field to "start clean,"
  canonicalize has nothing to map from and cannot rebuild it.** Snapshot with `knowledge_dump_wal`
  before such an operation.

**`knowledge_backfill_relation_types`** (DEPRECATED) does not classify at all — it mints
uppercased fact-prefix pseudo-types (e.g. `THE_SPECIFICATION_DOCUMENT_DEFINES`) for edges with no
`relation_type`, rather than matching against the ontology. Avoid it for building a typed
taxonomy; prefer `knowledge_reprocess_relation_types` below, which supersedes it for that purpose.

**`knowledge_reprocess_relation_types`** is the relation-side twin of
`knowledge_reprocess_entity_types`: for each in-scope edge, it sends the edge's `fact` and the
ontology's declared relation types (name + description) to the configured extraction LLM and asks
it to pick exactly one type, or honestly abstain. This is the tool to reach for when you want
genuine fact-based classification instead of lexical matching or pseudo-typing:

- **Always reads the `fact`, never the predicate.** Unlike `canonicalize_relations`, classification
  is grounded in the edge's natural-language fact sentence against the ontology's declared menu —
  not lexical/alias/keyword matching on the existing predicate string.
- **A declared ontology relation-type menu is always required.** Unlike
  `knowledge_reprocess_entity_types` (whose `untyped` scope works with no ontology via open-ended
  classification), every scope value (`untyped`, `off_ontology`, `all`) fails with a structured
  `{success: false, error: ...}` if the ontology declares no relation types — there is no
  open-ended fallback (see [ADR-0037](adr/0037-relation-classification-abstention-writes-unclassified.md)).
- **Abstention is an honest, real write of `UNCLASSIFIED`.** If the LLM cannot map a fact to any
  declared type, the edge's `relation_type` is set to the literal string `UNCLASSIFIED` — never a
  force-assigned nearest match. This differs from `knowledge_reprocess_entity_types`, where an
  unclassifiable entity is simply left unchanged (see ADR-0037).
- **`scope`** controls candidates: `"untyped"` (default) — `relation_type` NULL/empty, the same
  predicate `backfill_relation_types` uses; `"off_ontology"` — untyped edges plus edges whose
  `relation_type` isn't a declared type (this naturally covers prior `UNCLASSIFIED` sentinels and
  `backfill_relation_types`'s fact-prefix pseudo-types with no special-casing); `"all"` — every
  edge in the group.
- **Idempotent.** An edge whose computed verdict already matches its current `relation_type`
  (including an edge already correctly `UNCLASSIFIED`) is left unchanged — no write, no WAL entry.
- **`dry_run: true`** returns `would_reclassify_count`, a `plan` array of per-edge
  `{edge_id, fact, old_type, new_type}` entries, and a `breakdown` object counting edges per
  assigned `new_type` (including an `UNCLASSIFIED` count) — without mutating the graph.

### Progress notifications

The five long-running operations — `knowledge_rebuild_from_wal`, `knowledge_canonicalize_relations`,
`knowledge_backfill_relation_types`, `knowledge_reprocess_relation_types`, and
`knowledge_reprocess_entity_types` — bridge to MCP progress notifications when the client
attaches a progress token to the `tools/call` request (`_meta.progressToken`), in both
standalone and attached mode. Without a progress token, these calls simply block until they
complete, same as over the socket protocol. In attached mode, each progress notification also
re-arms `LCG_ATTACHED_CALL_TIMEOUT_MS`'s per-read-line idle timer (see
[DB-access modes](#db-access-modes) above), so a progress-tracked call isn't falsely reported as
timed out just because it runs longer than that timeout in total.

### Example MCP client config

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--scope=read,write"],
      "cwd": "/path/to/your/workspace"
    }
  }
}
```

To attach to an already-running socket service instead of opening the DB directly:

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--connect", "/path/to/your/workspace/.lcg/service.sock", "--scope=read"]
    }
  }
}
```

See [ADR 0035](adr/0035-mcp-stdio-transport.md) for the transport's internal architecture.
