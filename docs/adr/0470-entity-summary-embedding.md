# ADR-0470: Entity Summary Embedding for Semantic Search

**Status**: Accepted
**Date**: 2026-08-22
**Issue**: #470
**Relates to**: ADR-0353/ADR-0387 (precedent for a documented graphiti-parity divergence),
ADR-0025/ADR-0036 (auto-heal index build, eager index build at startup), ADR-0314 (empty-summary
salvage), #440 (recompute embeddings on WAL replay, related but separate), #444/#445 (adjacent,
explicitly out of scope)

## Context

`Entity` stores a `summary STRING` but had no vector representation of it — only `name_embedding`
existed, so vector-based semantic retrieval was name-only. `Entity` already carries a full-text
search (FTS) index over both `name` and `summary`, so a keyword match against the summary already
worked; what was missing was the ability to find an entity by a *paraphrase* of its summary that
shares no significant vocabulary with it — the catalogue-style use case #379's direct-assertion
API was built to unblock, and the gap community report #465 (imprecisely) surfaced.

Four design questions had to be settled, each with a real cost either way.

## Decision 1: `summary_embedding` is a deliberate divergence from graphiti's schema, not a parity gap

**Chosen**: add `summary_embedding FLOAT[{dim}]` to `Entity`'s node table, documented inline as an
intentional divergence from upstream graphiti-core's `kuzu_driver.py` Entity schema (which has no
summary vector), following the same pattern as `WalPosition.generation` (ADR-0353/ADR-0387).

**Rejected**: treating this as a schema-parity catch-up needing no special documentation, the way
the #128/#130/#133/#136/#144 precedents (CLAUDE.md's schema-parity rule) were.

**Rationale**: this repo's CLAUDE.md instructs diffing new columns against graphiti's
`kuzu_driver.py` and treating a gap as something to fix by adding the missing column — the
opposite instinct is needed here, since `summary_embedding` doesn't exist upstream at all. Without
an explicit comment and this ADR, a future schema-parity audit could misread `summary_embedding`
as a column to remove rather than a genuine, additive local improvement.

## Decision 2: `summary_embedding` is write-once, matching `name_embedding`/`fact_embedding` — not "always current" as the spec's Edge Cases text literally asked for

**Chosen**: `summary_embedding` is set only at `CREATE` time (both `knowledge_assert_entity`'s
create branch and the extraction pipeline's entity-insert path). `update_entity_core` (the
re-assert/update path) never touches it, exactly as it already never touches `name_embedding`.

**Rejected**: refreshing `summary_embedding` via a plain `SET` on re-assert; delete+reinsert of the
`Entity` node on re-assert to allow a genuine refresh.

**Rationale**: lbug's HNSW index rejects a plain `SET` on an indexed column outright ("Cannot set
property … because it is used in one or more indexes. Try delete and then insert.") — the same
constraint that already makes `name_embedding` and `RelatesToNode_.fact_embedding` write-once,
accepted staleness in this codebase. Delete+reinsert is unprecedented here for any embedding
column and, critically, would also delete every incident `RELATES_TO`/`MENTIONS` edge (Kuzu
requires `DETACH DELETE` for a node with edges) — a faithful reinsert-with-edges-preserved is a
much larger, higher-risk undertaking than this issue's scope. This is an explicit, documented
deviation from the spec's Edge Cases text ("the summary embedding reflects the current summary
after the write, not a stale one from creation time") — FR-002's primary ask (embedding on every
path that *creates* an entity with a summary) is still fully satisfied; entity **creation** is
what User Story 1 / SC-001 / SC-002 actually test. A re-assert that changes `summary` leaves
`summary_embedding` pointing at the *original* summary until the entity is re-embedded via
`knowledge_backfill_summary_embeddings` — asserted explicitly in
`crates/core/tests/summary_semantic_search.rs`'s
`reassert_with_changed_summary_leaves_summary_embedding_stale` test, so this doesn't regress
silently later.

## Decision 3: Migration zero-fills existing rows before any index exists over the column

**Chosen**: `schema::migrate` probes for `summary_embedding`, and — only in the branch where the
`ALTER TABLE Entity ADD summary_embedding FLOAT[{dim}]` just ran — immediately zero-fills every
existing row (`SET n.summary_embedding = vec![0.0; dim]`) before `build_indices_and_constraints`
ever creates `entity_summary_embedding_idx` over the column.

**Rejected**: leaving `summary_embedding` `NULL` for pre-existing rows until backfilled.

**Rationale**: a plain `SET` is only legal on this column *before* an HNSW index exists over it
(see Decision 2) — this is the one window where every row, old or new, can be given a real value
rather than `NULL`. It also sidesteps an otherwise-unverified question (does `CREATE_VECTOR_INDEX`
tolerate `NULL` entries?) entirely: after migration, `summary_embedding` is always a same-length
`FLOAT[dim]` vector, never absent, whether it holds a real embedding, a zero-vector from an
empty-summary entity, or a zero-vector placeholder awaiting backfill. `insert_entity` applies the
identical zero-vector fallback for any caller that doesn't supply a real embedding (sized off
`name_embedding`'s length, which every caller already populates correctly) — the same sentinel,
used uniformly whether the value came from a migration, an empty summary, or an embedder failure.
This was verified empirically first: `ALTER TABLE … ADD summary_embedding FLOAT[{dim}]` against a
throwaway DB, since none of the six prior `migrate()` additions added a fixed-size `FLOAT[N]`
ARRAY column and this was flagged unverified in Research.

