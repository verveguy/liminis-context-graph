# ADR-0312: Strict-Mode Entity-Type Filtering Reclassifies, Never Drops

**Status**: Accepted
**Date**: 2026-08-02
**Context**: Issue #312 — strict mode still deletes out-of-vocabulary entities while edges are
now preserved

## Context

[ADR-0310](0310-strict-mode-reclassifies-not-drops.md) (issue #310) established that strict
ontology mode must never destroy an extracted relation: an out-of-vocabulary edge is
reclassified to `UNCLASSIFIED` with its original label preserved in `attributes`, rather than
dropped. The entity side was deliberately left alone in that work — correctly, for the reason
ADR-0310 itself gives: `EntityTypeDef` has no `aliases`/`keywords` fields, so the
alias-blindness defect #310 fixed on the edge side cannot exist for entities. There is no alias
map to consult.

But the alias question and the delete-vs-reclassify question are separate, and only the first
was in scope for #310. ADR-0310's own "Alternatives Considered" section flags the entity-side
gap explicitly: extending reclassify-not-drop to entities was "rejected as out of scope...
it requires a genuine new design decision (what an `UNCLASSIFIED` entity type sentinel would
be) that deserves its own issue."

The result was an asymmetry visible in a single function: `add_episode` preserved every edge it
couldn't type, and deleted every entity it couldn't type. Deleting an entity is worse than
deleting an edge: entities are edge endpoints, so a dropped entity cascades into edge loss. The
old code made this cascade explicit — when the strict-mode entity filter emptied
`extraction.entities` for a chunk, it also unconditionally cleared `extraction.edges` for that
chunk, destroying edges that were never themselves out of vocabulary.

Entity-type conformance is already very high (97.5–99.9% per the #310 corpus measurement), so
the affected population is small — this is a correctness and consistency fix, not a recall win.

## Decision

The strict-mode entity filter in `episode.rs` never drops an entity for its `entity_type` alone.

1. **An entity whose normalized type is empty or the literal `"Entity"` passes through
   unchanged.** This means "no specific type was extracted," exactly as it does everywhere else
   in `add_episode` under every ontology mode — it is not a rejection and must not be routed
   through the reclassification path below.

2. **A non-empty, non-matching type is reclassified to a new sentinel label, `Unclassified`,
   never deleted.** `Unclassified` is the entity-side counterpart of the relation-side
   `UNCLASSIFIED` (ADR-0033/ADR-0037/ADR-0310), but a distinct value: entity types in this
   codebase are PascalCase (`Person`, `Spacecraft`), a different casing convention from relation
   types' `SCREAMING_SNAKE_CASE`. The original out-of-vocabulary type is preserved on the
   extracted entity (`original_entity_type`) and, at insert time, written into the stored
   entity's `attributes` field as `{"original_entity_type": "<original>"}` — mirroring the
   edge-side `attributes.original_relation_type` convention exactly. `Entity.attributes` is an
   existing free-form `STRING` column, previously always written as the literal `"{}"`; this is
   its first real consumer, so no schema change is required.

3. **Reclassification means substituting the label, not appending to the set.** An entity's type
   is represented as one or more entries in `EntityRow.labels` (`Entity` always present,
   declared/ancestor types appended). The out-of-vocabulary type is not appended to `labels` as
   one of its own entries — that would reopen the closed set `strict` exists to guarantee.
   Instead `Unclassified` is appended in its place, alongside the always-present base `Entity`
   label.

4. **The reclassified count is a per-run tally**, `entities_reclassified_unclassified` on
   `AddEpisodeResult`, surfaced in `knowledge_process_chunk`'s response — mirroring
   `edges_reclassified_unclassified`. Unlike the edge-side tally, which is deferred to Phase C
   because a reclassified edge can still be dropped afterward as self-referential or
   unresolvable (ADR-0051), the entity tally is counted directly in the Phase A filter loop: a
   reclassified entity is never subsequently dropped by anything downstream in this pipeline, so
   there is no equivalent desync risk between "counted" and "persisted."

5. **The `is_empty()` → `edges.clear()` cascade is deleted outright**, not narrowed. Once the
   filter stops removing entities, `extraction.entities.len()` is invariant across it, so that
   branch could now only fire when the extractor itself returned zero entities to begin with —
   an unrelated case Phase C's existing per-edge unresolvable-endpoint handling already covers
   more observably (via `edges_dropped_unresolvable`) than the old silent `eprintln` + full
   clear.

## Why This Follows Independently, Not Just by Analogy to ADR-0310

Two arguments, not one, support reclassify-not-drop for entities — and neither depends on
"ADR-0310 did it for edges, so do it here too":

- **The edge-endpoint cascade.** An entity is an edge endpoint. Dropping it is strictly worse
  than dropping an edge, because it destroys every edge referencing it too — including edges
  whose own `relation_type` was perfectly in-vocabulary and had nothing to do with the rejected
  entity type. This risk has no edge-side equivalent; it is specific to why entity deletion is
  worse than edge deletion.
- **Schema closure.** `strict` mode's entire distinguishing value is a closed type domain for
  tooling and aggregation to rely on (the same "what `strict` actually buys" framing ADR-0310
  uses for relation types). Retaining an entity's raw out-of-vocabulary type string instead of a
  sentinel would make `strict` behaviorally indistinguishable from `open` for exactly this
  population, defeating the reason to run `strict` at all.

Both arguments independently rule out the same two rejected alternatives ADR-0310 weighed for
edges (see below) — the alias-blindness reasoning ADR-0310 gives for *not* touching entities in
#310 is a separate question (no alias map exists) from the drop-vs-reclassify question this ADR
resolves.

## Consequences

1. **No type-based automated deletion path remains for entities.** This is the entity-side
   installment of the reclassify-not-delete line of work started by ADR-0033 (offline
   canonicalize), continued by ADR-0037 (classification abstention) and ADR-0310 (edge-side
   strict-mode filtering) — it closes the one remaining automated deletion point on the entity
   side.
2. **`labels` stays closed under `strict`** for the type dimension: declared types plus
   `Unclassified`. The original out-of-vocabulary label lives in `attributes`, already free-form
   and not part of any closed-set assumption.
3. **Dedup is unaffected.** Phase B resolution is keyed by name (`get_entity_by_name_ci` /
   embedding candidates), not by type, so a reclassified entity dedups identically to any other
   entity — including merging into a pre-existing entity already stored under its correct
   declared type. One side effect: the existing "type conflict" log line in the name-match dedup
   branch now also fires, harmlessly, whenever an `Unclassified`-typed extraction name-matches an
   existing entity that already has a real declared type. This is log noise, not a correctness
   issue.
4. **The existing test `strict_mode_entity_type_still_drops_not_reclassifies` was invalidated**,
   not merely extended, and is replaced by
   `strict_mode_entity_type_reclassifies_not_drops` with assertions on the reclassified
   `labels` and the recoverable `attributes.original_entity_type`.
5. **No entity-side `knowledge_canonicalize_relations` equivalent exists or is introduced.** An
   entity reclassified by this feature surfaces automatically as `OffOntology` in the existing
   `knowledge_reprocess_entity_types` scope (it is, by construction, not in
   `onto.entity_type_names()`) — no new plumbing needed for that integration, and building a new
   entity-side canonicalize pass remains out of scope for this issue.

## Alternatives Considered

- **Drop the entity, as before**: rejected — an entity is an edge endpoint, so dropping it
  cascades into edge loss the entity's own out-of-vocabulary type had nothing to do with.
  Reclassify-not-drop is also the established precedent for every other automated deletion path
  in this service; the entity-type filter was the one remaining path that still deleted outright.
- **Retain the entity's original (non-canonical) type, flagged some other way**: rejected for the
  same reason ADR-0310 rejected it for edges — it would defeat the schema-closure guarantee
  `strict` mode exists to provide. `open` mode already pushes the raw (post-normalization)
  `entity_type` onto `labels` with no vocabulary check, so `labels` is already open-ended under
  `open`; retaining an arbitrary out-of-vocabulary type string under `strict` would make `strict`
  behaviorally indistinguishable from `open` for exactly this population.
- **Defer the `entities_reclassified_unclassified` tally to Phase C**, mirroring the edge-side
  deferral: rejected — that deferral exists specifically because a reclassified *edge* can still
  be dropped afterward (self-referential, unresolvable endpoint), creating a desync risk between
  "counted" and "persisted" that ADR-0051 fixed by making Phase C authoritative. An entity, once
  reclassified, is never later dropped by anything in this pipeline, so deferring would add
  complexity for no correctness benefit.
- **Keep the `is_empty()` → `edges.clear()` cascade as defensive dead code** for "extractor
  returned zero entities": rejected — that residual case is already handled correctly and more
  observably by Phase C's existing per-edge unresolvable-endpoint handling, and no test could
  distinguish the old branch's behavior from the generic path once it stopped being reachable via
  type-vocabulary rejection.
- **Introduce `aliases`/`keywords` fields to `EntityTypeDef`**: out of scope — ADR-0310 already
  confirmed no such fields exist and none are needed for this issue; the defect here is
  delete-vs-reclassify, not alias-blindness.

## References

- Issue #312 — strict mode still deletes out-of-vocabulary entities while edges are now
  preserved
- [ADR-0310](0310-strict-mode-reclassifies-not-drops.md) — the edge-side precedent this ADR
  extends, cited as precedent rather than authority
- [ADR-0033](0033-noise-edges-reclassified-not-deleted.md) — reclassify-not-delete precedent for
  the offline canonicalize pass
- [ADR-0037](0037-relation-classification-abstention-writes-unclassified.md) — reclassify-not-
  delete precedent for classification abstention
- [ADR-0051](0051-edge-endpoint-salvage-and-deferred-drop.md) — why the edge-side reclassify
  tally is deferred to Phase C, and why the entity-side tally does not need the same deferral
- `crates/core/src/episode.rs` — the strict-mode entity filter
- `crates/core/src/ontology.rs` — `EntityTypeDef`, `normalize_entity_type`, `has_entity_types`
- `crates/core/src/corrections.rs` — `is_off_ontology`, the existing offline entity-reclassification
  machinery an `Unclassified`-labeled entity surfaces through automatically
