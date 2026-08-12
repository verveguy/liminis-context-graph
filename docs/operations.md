---
layout: default
title: Operations
---

# Operations

## On-disk layout

Everything the service manages lives under `.lcg/` in the workspace:

```text
.lcg/
├── wal/               # append-only JSONL mutation log — the durable record (git-friendly)
├── db/liminis.db      # LadybugDB files — a derived index, rebuildable from the WAL
├── ontology.yaml      # optional extraction vocabulary (yours to edit)
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
- **Bounded rebuild** with `to_seq`: pass an inclusive upper bound (`from_seq <= seq <= to_seq`)
  to exclude a known-bad mutation and everything after it — e.g. recovering from an operator
  mistake that is itself recorded in the WAL. `knowledge_rebuild_from_wal {from_seq: 0,
  to_seq: <seq before the bad mutation>, force_clear: true}` rebuilds the graph as it stood just
  before the mistake. This is **not durable**: WAL entries beyond `to_seq` are left on disk,
  unapplied — they are not truncated or archived. A later unbounded rebuild, or a `from_seq`
  resume that covers the excluded range, reapplies everything that was excluded, including a
  previously-excluded bad mutation. Durable rollback (truncating/archiving the WAL tail) is not
  provided by this primitive.
- **Dump** the database back to a compacted log with `knowledge_dump_wal` — this is also the
  way to take a restore-point snapshot before a large or destructive operation, since WAL replay
  is forward-only. The output directory starts with no checkpoints: any WAL marks (below)
  recorded against the source directory are not carried forward, since dump_wal renumbers
  sequence numbers and a copied mark's `seq` would be meaningless against the new numbering.
- **Name a known-good position** with `knowledge_wal_mark_create {name}` — a lightweight
  alternative to a full `knowledge_dump_wal` snapshot when all you need is a durable pointer back
  to "this graph was good here," not a materialized copy. A `name` must be 1-200 characters of
  `[A-Za-z0-9_-]`, because it becomes a single directory name under `.checkpoints/`. It records
  the database's current `applied_seq` under `<wal_dir>/.checkpoints/`, is O(1) (no WAL scan or
  replay), and fails if the position is unknown (`applied_seq` is `null`) or the name is already
  in use by an active mark. `knowledge_wal_mark_list` lists every active mark with its `seq`, its
  `wal_min_seq`/`wal_max_seq` (the bounds of WAL content currently on disk), and whether it is
  currently `reachable`: this requires both `wal_min_seq == 0` (the WAL's own prefix has not been
  externally truncated, e.g. by routine retention deleting old WAL files) and `seq <=
  wal_max_seq` — a mark whose `seq` merely falls inside `[wal_min_seq, wal_max_seq]` is still
  reported unreachable if `wal_min_seq > 0`, since a restore would silently omit everything before
  it. This does not detect a gap in the *middle* of that range. `knowledge_wal_mark_delete {name}` removes a
  mark (recording a tombstone, never rewriting the original record) and frees the name for reuse.
  To restore: `knowledge_rebuild_from_wal {from_seq: 0, to_seq: <seq>, force_clear: true}` for a
  mark with an integer `seq`, or `knowledge_clear_all` for a mark with `seq: null` (a genuinely
  empty graph). These tools are unrelated to `knowledge_prepare_checkpoint` below — they name a
  WAL position, not flush a writer — and their `.checkpoints/` store lives in its own
  subdirectory precisely so it is invisible to the WAL file scans that discover `.jsonl` mutation
  files (`knowledge_dump_wal` and the replayer among them), and so it travels with the WAL
  directory itself when checked into git. Exactly-one-wins under concurrent `create` for the same
  name relies on exclusive file creation (`O_EXCL`), a local-filesystem guarantee — not reliable
  on an NFS-mounted WAL directory (see [ADR-0365](adr/0365-wal-checkpoints-directory-per-name-store.md)).
- **Checkpoint** before backups with `knowledge_prepare_checkpoint` — this rotates and flushes
  the live WAL writer so pending mutations are on disk before an external filesystem backup. It
  shares the word "checkpoint" with `knowledge_wal_mark_*` above by coincidence, not by relation.
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

A runtime recovery — any `knowledge_recover` strategy (`drop_lbug_wal`,
`rebuild_from_workspace_wal`, `restore_from_backup`) or `knowledge_recover_full` — also leaves
`indices_built` correctly `true` on success: `drop_lbug_wal`/`restore_from_backup` reopen an
already-indexed checkpoint or backup, while `rebuild_from_workspace_wal`/`knowledge_recover_full`
explicitly rebuild the indices before reporting success. Failure handling differs by strategy:
`rebuild_from_workspace_wal` and `knowledge_recover_full` invalidate indices as part of the
attempt, so a failure that aborts before the rebuild completes leaves the flag `false` rather than
reporting stale readiness; `drop_lbug_wal` and `restore_from_backup` never touch indices, so a
failed call leaves the flag at whatever it was before the attempt.

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

**`wal.applied_seq`** and **`wal.max_seq`** (issue #353) — let a caller decide, from a single
`knowledge_status` call and an integer comparison, whether its local DB is already consistent
with the WAL, needs an incremental resume, or needs a full rebuild. `wal.applied_seq` is read
from a persisted DB row on every call — never cached in memory, so the value survives a service
restart. `wal.max_seq` is recomputed fresh from the on-disk WAL files on every call (the highest
`seq` actually present, or `None`/`null` if the WAL is empty or unconfigured) — never cached,
so an externally-updated WAL (e.g. a distributed, git-published WAL pulled by another process) is
observed on the very next call.

The consumer decision, comparing the two fields — check both for `null` before any numeric
comparison:

| `applied_seq` | `max_seq` | Meaning | Action |
|---|---|---|---|
| `null` | any | position unknown | full rebuild |
| any | `null` | WAL empty or unconfigured | nothing to resume from; treat like an empty WAL |
| `N` | `N` (equal) | DB is caught up | none |
| `N` | `M > N` | DB is behind, as a forward extension | incremental resume from `applied_seq + 1` (not `applied_seq` — replay's `from_seq` filter keeps lines with `seq >= from_seq`, so resuming *at* `applied_seq` would re-replay the last-applied line) |
| `N` | `M < N` | DB has advanced beyond what the currently-visible WAL contains (e.g. a corpus reset, or a stale copied-back WAL) — not a forward extension | full rebuild |

A bounded rebuild (`to_seq` set — see [WAL administration](#wal-administration) above) is one
deliberate way to land in the "DB is behind, as a forward extension" row: `applied_seq` reports
the bounded landing point (`<= to_seq`), while `max_seq` still reflects the WAL's true, unbounded
on-disk maximum. This is expected, not a fault to recover from automatically — an incremental
resume covering the gap (or a later unbounded rebuild) reapplies everything the bounded rebuild
excluded, including a previously-excluded bad mutation.

**`applied_seq` has three distinct values, not two — treat them as different types, not points
on a number line:**

- **`null`** — unknown position. Reported when a pre-existing DB has no recorded position and
  the one-time backfill (below) fails to derive one: either a populated DB (has `Entity` or
  `Episodic` content) whose last episode's uuid isn't found in the WAL, or a DB with no
  `Episodic` nodes but surviving `Entity`/relationship content (episode deletion removes only
  the `Episodic` node, never the entities it created, so a graph can be non-empty with zero
  episodes — there is nothing left to derive a position from, but real content to lose track
  of). The documented action is always a full rebuild.
- **`0`** (integer) — a known position: nothing has been applied yet. Reported for a
  fresh/cleared DB (including a pre-existing DB with *no* `Episodic` nodes **and** no
  `Entity`/relationship content either — genuinely nothing to derive a position from and
  nothing to lose track of, so the backfill writes `0` directly without a WAL scan), or
  immediately after `knowledge_clear_all`.
- **A positive integer** — a known, applied WAL position.

Do not treat `null` as if it sorted below `0`. **This distinction is not just a Rust/Python
concern — it changes behavior across languages.** `null < 5` throws or is a type error in Rust
and Python (arithmetic on `null`/`None` isn't defined), which tends to surface the bug
immediately. But in JavaScript, `null < 5` coerces to `true` — a naive port of the "if behind,
resume" comparison silently takes the *incremental resume* branch on an *unknown* position,
skipping the full rebuild the `null` state actually calls for. The same footgun applies to a
`null` `max_seq`: `5 < null` coerces to `false` in JavaScript, so a check written only as
`applied_seq < max_seq` silently falls through neither branch when the WAL is empty or
unconfigured. Check both fields for `null` explicitly, before doing any numeric comparison, in
every client language.

**Upgrading an existing deployment**: a DB populated before this feature existed has content but
no recorded position on its first boot under the new version. Rather than reporting `null` for
that (indistinguishable from a genuinely unknown position, and prone to a client either skipping
a needed rebuild or being unable to tell "empty" from "unknown"), the service backfills a
conservative position on first open, derived from the last `Episodic` node's location in the WAL
(the retroactive episode-cursor mechanism from
[ADR-0026](adr/0026-episode-cursor-wal-resume.md); see [ADR-0353](adr/0353-persist-and-expose-applied-wal-seq.md)
for why this issue persists a cursor for the fast path in addition to ADR-0026's own recovery-time
use of the same mechanism). This backfill runs once at startup and is a no-op on every subsequent
boot once a position is recorded.

## Streaming progress

Long operations accept a `_progress_token` and stream progress frames before the terminal
result — see [Progress notifications](ipc-mcp-reference.md#progress-notifications) for the MCP
bridge and the list of operations that support it.

## Recovery and export tools

`knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_wal_mark_create`,
`knowledge_wal_mark_list`, `knowledge_wal_mark_delete`, `knowledge_rebuild_from_wal`,
`knowledge_recover`, and `knowledge_recover_full` are all `admin`-scope IPC/MCP tools — see
[Scopes](ipc-mcp-reference.md#scopes) for the full admin-scope list and the MCP `--scope` flag.
