# ADR-0310: Strict-Mode Relation-Type Filtering Reclassifies, Never Drops

**Status**: Accepted
**Date**: 2026-08-02
**Context**: Issue #310 — strict ontology mode drops declared aliases and never tells the model
the constraint

## Context

`strict` ontology mode was implemented only as a post-extraction filter in `episode.rs`, and
that filter threw away edges the ontology explicitly knew how to map.

Measured on 158 chunks of the `real_corpus_wal` corpus with `qwen3.6-27b`:

| | count | share |
|---|---:|---:|
| edges extracted | 2414 | — |
| strict would drop (`episode.rs:207`) | 183 | 7.6% of edges |
| …of those, declared aliases | 34 | 19% of drops |
| …genuinely outside the ontology | 149 | 81% of drops |

Three distinct defects combined to produce this:

1. **The filter ignored aliases.** It tested `vocab.contains(relation_type)` against canonical
   names only. `canonicalize.rs` already built exactly the alias→canonical map required for
   the offline `knowledge_canonicalize_relations` pass, but the ingest-time filter never
   consulted it — that map is reachable only through a manual maintenance IPC method, not
   ingest.
2. **The edge prompt never mentioned the mode.** `build_entity_types_section` had a mode-aware
   instruction; `build_fact_types_section` had none at all. The rendered edge system prompt was
   byte-identical between `open` and `strict` (same sha256). The model was never told to
   constrain relation types, so it generated freely and the constraint was enforced only
   afterward, by deletion.
3. **Aliases and keywords never reached the model.** The relation-type section showed only
   name, signature, and description. A model shown `LAUNCHED (aliases: LAUNCHED_BY,
   LAUNCHED_FROM)` would likely emit the canonical name directly.

Filtering *deletes* information, whereas instructing the model lets it *conform* information.
`LAUNCHED_BY` is semantically `LAUNCHED` with inverted endpoints — a model told to use only
declared types would emit `LAUNCHED` with source and target swapped, preserving the fact.
Dropping it loses the fact entirely.

[ADR-0033](0033-noise-edges-reclassified-not-deleted.md) established that noise edges are
reclassified to `UNCLASSIFIED`, not deleted, and
[ADR-0037](0037-relation-classification-abstention-writes-unclassified.md) did the same for
classification abstention. Strict-mode ingest-time filtering was the one remaining automated
path in the service that deleted an edge outright rather than reclassifying it.

### What `strict` actually buys

The measurements make this narrower than the mode's name suggests:

- **`strict` buys schema closure** — a fixed relation domain (declared types + `UNCLASSIFIED`)
  for tooling and aggregation to rely on.
- **It does not buy query success.** Under `open` mode the declared types already covered
  93.1% of edges in the measured corpus, so queries targeting declared types already worked
  regardless of mode. The difference `strict` makes is whether an unexpected type can *appear*
  in `relation_type`, not whether a query matches.
- After alias normalisation (below) and the new prompt instruction, the population needing
  reclassification should fall well under the 6.2% measured before this change — the prompt
  instruction in particular should push in-vocabulary compliance above `open`'s 93.1%, since the
  model is finally told what the constraint is.

## Decision

The strict-mode edge filter in `episode.rs` never drops an edge for its `relation_type` alone.

1. **Alias-normalise before checking vocabulary membership.** `canonicalize::build_alias_map`
   (factored out of `canonicalize::build_lexical_index`, which now delegates to it) is consulted
   by the ingest-time filter — the same map the offline `knowledge_canonicalize_relations` pass
   uses, not a second parallel one. A declared alias (e.g. `LAUNCHED_BY`) is rewritten to its
   canonical name (`LAUNCHED`) and retained.

2. **Reclassify what's left, never delete it.** An edge whose relation type is still outside the
   vocabulary after normalisation gets `relation_type = "UNCLASSIFIED"` — reusing the constant
   already established by ADR-0033/ADR-0037 — and its original label is preserved in the stored
   edge's `attributes` field (`RelatesToNode_.attributes`, already a free-form `STRING` column;
   no schema change required), as `{"original_relation_type": "<original>"}`.

   The alternative of reclassifying to `UNCLASSIFIED` *without* preserving the label was
   explicitly rejected: it would make `strict` worse than `open` for exactly these edges. Under
   `open`, an out-of-vocabulary edge keeps both the edge and its label. Reclassify-and-lose-the-
   label keeps the edge but destroys the label — paying information to buy closure, when the
   trade is avoidable. Preserving the label in `attributes` gives a closed, queryable
   `relation_type` *and* full recoverability: the raw label is never destroyed, so a *future*
   tool can still map it into the vocabulary as it grows.

   **This is a data-preservation guarantee, not a claim that today's
   `knowledge_canonicalize_relations` already performs that recovery.** That pass classifies
   lexically over the edge's current `relation_type`/predicate (README, "Relation typing"), never
   reads `attributes`, and on a re-run skips any edge already at `UNCLASSIFIED`. An edge
   reclassified by this feature is exactly such an edge, so `canonicalize_relations` as it exists
   today will not recover it — teaching that pass to read `attributes.original_relation_type` for
   residual `UNCLASSIFIED` edges is future work, out of scope for issue #310 (see Out of Scope:
   "Any change to `knowledge_canonicalize_relations`'s own behavior"). What this decision buys
   *now* is that the information survives for that future work to consume — not automatic
   reclassification today.

