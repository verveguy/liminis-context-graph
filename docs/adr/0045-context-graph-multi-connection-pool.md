# ADR 0045: Named Multi-Connection Pool for ContextGraphSocketClient

**Date**: 2026-05-22
**Status**: Accepted
**Issue**: #42

## Context

The liminis-app integrated with the Liminis Context Graph through Python stdio MCP servers
(`reader_server.py`, `writer_server.py`) proxied via `mcpDirectInvoker`. Each stdio server entry
enforced one-in-flight-at-a-time serialization via an `inFlight` flag. This meant:

- The renderer's `knowledge_status` poll (near-instant on the Rust binary) queued behind a
  120-second `knowledge_process_chunk` call in the indexing queue.
- Agent quick-writes (`knowledge_delete_by_source`, `knowledge_apply_corrections`) queued behind
  the entire indexing backlog.
- The Rust binary's `tokio::sync::RwLock` provided correct DB-level serialization, but the MCP
  transport layer imposed unnecessary process-level serialization on top.

The fix is to eliminate the MCP transport and connect directly to the Rust binary's Unix socket
server via separate, independently-owned connections — one per logical subsystem.

## Decision

`ContextGraphSocketClient` manages **6 fixed, named connections** to the Rust binary's Unix socket:

| Connection Name   | Owner / Caller               | Semantics                                    |
|-------------------|------------------------------|----------------------------------------------|
| `indexing-write`  | IndexingQueue                | One chunk in-flight at a time (explicit await) |
| `agent-write`     | In-process knowledge-writer MCP provider | Serialized at Rust write_lock       |
| `agent-read`      | In-process knowledge-reader MCP provider | Concurrent reads                    |
| `renderer-read`   | IPC contextGraph/graph handlers  | Concurrent reads                             |
| `renderer-write`  | IPC contextGraph/graph handlers  | Serialized at Rust write_lock                |
| `lifecycle`       | Lifecycle manager            | health_check, prepare_checkpoint, rebuildFromWal |

The 6 connections are fixed at construction — no dynamic pool growth. Adding a new call site
requires explicitly choosing a connection, creating intentional friction that prevents the
serialization bug from recurring.

## Why Named Connections Over a Generic Pool

A generic pool of N identical connections would allow any caller to grab any slot. The failure
mode is identical to the current bug: a slow caller on any slot blocks fast callers waiting for
a free slot. Named connections enforce the subsystem isolation contract at the call site level.

## Write Lock Bounds

The Rust binary uses `tokio::sync::RwLock<()>` (write-preferring):

- Phases A and B of `add_episode` hold **no lock** (HTTP embedding + LLM extraction, dedup).
- Phase C (DB commit in `spawn_blocking`) holds the **write lock** for ~100ms typical.
- `knowledge_status` acquires a **read lock** — waits only during Phase C.

With separate connections, a `renderer-read` status call queues only behind Phase C of an in-flight
write on another connection, not behind the entire ~120s processing time.

Worst case with both `indexing-write` and `agent-write` in Phase C simultaneously: readers wait
~200ms. The spec's 10ms p95 target holds under typical load (only one writer in Phase C at a time).

## `isRebuilding` Flag

WAL rebuild (`knowledge_rebuild_from_wal`) runs on the `lifecycle` connection and streams progress
lines per ADR-043. During a rebuild, live `knowledge_status` calls on `renderer-read` would return
stale data. `ContextGraphSocketClient` exposes `isRebuilding: boolean`, set synchronously on entry to
`rebuildFromWal()` and cleared on exit. Callers check this to skip live status polls and return
cached data with `rebuilding: true`.

The lifecycle manager must also check `isRebuilding` before issuing `healthCheck()` on the
`lifecycle` connection. While a rebuild is in progress, the `lifecycle` connection slot is occupied;
a concurrent `healthCheck()` would queue behind it and time out at the 30-second slot limit, which
the lifecycle manager could misinterpret as the service being dead and trigger a restart loop. The
correct behaviour is to suppress health probes while `isRebuilding` is true — the rebuild is a
controlled operation, not an error state.

## Python-Only Tools

Six tools from the Python servers (`knowledge_index_document`, `knowledge_process_document`,
`knowledge_list_sources`, `knowledge_preview_chunks`, `knowledge_suggest_duplicates`,
`knowledge_entity_edge_analysis`) are not yet implemented in the Rust binary. They are included in
the in-process MCP providers and the `ContextGraphSocketClient` method surface, but the Rust binary
returns `-32000 Method not found` for them. Callers handle this gracefully. A follow-up issue will
add these methods to the Rust binary.

## Connection Pool Timeout

When a named connection slot is in-flight, subsequent callers for that slot queue locally with a
30-second timeout. On timeout, the call rejects with `ConnectionPoolTimeout`. On socket error, all
in-flight requests reject with `ServiceRestarted`. Both are typed errors, not generic `Error`
instances.

## Consequences

- Renderer status polls are no longer blocked by indexing-queue writes.
- Agent quick-writes serialize only at the Rust `write_lock` (Phase C, ~100ms), not at the MCP transport.
- The `indexing-write` connection enforces single-chunk-at-a-time semantics through explicit `await` in the queue loop (FR-014), not through transport serialization.
- The Rust binary's per-connection `tokio::spawn` pattern handles all 6 connections concurrently.
- `mcpDirectInvoker` no longer handles `knowledge-*` calls (SC-008).
