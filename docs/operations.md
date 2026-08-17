---
layout: default
title: Operations
---

# Operations

## On-disk layout

Everything the service manages lives under `.lcg/` in the workspace:

```text
.lcg/
├── wal/                    # WAL root — one subdirectory per group_id (issue #378)
│   └── liminis/            # the default group's stream: *.jsonl, .checkpoints/, .wal-bounds.json,
│                            #   .wal-generation.json
├── db/liminis.db           # LadybugDB files — a derived index, rebuildable from the WAL
├── ontology.yaml           # optional extraction vocabulary (yours to edit)
└── service.sock            # JSON-RPC 2.0 endpoint while the service runs
```

**The write-ahead log is the source of truth — and it's just JSON.** Every mutation is appended
to plain JSONL files in `.lcg/wal/<group_id>/` before it touches the database. The WAL is
human-readable, append-only, and git-friendly: check it into the same repository as your notes or
documents, diff it, and carry it across machines. The database is a derived index — delete it and
`knowledge_rebuild_from_wal` reconstructs the entire graph from the log.

**`.lcg/wal/` is a WAL root, not a single stream (issue #378).** Each `group_id` gets its own
subdirectory — its own `*.jsonl` files, its own `.checkpoints/` store, its own
`.wal-bounds.json` manifest, its own `.wal-generation.json` identity, and its own independent
`seq` numbering starting at 0. A group's subdirectory is created lazily on that group's first
write; a group that has never been written to simply has no subdirectory yet. A single-group
deployment (the common case — everything under the default `"liminis"` group, no caller ever
passing a different `group_id`) behaves exactly as a pre-378 deployment did: one subdirectory, one
writer, one recorded position. An **existing** pre-378 `.lcg/wal/` (loose
`*.jsonl`/`.checkpoints/`/`.wal-bounds.json` directly under `wal/`, no `liminis/` subdirectory) is
migrated automatically and idempotently on first boot under the upgraded binary — see
[ADR-0378](adr/0378-multi-stream-wal-per-group-directory.md) for the migration mechanics; no
operator action is required. As of issue #431, this migration also mints a
`.wal-generation.json` for the group it relocates content into: a legacy flat WAL predates
generation identity (issue #387) entirely, and this node wrote it itself, so it is treated as
locally owned rather than left with an unknown generation (see the unknown-generation refusal
below, and [ADR-0414](adr/0414-wal-generation-unknown-refuses-replay.md)'s amendment note). No
operator action is required for this either — it happens as part of the same migration.

**`.wal-generation.json` (issue #387) gives each group's stream a stable identity, distinct from
its `seq` numbering.** `seq` identifies a position *within* a stream; it says nothing about
*which* stream a position belongs to — so nothing distinguishes "the same stream, further along"
from "a different stream that happens to also number its lines from 0." A publisher can
legitimately reset a group's stream (re-extract a corpus and republish from `seq: 0` with
entirely different content and entity identities); the generation is what lets a consumer tell
that apart from ordinary forward progress. It is minted once, the first time a group's directory
is created with no prior content, and never changes for the life of that stream — appending never
changes it, and it is opaque (compared for equality only, never interpreted or ordered). The file
holds a single JSON object:

```json
{"generation": "3f9a1c2e-4b7d-4e21-9c8a-1a2b3c4d5e6f"}
```

Any string value works — lcg mints a UUID, but nothing requires that shape. **This file is
publisher-writable**: an external, non-lcg publisher (e.g. a distributed, git-published WAL model)
that creates a group's stream directory directly, without going through lcg, MUST write this file
itself (a plain `json.dump({"generation": <any unique string>}, f)` from Python is sufficient) for
`knowledge_rebuild_from_wal`'s reset detection (below) to work against that stream — lcg never
retroactively mints one into a directory it didn't create, and a directory with no
`.wal-generation.json` is treated as having an unknown generation (see `generation_status` in
[the generation-scoped `applied_seq` fields below](#knowledge_status-health-fields)). Whether that
is silently tolerated or an outright failure depends on whether a position for the group has
already been recorded — see issue #414 below. Like `.checkpoints/` and `.wal-bounds.json`, it is
invisible to every existing non-recursive `*.jsonl` scan.

### Publishing a WAL stream (issue #414)

**Publishing a group's stream directory means copying the entire directory, dot-namespace
included — never a `*.jsonl` or `wal/*` glob.** A shell glob does not match a leading dot by
default, so `git add wal/*`, `cp wal/*.jsonl`, `rsync --include='*.jsonl'`, and `tar wal/*` all
silently drop every dotfile in the directory while appearing to publish the complete stream. This
was confirmed as the root cause of a real-world reset-detection outage (issue #414): a publisher's
`*.jsonl`-only copy step dropped `.wal-generation.json` on every publish, so every consumer that
hydrated from it reported `generation: null` forever and `knowledge_rebuild_from_wal`'s reset
detection never once had a generation to compare.

Use a whole-directory copy instead — `cp -R`/`rsync -a` with no include-filter, or `git add -A` —
and know what each entry in the dot-namespace costs you if you omit it anyway:

| entry | requirement | consequence if dropped |
|---|---|---|
| `.wal-generation.json` (issue #387) | **MUST travel** — load-bearing | reset detection can never run for this stream again; every consumer that already recorded a position for this group starts hard-failing `knowledge_rebuild_from_wal` (issue #414, below) until the stream is republished with its generation intact |
| `.wal-bounds.json` (issue #375) | MAY be omitted | not wrong, just slow — a cache; the consumer regenerates it by rescanning every `*.jsonl` file on next read |
| `.checkpoints/` (issue #365) | MAY be excluded | local-only recovery state — omitting it is a legitimate choice, but make it an explicit, stated decision rather than an accident of the same glob that drops generation |

Only `.wal-generation.json` is load-bearing. The other two are safe to omit deliberately; they are
never safe to omit *by accident* as a side effect of a glob pattern that was only ever meant to
select `*.jsonl` files.

## WAL administration

- **Rebuild** one group's data from its own WAL directory with `knowledge_rebuild_from_wal
  {group_id, ...}` (`group_id` defaults to `"liminis"`, so a single-group deployment needs no
  change). A `from_seq: 0` (default) rebuild against a *group* that already has data in it fails
  fast with an explicit error instead of silently producing a duplicate-key failure per node —
  pass `force_clear: true` to clear that group's data automatically first (issue #378: this
  clears only the target group via the same primitive `knowledge_delete_by_group` uses, not the
  whole database file), or clear it yourself with `knowledge_delete_by_group` before calling
  rebuild. Rebuilding one group never touches another group's `WalPosition`, WAL directory, or
  data. A successful non-dry-run rebuild automatically rebuilds the entity/relationship search
  indices, so `knowledge_find_entities`/`knowledge_find_relationships` are immediately queryable
  afterward — `knowledge_build_indices` is not normally required.
- **Unknown-generation refusal (issue #414).** Before comparing anything, `knowledge_rebuild_from_wal`
  checks whether the group already has a previously recorded position (`applied_seq` not null —
  note a `knowledge_status` call can itself cause this to become true via its own backfill, so
  this can trip on what looks like the first explicit rebuild call ever made against a group) and
  whether the group's current on-disk generation is unknown (missing or corrupt
  `.wal-generation.json` — the two are indistinguishable by design, see below). If both hold, the
  call fails outright with an explicit error naming the group and pointing at the publish contract
  above — replay does not proceed, `from_seq`/`to_seq`/`force_clear` are not applied, and this
  applies uniformly to `dry_run: true` as well (there is nothing safe to preview). No
  configuration flag, environment variable, or request parameter bypasses this check. The refusal
  is scoped to the affected group only — a sibling group sharing the same WAL root whose own
  generation is known remains independently replayable in the same or a later call. A group with
  no previously recorded position is unaffected: it performs ordinary first-time adoption, including
  adopting an unknown generation, exactly as before this issue. See
  [ADR-0414](adr/0414-wal-generation-unknown-refuses-replay.md) for the full rationale.
  A workspace migrated from a legacy flat WAL by a binary containing issue #431's fix does not
  hit this refusal — migration itself stamps a generation, so the group's current on-disk
  generation is never unknown afterward (see the migration paragraph above). If it still fires,
  the error message gives two possible remedies, since the two situations that can produce this
  state are indistinguishable on disk: republish the stream's full directory if it was received
  from a publisher (above), or — for a local workspace with no publisher, e.g. one migrated by a
  binary older than issue #431's fix — create `.wal-generation.json` in the group's WAL directory
  by hand with any unique string value, `{"generation": "<any unique string>"}`, as a one-time,
  deliberate assertion of ownership.
- **Reset detection (issue #387).** Once the check above has passed, `knowledge_rebuild_from_wal`
  compares the group's recorded generation against what's currently on disk
  (`.wal-generation.json`). If they differ (both known and unequal — see `wal.generation_status`
  below for the unknown-generation case, handled by the refusal above instead), the caller's
  `from_seq`/`to_seq`/`force_clear` are overridden
  entirely: this is always a full, automatic self-heal — purge the group, replay it from scratch
  against the new generation, then re-bind any cross-group pointers into it — rather than
  silently replaying new-generation mutations on top of old-generation data (the corruption this
  issue exists to prevent; the two do not reconcile, since the native write path emits `CREATE`
  rather than `MERGE`). The result reports `reset_detected: true`, `previous_generation`,
  `generation` (the generation just replayed), and `cross_group_rebind` (the same counts
  `knowledge_rebind_pointers` reports), on both the streaming response and the background-job's
  polled `result`, so a caller can tell this apart from an ordinary incremental replay. A
  `dry_run: true` call against a mismatched group reports the same `reset_detected`/
  `previous_generation`/`generation` fields but purges and replays nothing — report-only, like
  every other dry-run path in this codebase.
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
  sequence numbers and a copied mark's `seq` would be meaningless against the new numbering. For
  the same reason, the output always gets a freshly minted generation (issue #387) — never the
  source's: it is a new stream, not a copy of the source's identity, so a consumer must not treat
  it as "the same stream" it was tracking before.
- **Name a known-good position** with `knowledge_wal_mark_create {name, group_id}` (`group_id`
  defaults to `"liminis"`) — a lightweight alternative to a full `knowledge_dump_wal` snapshot
  when all you need is a durable pointer back to "this group's stream was good here," not a
  materialized copy. A `name` must be 1-200 characters of `[A-Za-z0-9_-]`, because it becomes a
  single directory name under that group's own `.checkpoints/`. It records the target group's
  current `applied_seq`, **and the group's current generation (issue #387)**, under
  `<wal_root>/<group_id>/.checkpoints/`, is O(1) (no WAL scan or replay), and fails if the
  position is unknown (`applied_seq` is `null`) or the name is already in use by an active mark
  **within that group** — two different groups may each have an active mark of the same name,
  since each group's checkpoint store is independent. `knowledge_wal_mark_list
  {group_id}` (also defaulting to `"liminis"`, and always scoped to exactly one group — there is
  no cross-group aggregate listing) lists every active mark in that group with its `seq`, its
  `generation`, its `wal_min_seq`/`wal_max_seq` (the bounds of that group's WAL content currently
  on disk), and whether it is currently `reachable`: this requires both the existing bounds check
  (`wal_min_seq == 0` — the WAL's own prefix has not been externally truncated, e.g. by routine
  retention deleting old WAL files — and `seq <= wal_max_seq`) **and**, independently, that the
  mark's recorded `generation` matches the group's current on-disk generation whenever both are
  known (issue #387, FR-007) — a mark taken against a generation that has since been reset is
  never reachable, even when its `seq` still falls comfortably inside
  `[wal_min_seq, wal_max_seq]` (exactly the "looks like forward progress, isn't" case issue #387
  exists to close). Separately, on the bounds side, a mark whose `seq` merely falls inside
  `[wal_min_seq, wal_max_seq]` is still reported unreachable if `wal_min_seq > 0`, since a restore
  would silently omit everything before it. Neither check detects a gap in the *middle* of that
  range. `knowledge_wal_mark_delete {name,
  group_id}` removes a mark from that group (recording a tombstone, never rewriting the original
  record) and frees the name for reuse within that group. To restore: `knowledge_rebuild_from_wal
  {group_id, from_seq: 0, to_seq: <seq>, force_clear: true}` for a mark with an integer `seq`, or
  `knowledge_delete_by_group {group_ids: [group_id]}` for a mark with `seq: null` (a genuinely
  empty group) — or `knowledge_clear_all` if you mean to reset every group, not just one. These
  tools are unrelated to `knowledge_prepare_checkpoint` below — they name a WAL position, not
  flush a writer — and each group's `.checkpoints/` store lives in its own subdirectory precisely
  so it is invisible to the WAL file scans that discover `.jsonl` mutation files
  (`knowledge_dump_wal` and the replayer among them), and so it travels with that group's WAL
  directory itself when checked into git. Exactly-one-wins under concurrent `create` for the same
  name (within one group) relies on exclusive file creation (`O_EXCL`), a local-filesystem
  guarantee — not reliable on an NFS-mounted WAL directory (see
  [ADR-0365](adr/0365-wal-checkpoints-directory-per-name-store.md)).
- **Checkpoint** before backups with `knowledge_prepare_checkpoint` — this rotates and flushes
  every group's live WAL writer (issue #378: an instance-wide operation now spans however many
  groups this process has written to, not one writer) so pending mutations are on disk before an
  external filesystem backup. It shares the word "checkpoint" with `knowledge_wal_mark_*` above
  by coincidence, not by relation, and takes no `group_id` — it is always whole-instance.
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

**`wal_groups`** (issue #378) — an additive map, keyed by `group_id`, of every group that
currently has a WAL directory, each entry shaped like the flat `wal` object below
(`{applied_seq, max_seq, generation, generation_status}`). This is the multi-group view; the flat
`wal.applied_seq`/`wal.max_seq`/`wal.generation`/`wal.generation_status` fields described next remain present and
**pinned specifically to the default `"liminis"` group**, unchanged in meaning from a pre-378
single-group deployment — a caller that only reads the flat fields (e.g. an existing integration
written before this issue) needs no change. If the default group has no WAL directory at all
(e.g. a pure replica that has only ever hydrated non-default groups), the flat fields report
`null`/absent rather than an error — a documented signal that this instance has no default group,
not a broken or un-hydrated instance. Do not confuse "not in `wal_groups`" with "at position 0": a
group present in the map with `applied_seq: 0` has a directory and a known position; a group
absent from the map entirely has no WAL directory yet.

**`wal.applied_seq`** and **`wal.max_seq`** (issue #353; scoped to the default group by issue
#378) — let a caller decide, from a single `knowledge_status` call and an integer comparison,
whether its local DB is already consistent with the default group's WAL, needs an incremental
resume, or needs a full rebuild. `wal.applied_seq` is read from a persisted DB row on every call —
never cached in memory, so the value survives a service restart. `wal.max_seq` always reports the
true highest `seq` actually present on disk for the default group (or
`None`/`null` if the WAL is empty or unconfigured); an externally-updated WAL (e.g. a distributed,
git-published WAL pulled by another process) is observed on the very next call, at worst after one
reconciling full scan (issue #375). In the common case it's computed from a small manifest sidecar
(`<wal_dir>/.wal-bounds.json`) rather than by rereading every `.jsonl` file in the WAL directory on
every call — see [ADR-0375](adr/0375-wal-max-seq-bounds-manifest.md) for the caching mechanism and
why an earlier "never cached" design was revised. The same manifest and fast path also back
`wal_min_seq`, so `knowledge_wal_mark_list`'s reachability check (below) does not scale with WAL
file count either.

**`wal.generation`** (issue #387; also scoped to the default group, and mirrored per-group inside
`wal_groups`) — the group's current **on-disk (source-side)** generation, read from
`.wal-generation.json` alongside the same `wal_max_seq` machinery above, so reporting it costs
nothing beyond what `applied_seq`/`max_seq` already pay (no new full-directory scan). This is
deliberately the on-disk value, not lcg's own DB-recorded consumer-side position — an external
consumer (e.g. orac) compares this against its *own* bookkeeping to answer "is this the same
stream I was tracking?", the same on-disk-authoritative role `max_seq` already plays. `null` means
the stream currently has no generation recorded — its own `generation_status` (next) says whether
that is "no stream yet" or "unknown" (both used to collapse indistinguishably to this same `null`,
issue #414). Opaque: compare for equality only, never interpret or order it. lcg's own
internally-recorded generation (paired with its own `applied_seq`, and what
`knowledge_rebuild_from_wal`'s reset detection actually compares against) is not surfaced by
`knowledge_status` at all — it is a purely internal bookkeeping value with no separate
consumer-facing use.

**`wal.generation_status`** (issue #414; also scoped to the default group, and mirrored per-group
inside `wal_groups`) — a sibling string field alongside `generation`, classifying why `generation`
reads the way it does, since `generation: null` alone cannot distinguish "no stream" from "stream,
but generation unknown." Pure classification of `max_seq`/`generation`, no new I/O:

| `generation_status` | meaning |
|---|---|
| `"not_applicable"` | no WAL stream exists yet for this group (no `*.jsonl` content, no generation record) |
| `"unknown"` | a stream exists (`*.jsonl` content is present) but its generation is currently unrecoverable — missing or corrupt `.wal-generation.json`, most commonly because a publish step dropped the dot-namespace (see [Publishing a WAL stream](#publishing-a-wal-stream-issue-414) above) |
| `"known"` | a stream exists with a recorded generation — including a freshly-minted, still-empty stream (`max_seq: null`, `generation` non-null) |

`generation_status: "unknown"` is exactly the condition that makes `knowledge_rebuild_from_wal`
refuse once a position has been recorded for that group (see Unknown-generation refusal above) —
checking this field before calling rebuild lets an operator see the condition coming rather than
discovering it as an abrupt failure.

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
