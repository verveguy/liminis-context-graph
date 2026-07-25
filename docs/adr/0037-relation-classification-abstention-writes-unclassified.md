# ADR-0037: Relation Classification Has No Open-Ended Mode and Abstention Writes `UNCLASSIFIED`

**Status**: Accepted
**Date**: 2026-07-25
**Context**: Issue #210 — `knowledge_reprocess_relation_types`

## Context

ADR-0004 added `classify_entities` to the `Extractor` trait, establishing the pattern that
`knowledge_reprocess_relation_types` (issue #210) extends to relations: a new
`classify_relations` trait method, implemented by every `Extractor` (`AnthropicExtractor`,
`LlmRouter`, `MockExtractor`, `ConfigurableExtractor`, and the test-only implementors), driven
through `LlmRouter`'s existing primary/fallback routing and telemetry.

Two aspects of the entity-side precedent do not carry over cleanly to relations, and this ADR
records the deliberate divergence for each.

### 1. No open-ended classification mode

`classify_entities` supports `allowed_types: Option<&[String]>` — when `None`, the LLM freely
invents PascalCase entity type labels (`Person`, `Organization`, …). This lets
`knowledge_reprocess_entity_types`'s `untyped` scope work even with no ontology configured.

Relation classification has no equivalent open-ended mode. The entire value of this tool over
the two existing relation-typing tools (`knowledge_canonicalize_relations`,
`knowledge_backfill_relation_types` — see their doc comments and #204/#205) is that it asks the
LLM to pick from the ontology's *declared* relation types with descriptions, not to invent a
predicate string. A free-form relation label carries none of the semantic guarantees a declared
menu does, and there is no analogous "PascalCase noun" convention for relation predicates the way
there is for entity types. Consequently `classify_relations`'s `allowed_types` parameter is a
plain (required) `&[(String, Option<String>)]` slice, not an `Option`-wrapped one, and
`knowledge_reprocess_relation_types` returns a structured `{success: false, error: ...}` for
**every** scope value (`untyped`, `off_ontology`, `all`) when the ontology declares no relation
types — unlike `knowledge_reprocess_entity_types`, whose `untyped` scope only requires an
ontology for `off_ontology`/`all`.

### 2. Abstention is a real write, not a no-op

For `classify_entities`, an empty-string response means "leave the entity's labels untouched" —
abstention is silently a no-op; the entity keeps whatever label set it already had.

For relations, `knowledge_reprocess_relation_types` treats abstention differently: when the LLM
returns an empty string for an edge, the handler writes the literal sentinel `UNCLASSIFIED` to
that edge's `relation_type` (unless it is already `UNCLASSIFIED`, in which case idempotency
skips the write). This is a genuine, WAL-durable mutation, counted in `reclassified_count`, not
`unchanged_count`.

This mirrors the existing `UNCLASSIFIED` convention already established by
`knowledge_canonicalize_relations` (ADR-0033: residual/noise edges are set to `UNCLASSIFIED`,
never deleted or left ambiguous) — an edge's `relation_type` should always resolve to *something*
after a typing pass runs over it, so that "unclassified" is a queryable, first-class state rather
than indistinguishable from "never processed." Leaving abstained edges untouched (mirroring
entities) would make a `scope=untyped` run partially silently no-op on facts the LLM cannot map,
which is the exact failure mode (#204/#205) this tool exists to fix.

## Decision

1. `classify_relations`'s `allowed_types` parameter is always required and non-empty.
   `knowledge_reprocess_relation_types` fails with a structured error before doing any work if
   the configured ontology declares zero relation types, for all three scope values.
2. When the LLM abstains (returns an empty string) for an edge, the handler writes
   `relation_type = "UNCLASSIFIED"` — a real mutation, subject to the same idempotency check as
   any other verdict (skip the write only if the edge's current value already equals the
   computed verdict, i.e. it's already `UNCLASSIFIED`).

## Consequences

- A future 10th `Extractor` implementor must supply `classify_relations` with these semantics in
  mind: an empty-string entry means "I could not classify this edge," and the *caller*
  (`reprocess_relations::reprocess_relation_types`), not the extractor, is responsible for
  turning that into the `UNCLASSIFIED` sentinel. The extractor itself never emits the literal
  string `"UNCLASSIFIED"`.
- Re-running the same scope on a graph where every candidate already carries its correct verdict
  (a declared type, or `UNCLASSIFIED` for edges the LLM still cannot map) is idempotent:
  `reclassified_count: 0`, no new WAL entries — verified by
  `test_reprocess_relation_scope_off_ontology_idempotency` in `crates/core/tests/ipc_parity.rs`.
- Anyone comparing `handle_reprocess_entity_types` and `handle_reprocess_relation_types` side by
  side must not assume symmetry on abstention handling; this ADR is the record of why they
  differ.

## Alternatives Considered

- **Leave abstained edges untouched, mirroring entity classification exactly**: rejected because
  it would silently reproduce the "some edges never get typed" gap this tool exists to close —
  an operator running `scope=untyped` would have no way to distinguish "already processed, LLM
  couldn't map it" from "never processed."
- **Support an open-ended relation classification mode for parity with entities**: rejected
  because a freely-invented relation predicate has no declared description, aliases, or keywords
  to anchor it — it would reintroduce exactly the kind of ungoverned taxonomy sprawl
  `knowledge_backfill_relation_types`'s fact-prefix pseudo-types already cause (#205), which this
  tool is meant to supersede.

## References

- ADR-0004 — established the `classify_entities` trait-method pattern this ADR extends
- ADR-0033 — established the `UNCLASSIFIED` sentinel convention for `knowledge_canonicalize_relations`
- ADR-0030 — batched write-lock discipline, reused unchanged by this feature's Phase C
- Issue #204 — reported the classification gap and the fact-based-classification prototype behind this feature
- Issue #205 — `backfill_relation_types`'s fact-prefix pseudo-typing gap, subsumed by this feature's `untyped` scope
- Issue #210 — this feature
