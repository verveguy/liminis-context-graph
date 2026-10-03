# ADR-0673: Ingest Time Is Stored Separately From Event Time

**Status**: Accepted
**Date**: 2026-10-03
**Issue**: #673 (requested by verveguy/liminis PR #1173)

## Context

`reference_time` is the caller-supplied *event* time. The service wrote it into both `created_at`
and `valid_at` for episodes and into `created_at` for entities and edges, so once callers send real
event times (document-content chunk times, backdated conversation consolidations) the graph keeps no
record of *when it learned* a fact. "What did the graph learn this week?", ingest-order debugging and
knowledge-age retention all became impossible.

## Decision

**A nullable `ingested_at TIMESTAMP` on `Entity`, `Episodic` and `RelatesToNode_`**, always the
service clock, never caller-supplied. It is a fourth documented, additive divergence from graphiti's
`kuzu_driver.py` (after `summary_embedding`, `lookup_key`, `kind`); graphiti-shaped statements never
mention it and still bind.

**Stamped in the four `Conn::insert_*` helpers** (`insert_entity`, `insert_episodic`,
`insert_relates_to_edge`, `insert_cross_group_edge`) when the row's `ingested_at` is empty, in the WAL
`ts` format (`%Y-%m-%dT%H:%M:%S%.6f+00:00`). One place covers extraction, direct assertions,
cross-group edges and corrections' copies, and it runs under the write lock. FR-002 holds
structurally: no handler maps a request key onto the field. The "only when empty" rule exists for
one service-internal caller: the `same_as` / endpoint-rewrite corrections re-create an edge under a
new uuid and pass the original edge's value, because the graph learned the fact when the original was
written.

**First-write semantics.** Nothing that updates, matches or merges an existing record touches the
column (`merge_entities` re-dates `created_at` via `update_entity_created_at`, never `ingested_at`).

**Durable, not derived.** Unlike `lookup_key` and the vector params (ADR-0526/0577), `ingested_at` is a
bound param of the CREATE statements and is **kept** in the WAL — it is in neither
`DERIVED_PARAM_KEYS` nor the vector strip lists. Replay executes recorded params verbatim, so rebuild,
tail recovery, startup recovery and checkpoint restore cannot restamp (FR-006). `ingested_at` is in
`TIMESTAMP_PARAM_NAMES` so a bare `col: $param` binds as a typed timestamp on the live and replay paths.

**Dump compaction (ADR-0028).** The three dump templates carry `ingested_at`
(`CASE WHEN $ingested_at IS NULL THEN NULL ELSE timestamp($ingested_at) END`) and the page queries
append the column last, so positional consumers do not shift. A row that still has no ingest time at
dump falls back to its `created_at`. Dump-time WAL `ts` is never used as a learn-time signal.

**Reads.** `ingested_at` is appended to the end of every RETURN list that builds `EntityRow`,
`EpisodicRow` or `RelatesToEdge`, and to `EPISODE_FIELDS` / `ENTITY_FIELDS` for projection. Edge reads
still leave `created_at` empty (FR-003); paging stays on `created_at, uuid`.

**Upgrade backfill** (`schema::ensure_ingested_at_backfill`), in precedence order: (1) the `ts` of the
first *native* creating WAL line for the uuid (`CREATE (:Label {…` or the legacy
`MERGE … ON CREATE SET`), found by one streamed pass over the group WAL directories in first-`seq`
order with `IS NULL`-guarded point updates; (2) `created_at`. Dump-style `MERGE … SET` lines are not
creating lines (their `ts` is compaction time). Completion is persisted in `SchemaState`
(`ingested_at_backfill_v1`) so a clean start is one point lookup. The WAL needs a directory that
`migrate()` does not have, so the ensure runs right after `init_schema` at the sites that do (service
startup, `Db::open_or_rebuild`, the checkpoint/backup recovery strategies); a site with no WAL dirs is
a no-op rather than a `created_at`-only pass, which would discard the better signal. Every rebuild
site (`open_or_rebuild`, `knowledge_rebuild_from_wal`/reload, the `knowledge_recover*` family,
`run_full_recovery_sequence`) runs the unconditional variant after replay, since replaying a legacy WAL
creates NULL rows after an earlier "complete" marker.

## Consequences

- Backfilled values from signal 2 are approximate (`created_at` was event time for those records); the
  docs say so.
- Records in one chunk get slightly different `ingested_at` values (stamped per insert). Acceptable
  under per-record first-write semantics.
- Out of scope: filtering/sorting by ingest time, time-aware ranking, supersession, decay, retention
  policy, and a "last updated" clock.