3. **The reclassified count is a per-run tally**, not just per-edge log lines: threaded through
   `AddEpisodeResult` as `edges_reclassified_unclassified` and surfaced in
   `knowledge_process_chunk`'s response, mirroring the existing
   `edges_dropped_unresolvable` field.

4. **The edge-extraction prompt is now mode-aware.** `build_fact_types_section` mirrors
   `build_entity_types_section`'s existing `match onto.mode` pattern: under `strict`, it lists
   each relation type's declared aliases/keywords and appends an instruction restricting the
   model to the declared vocabulary. Under `open`, the section is unchanged from before this
   issue — the two renderings must differ, and `open`'s must not.

5. **Entity-side filtering is unchanged.** `EntityTypeDef` has no `aliases`/`keywords` fields at
   all, so the alias-blindness defect fixed here for edges cannot exist on the entity side —
   there is no alias map to consult. Out-of-vocabulary entities are still dropped, not
   reclassified; extending reclassify-not-drop to entities is a larger design decision (what to
   reclassify an entity's type *to*, since there is no untyped-but-present sentinel comparable to
   `UNCLASSIFIED`) that is out of scope for this issue.

## Consequences

1. **No relation-type-based automated deletion path remains in ingest.** This is the third and
   final installment of the reclassify-not-delete line of work started by ADR-0033
   (offline canonicalize) and ADR-0037 (classification abstention) — it closes the gap those two
   left open at the one remaining automated deletion point, `add_episode`'s strict-mode filter.

2. **`relation_type` stays closed under `strict`**: declared types plus `UNCLASSIFIED`. Nothing
   querying `relation_type` sees an unexpected value; the original out-of-vocabulary label lives
   in `attributes`, which is already free-form and was not part of any closed-set assumption.

3. **The existing test `strict_mode_relation_type_drops_non_matching_edges` was invalidated**,
   not merely extended, and is renamed to
   `strict_mode_relation_type_reclassifies_non_matching_edges_to_unclassified` with assertions
   on the reclassified `relation_type` and the recoverable `attributes` label.

4. **The edge-extraction prompt grows under `strict`** for ontologies with many aliases/keywords
   declared. Nothing in the spec suggested this is a blocking concern at realistic ontology
   sizes, but it is a real, measurable cost of making the constraint visible to the model.

5. **`knowledge_canonicalize_relations` does not yet consume the preserved label.** It classifies
   lexically over `relation_type`/predicate, never reads `attributes`, and skips edges already at
   `UNCLASSIFIED` on a re-run — so an edge reclassified by this feature is not, today, mapped
   back into the vocabulary by that pass. This decision preserves the information so that future
   work can teach `canonicalize_relations` (or a new tool) to consume
   `attributes.original_relation_type`; it does not itself deliver that recovery, and doing so is
   explicitly out of scope for issue #310.

## Alternatives Considered

- **Reclassify to `UNCLASSIFIED` without preserving the original label**: rejected — see
  "Reclassify what's left, never delete it" above. Loses information `open` mode would have kept.
- **Retain the edge with its original (non-canonical) `relation_type`, flagged some other way**:
  rejected — this would defeat the schema-closure guarantee `strict` mode is meant to provide;
  a query against `relation_type` would need to account for an open-ended set of values, which is
  exactly what `strict` exists to avoid.
- **Extend reclassify-not-drop to the entity-side filter in this issue**: rejected as out of
  scope — no User Story or Success Criterion in the spec calls for it, and it requires a genuine
  new design decision (what an "UNCLASSIFIED entity type" sentinel would be) that deserves its
  own issue rather than being bundled in here.
- **Build a second, ingest-local alias map instead of reusing `canonicalize::build_alias_map`**:
  rejected — the spec's Key Entities section explicitly calls out the existing `exact` map in
  `canonicalize.rs` as the thing to add a second consumer to, not something to duplicate.

## References

- Issue #310 — strict ontology mode drops declared aliases and never tells the model the
  constraint
- [ADR-0033](0033-noise-edges-reclassified-not-deleted.md) — reclassify-not-delete precedent for
  the offline canonicalize pass
- [ADR-0037](0037-relation-classification-abstention-writes-unclassified.md) — reclassify-not-
  delete precedent for classification abstention
- `crates/core/src/episode.rs` — the strict-mode edge/entity filters
- `crates/core/src/canonicalize.rs` — `build_alias_map`, shared by the offline pass and ingest
- `crates/core/src/prompts/mod.rs` — `build_fact_types_section`, now mode-aware