## Decision 4: A new IPC/MCP method (`knowledge_backfill_summary_embeddings`), a deviation from FR-006's default

**Chosen**: a dedicated `knowledge_backfill_summary_embeddings` operation (Admin scope), mirroring
`knowledge_backfill_relation_types`'s paginated-read / dry-run / batched-write / progress-event
shape, but additionally dropping `entity_summary_embedding_idx` before its write phase and
rebuilding it after — holding `state.write_lock.write()` for the *entire* drop→batch→rebuild
sequence, not released between batches.

**Rejected**:
- Folding backfill into `knowledge_rebuild_from_wal`'s existing response — conflates a targeted
  embedding backfill with a full destructive purge-and-replay of the whole group, and requires a
  complete WAL history to exist (a pre-migration WAL entry has no `summary_embedding` parameter to
  replay in the first place — `replay.rs` executes raw recorded Cypher verbatim, it doesn't
  reconstruct or recompute anything).
- Running it automatically inside the eager/auto-heal `build_indices_once` startup path — would
  put unbounded embedder round-trips on a hot startup path with no dry-run or progress surface.
- Per-row `SET` without dropping the index first — rejected outright by lbug once the index
  exists (Decision 2's constraint).

**Rationale**: FR-006 itself provides the escape valve this decision uses: "If implementation
reveals that a schema change is in fact required, that MUST be surfaced explicitly … rather than
folded silently into this work." Both alternatives above are worse fits for exactly the reasons
listed, so the new method — costing nothing new architecturally, since it exactly mirrors an
existing precedent — is the least-bad option. It's scoped `Admin` rather than `Write` (unlike its
`relation_type` cousin) because it performs index-maintenance (drop/rebuild), matching CLAUDE.md's
scope-bucket guidance for admin/index-maintenance operations. The write lock is held for the whole
sequence (not released between batches, unlike `backfill_relation_types`) because the index is
genuinely absent for that whole window — releasing the lock between batches would let a concurrent
`knowledge_find_entities` call observe the missing index and race the auto-heal path
(`is_missing_index_error` → `build_indices_once`) against this pass's own rebuild. If the process
crashes mid-backfill (index dropped, not yet rebuilt), the next read's auto-heal path self-heals
it via the same idempotent `CREATE_VECTOR_INDEX` call — no extra recovery code needed.

Backfill re-embeds every non-empty-summary entity in `group_id` on each call rather than skipping
rows already embedded — no cheap way exists to distinguish "already has a real embedding" from
"still the migration's zero-vector placeholder" by reading a stored `FLOAT[]` value back out (no
such array-decoding helper existed in `db.rs`, and building one solely for this purpose was judged
disproportionate). This is safe (idempotent in effect) but not free (an embedder round-trip per
row, no batching — #445, explicitly out of scope); `dry_run` and progress events give cost
visibility before/during a run, matching `knowledge_backfill_relation_types`'s own precedent.

Empty-string summaries (ADR-0314: a legitimate, common state, not an error) are never sent to the
embedder on either write path — both `handle_assert_entity` and the extraction pipeline skip the
embed call and store the same `vec![0.0; dim]` sentinel directly when
`summary.trim().is_empty()`, reusing the existing embedder-failure zero-vector convention.

## Consequences

- `Entity` gains one column (`summary_embedding`) and one HNSW index
  (`entity_summary_embedding_idx`), created/dropped in the same lifecycle as the existing three
  vector indexes (`create_vector_indexes`/`drop_vector_indexes`, now also exposing
  `create_entity_summary_embedding_index`/`drop_entity_summary_embedding_index` for independent
  control — the one thing `knowledge_backfill_summary_embeddings` needs that the aggregate
  functions don't provide).
- `rrf_fuse` becomes N-ary (`&[&[(String, f64)]]`) instead of a fixed two-list signature, so
  `hybrid_entity_search` fuses three inputs (BM25, name-vector, summary-vector) while
  `hybrid_edge_search` and `hybrid_dedup_similar_entity` keep fusing exactly two, unchanged in
  behavior — both call the same implementation.
- `EntityRow` gains `#[serde(skip)] pub summary_embedding: Vec<f32>`, following `name_embedding`'s
  existing convention — invisible to every IPC/MCP response, consistent with FR-006's response-
  shape guarantee even though the new `knowledge_backfill_summary_embeddings` method is itself an
  acknowledged FR-006 deviation.
- A first post-upgrade DB open pays a one-time zero-fill cost proportional to entity count, run
  synchronously inside `migrate()`. This is a one-time cost, not a recurring regression.
- No IPC/MCP schema change to any *existing* method — `knowledge_find_entities`'s request/response
  shape is unchanged; only its internal ranking gained a third fused signal.
