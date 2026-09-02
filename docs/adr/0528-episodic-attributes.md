# ADR-0528: Structured Attributes on Episodes (`Episodic.attributes`)

**Status**: Accepted
**Date**: 2026-09-02
**Issue**: #528
**Relates to**: ADR-0470 (precedent for a documented graphiti-parity divergence and a migration
zero-fill decision), ADR-0221 (precedent for the probe-then-`ALTER` migration pattern and its
`SchemaState` retry marker), #410 (precedent for per-tool documentation of a field's inclusion/
exclusion), #524 (precedent for the same per-call-asymmetry risk), #525 (the originating
community report, from the orac project)

## Context

A document's structured metadata (originating system, ingestion batch, custom tags, …) and the
facts extracted from its prose could not be co-located on, or joined to, one node.
`knowledge_process_chunk`/`knowledge_add_episode` anchor extracted entities on the episode via the
`MENTIONS` edge; `knowledge_assert_entity` is the only call that accepts an `attributes` map, and
it creates a *separate* `Entity` node. The two halves were unified only by a shared identity
string convention (callers reusing one name across both calls), not a graph relationship.

`MENTIONS` already runs `Episodic -> Entity`, so the traversal half of the requirement exists
today: every entity extracted from a chunk's prose is already one hop from that chunk's episode.
Putting structured attributes on the episode node itself (zero hops) and leaving extracted facts
reachable via the existing `MENTIONS` edge (one hop) satisfies the requirement without inventing a
new join, a new edge type, or a new identity concept — the two alternatives considered and
rejected (see Decision 1).

## Decision 1: Attributes live on `Episodic`, not on a new edge or an entity binding

**Chosen**: add `attributes STRING` to `Episodic`, holding a JSON object serialized as a string —
the same convention already used by `Entity.attributes`/`RelatesToNode_.attributes`.

**Rejected**:
- Generalizing `knowledge_assert_relationship` to accept an episode endpoint, so a relationship
  could directly connect an episode to an entity carrying the metadata. This invents a new
  Episode-as-relationship-endpoint concept and a new join that `MENTIONS` already provides for the
  traversal direction that matters (episode to its extracted entities).
- Binding an episode to an already-asserted `Entity` node (e.g. a new edge type from `Episodic` to
  the `Entity` created by `knowledge_assert_entity`). This still requires the caller to maintain a
  second node and a shared identity between it and the episode — the exact problem this issue
  exists to remove, not a solution to it.

**Rationale**: the community report's motivation is a single queryable node — *this document, with
its structured attributes, from which the facts it states are reachable*. Putting the metadata
directly on `Episodic` is the only shape that satisfies this without a new edge type or a new
identity concept, and it reuses a column pattern (`Entity.attributes`) already proven in this
codebase.

## Decision 2: `Episodic.attributes` is a deliberate divergence from graphiti's schema, not a parity gap

**Chosen**: `attributes STRING` on `Episodic`'s node table is documented inline as an intentional
divergence from upstream graphiti-core's `kuzu_driver.py` `EpisodicNode` schema (which has no
attributes column), following the same pattern ADR-0470 established for `Entity.summary_embedding`
and ADR-0221 for `Entity.lookup_key`.

**Rationale**: this repo's CLAUDE.md instructs diffing new columns against graphiti's
`kuzu_driver.py` and treating a gap as something to fix by adding the missing column upstream-side
— the opposite instinct is needed here, since `attributes` doesn't exist on `EpisodicNode` at all.
Without an explicit comment and this ADR, a future schema-parity audit could misread this column
as one to remove rather than a genuine, additive local improvement.

## Decision 3: Migration zero-fills existing rows to `"{}"`, even though no index forces it

**Chosen**: `schema::migrate` probes for `Episodic.attributes`, and — only in the branch where the
`ALTER TABLE Episodic ADD attributes STRING` just ran — immediately zero-fills every existing row
to the empty-JSON-object string `"{}"` (`zero_fill_null_episodic_attributes`). The same function is
called from every WAL-rebuild/recovery call site (`Db::open_or_rebuild`, the background/foreground
`knowledge_rebuild_from_wal` reload paths, `run_full_recovery_sequence`), since `WalReplayer`
executes recorded pre-#528 Cypher verbatim and a replayed old-WAL `CREATE`/`MERGE` never sets a
column that didn't exist when it was logged — verified in `handlers_wal_admin.rs`'s
`test_rebuild_from_wal_force_clear_zero_fills_legacy_episodic_attributes`.

