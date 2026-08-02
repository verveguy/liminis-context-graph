---
layout: default
title: Operations
---

# Operations

## On-disk layout

Everything the service manages lives under `.lcg/` in the workspace:

```
.lcg/
├── wal/               # append-only JSONL mutation log — the durable record (git-friendly)
├── db/liminis.db      # LadybugDB files — a derived index, rebuildable from the WAL
├── ontology.yaml       # optional extraction vocabulary (yours to edit)
└── service.sock       # JSON-RPC 2.0 endpoint while the service runs
```

**The write-ahead log is the source of truth — and it's just JSON.** Every mutation is appended
to plain JSONL files in `.lcg/wal/` before it touches the database. The WAL is human-readable,
append-only, and git-friendly: check it into the same repository as your notes or documents,
diff it, and carry it across machines. The database is a derived index — delete it and
`knowledge_rebuild_from_wal` reconstructs the entire graph from the log.

## WAL administration

- **Rebuild** the database from the log with `knowledge_rebuild_from_wal`. A `from_seq: 0`
  (default) rebuild against a database that already has data in it fails fast with an explicit
  error instead of silently producing a duplicate-key failure per node — pass
  `force_clear: true` to clear it automatically first, or delete it yourself before calling
  rebuild. A successful non-dry-run rebuild automatically rebuilds the entity/relationship
  search indices, so `knowledge_find_entities`/`knowledge_find_relationships` are immediately
  queryable afterward — `knowledge_build_indices` is not normally required.
- **Dump** the database back to a compacted log with `knowledge_dump_wal` — this is also the
  way to take a restore-point snapshot before a large or destructive operation, since WAL replay
  is forward-only.
- **Checkpoint** before backups with `knowledge_prepare_checkpoint`.
- **Rotation.** `LCG_WAL_MAX_BYTES_PER_FILE` (default 5 MB) and `LCG_WAL_MAX_EVENTS_PER_FILE`
  (default 10000) bound each WAL file's size; rotation fires when either threshold is reached
  and emits a `wal_rotated` [telemetry event](telemetry.md#wal_rotated).
- **Failure reporting.** Failure reports from replay dedupe by `(template, error)`, so a schema
  gap on one mutation type can no longer hide an unrelated failure category behind a wall of
  identical samples. Use `LCG_REPLAY_FAILURE_SAMPLES` to control how many distinct failing
  lines are retained per replay.

See [Configuration](configuration.md) for the full set of `LCG_WAL_*`/`LCG_REPLAY_*` environment
variables, and [IPC & MCP Reference](ipc-mcp-reference.md#mcp-over-stdio-transport) for the
`knowledge_rebuild_from_wal` non-empty-database refusal behavior in detail.

## Self-healing and degraded mode

The service binds its socket **before** opening the database, so a corrupted store leaves it
reachable in degraded mode rather than dead ([ADR-0009](adr/0009-degraded-mode-startup-recovery.md)).
Autonomous startup recovery ([ADR-0027](adr/0027-autonomous-wal-startup-recovery.md)) then reopens
at the last good checkpoint, replays the WAL tail, and rebuilds indices without intervention.
Recovery progress is observable via the [`wal_auto_recovery` telemetry event](telemetry.md#wal_auto_recovery),
whose `phase` field steps through `corruption_detected` → `checkpoint_drop_complete` →
`cursor_derived` → `replay_complete` → `index_build_complete` → `recovery_complete` (or
`fallback_triggered`, if automatic recovery gives up and manual intervention via
`knowledge_recover`/`knowledge_recover_full` is needed).

## `knowledge_status` health fields

Beyond the [ontology summary](ontology.md#knowledge_status-summary), `knowledge_status` reports:

**`indices_built`** (boolean) — whether the entity/relationship FTS + HNSW search indices are
currently built and reflect the graph's current contents. The service builds these indices
**eagerly at startup** — immediately after schema init on a fresh DB, or as part of
self-recovery after a WAL-corruption auto-heal — before the socket accepts any request, so
`indices_built` is normally `true` from the very first `knowledge_status` call onward (see
[ADR-0036](adr/0036-eager-index-build-at-startup.md)). A genuine build failure during that eager
startup build fails startup outright rather than silently leaving indices unbuilt.

`indices_built` still goes back to `false` in narrower, later situations: after
`knowledge_clear_all`, or if a post-rebuild index build genuinely fails (as opposed to the
common, harmless "already built" case). In those cases `false` does not mean search or ingest
is broken — `knowledge_find_entities`/`knowledge_find_relationships`, and the ingest
hybrid-dedup path used once a `group_id` passes the dedup threshold, all auto-heal by
transparently rebuilding indices and retrying on their first call after a `false` state. The
field exists so a caller can *observe* readiness proactively instead of discovering it only via
a search or ingest attempt. The same field appears on `knowledge_rebuild_from_wal`'s result (and
on `knowledge_rebuild_status`'s `result` for the background-job path) for the specific rebuild
that produced it; it is omitted from dry-run rebuild results, since a dry run never touches
indices.

**`name_index_trusted`** (boolean) and **`name_index_fallback_scans`** (integer) — report the
health of the in-process `NameIndex` accelerator behind case-insensitive entity name lookups
([ADR-0038](adr/0038-in-process-name-index.md)). `name_index_trusted` is `true` unless a write path
is known to have bypassed the index — e.g. a raw-Cypher mutation via `knowledge_query_cypher`
whose follow-up rebuild failed, or a post-replay `rebuild_name_index()` failure inside
`knowledge_rebuild_from_wal` — and goes back to `true` once the next rebuild succeeds.
`name_index_fallback_scans` counts how many times an endpoint-existence lookup (the
"does this entity exist anywhere in the group" check used during edge-endpoint resolution)
missed the index and fell back to a bounded database scan; it only increments on a miss; a
healthy, coherent index keeps this at (or near) `0`. Both fields are `null` while the service is
degraded (no connected database). A rising `name_index_fallback_scans` count, or a
`name_index_trusted: false` that doesn't clear on its own, signals index desync worth
investigating — see [ADR-0283](adr/0283-name-index-scan-fallback-for-endpoint-authority.md) for the
mechanism.

## Streaming progress

Long operations accept a `_progress_token` and stream progress frames before the terminal
result — see [Progress notifications](ipc-mcp-reference.md#progress-notifications) for the MCP
bridge and the list of operations that support it.

## Recovery and export tools

`knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_rebuild_from_wal`,
`knowledge_recover`, and `knowledge_recover_full` are all `admin`-scope IPC/MCP tools — see
[Scopes](ipc-mcp-reference.md#scopes) for the full admin-scope list and the MCP `--scope` flag.