**Rejected**: leaving pre-existing and replay-created rows' `attributes` as `NULL`, requiring
callers to treat both `NULL` (read back as `""` by `value_as_string`) and `"{}"` as "no attributes"
on read.

**Rationale**: unlike `Entity.summary_embedding` (ADR-0470 Decision 3), no HNSW/ART index is ever
built over `Episodic.attributes`, so nothing about lbug's indexing constraints *forces* a
zero-fill here — a bare `NULL` would bind and read back fine. The zero-fill is done anyway for a
different reason: the issue's Key Entities section frames `attributes` as "a JSON object,
serialized as a string" — full stop, no caveat for a legacy row. A caller that unconditionally
does `json.loads(episode["attributes"])` should never hit `""`, which is not valid JSON, while
every freshly-created episode (even one that omits `attributes`) already reads back `"{}"` via
`attributes_param_to_string`'s existing convention. Leaving two different "empty" values — one of
them not parseable — for no offsetting benefit would violate that framing for no reason; the
zero-fill itself is a cheap, bulk, unindexed `SET ... WHERE ... IS NULL`.

Unlike `Entity.lookup_key` (ADR-0221), this migration does **not** gain a persisted `SchemaState`
retry marker. `lookup_key` needed one because a failed backfill left rows permanently invisible to
an ART-indexed lookup with no fallback scan — a genuine correctness hazard for every subsequent
`get_entity_by_name_ci` call. `attributes` has no index and no correctness-critical downstream
consumer: a row that stayed `NULL` after a failed zero-fill attempt is merely a marginally worse
"empty" value (`""` instead of `"{}"`) on the next read, not silently-corrupted or invisible data.
The existing `eprintln!`-and-continue non-fatal pattern already used by every other plain-`STRING`
migration branch in `schema.rs` is sufficient here.

## Consequences

- `Episodic` gains one column (`attributes`), no new index. `EpisodicRow` and `PassageResult` both
  gain a plain `String` field (no `#[serde(skip)]` — unlike embedding columns, `attributes` is
  meant to be visible in IPC/MCP responses, per FR-006/FR-010).
- `knowledge_process_chunk` and `knowledge_add_episode` both gain an optional `attributes` JSON-
  object parameter, parsed via the existing `attributes_param_to_string` helper (issue #379) — the
  same non-object-defaults-to-`{}` convention `knowledge_assert_entity`/`knowledge_assert_relationship`
  already use, satisfied by construction rather than a parallel implementation.
- `knowledge_get_episodes` and `knowledge_search_passages` return each episode's/passage's
  `attributes` directly — no second per-episode fetch needed to reach a search hit's structured
  metadata, which is the entire point of the originating community report.
- Every other episode-adjacent MCP tool (`knowledge_find_relationships`,
  `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`, `knowledge_list_relationships`,
  `knowledge_get_entity_neighbors`, `knowledge_list_entities`, `knowledge_get_entities_by_source`)
  states explicitly in its own tool description that it does *not* include episode `attributes` —
  mirroring the discipline issue #410 established for `episode_uuids` claims, verified by a
  registry test parallel to that issue's own.
- The WAL dump/compaction template (`EPISODIC_CYPHER`) and its extraction/hydration counterpart
  (`dump_episodics_page`/`dump_episodic_nodes`) both gained the new column in lockstep — verified
  by a dedicated dump→replay round-trip test (`handlers_wal_dump.rs`) so the two never silently
  drift apart (Cypher does not error on a column simply absent from a `SET` clause).
- A first post-upgrade DB open pays a one-time zero-fill cost proportional to episode count, run
  synchronously inside `migrate()`. This is a one-time cost, not a recurring regression.
- No IPC/MCP schema change to any *other* existing method — every episode-adjacent tool's request
  shape is unchanged; only two tools (`knowledge_process_chunk`, `knowledge_add_episode`) gained a
  new optional parameter, and two read tools' response shape gained one already-present-but-empty
  field for pre-existing rows.
