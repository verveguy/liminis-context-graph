# Feature Specification: Strict ontology mode discards relations the ontology knows how to keep

**Feature Branch**: `fabrik/issue-310`
**Created**: 2026-08-01
**Status**: Draft
**Input**: User description: "core: strict ontology mode drops declared aliases and never tells the model the constraint"

## Background

`strict` ontology mode is implemented **only** as a post-extraction filter, and that filter throws away edges the ontology explicitly describes how to map.

Measured on 158 chunks of the `real_corpus_wal` corpus with `qwen3.6-27b` (cassette `qwen3.6-27b-strict.jsonl`, captured 2026-08-01):

| | count | share |
|---|---:|---:|
| edges extracted | 2414 | — |
| strict would drop (`episode.rs:207`) | **183** | 7.6% of edges |
| …of those, **declared aliases** | **34** | 19% of drops |
| …genuinely outside the ontology | 149 | 81% of drops |

The fixture declares 25 canonical relation types and **86 aliases**. Dropped despite being declared aliases: `LAUNCHED_BY` (21) → `LAUNCHED`, `USED` (5) → `USES`, `SERVED_UNDER` (2) → `SERVED_IN`, `LAUNCHED_FROM` (2), `DOCKED_WITH`, `LAUNCHED_ON`, `TESTED_ON`, `LOCATED_AT`.

Three distinct defects combine to produce this:

1. **The filter ignores aliases.** `episode.rs:207` tests `vocab.contains(rt.as_str())` against canonical names only. `canonicalize.rs:106-107` already builds exactly the alias→canonical map required (`exact.insert(alias.clone(), rt.name.clone())`), but `episode.rs` never consults it. `canonicalize` is reachable only via the `knowledge_canonicalize_relations` IPC method — a manual maintenance pass, not part of ingest — so nothing normalises before the filter runs.
2. **The edge prompt never mentions the mode.** `build_entity_types_section` (`prompts/mod.rs:47-57`) emits a mode-specific instruction under `match onto.mode`. `build_fact_types_section` (`prompts/mod.rs:61-83`) has **no mode handling at all** — the rendered edge system prompt is byte-identical between `open` and `strict` (verified: same sha256, 4486 bytes). The model is never told to constrain relation types, so it generates freely and the constraint is enforced only afterward, by deletion.
3. **Aliases and keywords never reach the model.** `build_fact_types_section` emits name, signature and description only. A model shown `LAUNCHED (aliases: LAUNCHED_BY, LAUNCHED_FROM, …)` would likely emit the canonical name directly.

**Why this matters beyond tidiness:** filtering *deletes* information, whereas instructing the model lets it *conform* information. `LAUNCHED_BY` is semantically `LAUNCHED` with inverted endpoints — a model told to use only declared types would emit `LAUNCHED` with source and target swapped, preserving the fact. Dropping it loses the fact entirely.

**Precedent:** [ADR-0033](../../docs/adr/0033-noise-edges-reclassified-not-deleted.md) established that noise edges are reclassified to `UNCLASSIFIED`, not deleted, and [ADR-0037](../../docs/adr/0037-relation-classification-abstention-writes-unclassified.md) did the same for classification abstention. Strict-mode filtering silently contradicts both — it is the one remaining automated path in the service that deletes an edge outright rather than reclassifying it.

**What `strict` actually buys.** The measurements make this narrower than the mode's name suggests:

- **`strict` buys schema closure** — a fixed relation domain (26 values: 25 declared + `UNCLASSIFIED`) for tooling and aggregation to rely on.
- **It does not buy query success.** Under `open` mode the 25 declared types already cover 93.1% of edges, so queries targeting declared types already work today regardless of mode. The difference `strict` makes is whether an unexpected type can *appear* in `relation_type`, not whether a query matches.
- After FR-001 (alias normalisation) and FR-002 (the prompt instruction), the population needing reclassification should fall well under the 6.2% measured today — FR-002 in particular should push in-vocabulary compliance above `open`'s 93.1%, since the model is finally told what the constraint is.

This is related to #307 (which governs discarding data on *extraction failure*) but is not blocked by it: #307 is about calls that error out; this is about calls that **succeed** and whose output is then thrown away by a post-hoc filter.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A declared alias is not destroyed (Priority: P1)

A workspace runs `strict` mode with an ontology that declares relation type `LAUNCHED` with alias `LAUNCHED_BY`. The extractor emits an edge with relation type `LAUNCHED_BY`. Today that edge is silently dropped by the strict-mode filter. After this change, the edge is retained and recorded as `LAUNCHED` — the fact is not lost.

**Why this priority**: This is the core defect. 34 of 183 measured drops (19%) are edges the ontology already declares how to map; today they are being destroyed for no reason the ontology's own configuration doesn't already resolve.

**Independent Test**: Exercise the strict-mode filter (or a full ingest) with an ontology declaring an alias, feed it an edge using the alias's spelling, and confirm the resulting stored edge is present with the canonical relation type rather than absent.

**Acceptance Scenarios**:

1. **Given** a `strict`-mode ontology declaring relation type `LAUNCHED` with alias `LAUNCHED_BY`, **When** an edge is extracted with relation type `LAUNCHED_BY`, **Then** the edge is retained with relation type `LAUNCHED`, not dropped.
2. **Given** the same ontology, **When** an edge is extracted with relation type `launched_by` (different case/separator), **Then** it is still recognized as the `LAUNCHED_BY` alias and retained as `LAUNCHED`.

---

### User Story 2 - The model is told what the constraint is (Priority: P1)

Under `strict` mode, the edge system prompt differs from `open` mode and states that only declared relation types (identified by name, and by any declared aliases/keywords) may be used. Generation effort is not spent producing edges that are destined for deletion, and the model is far more likely to emit a declared alias's canonical form directly.

**Why this priority**: Without this, the fix in User Story 1 is a safety net for a preventable problem — the model is never told the rule it's being held to. This also directly reduces how often User Story 1's alias-normalisation path needs to fire, and reduces wasted generation on out-of-vocabulary relation types entirely.

**Independent Test**: Render the edge system prompt for the same ontology once under `open` and once under `strict`. Confirm the two renderings differ, and that the `strict` rendering contains an explicit instruction restricting relation types to the declared vocabulary.

**Acceptance Scenarios**:

1. **Given** an ontology with relation types declared, **When** the edge system prompt is rendered for `strict` mode, **Then** it contains a mode-specific instruction (mirroring the existing entity-type instruction) stating that only declared relation types may be used.
2. **Given** the same ontology, **When** the edge system prompt is rendered for `open` mode, **Then** it is unchanged from today's rendering (no new constraint language).
3. **Given** a relation type with declared aliases, **When** the edge system prompt section for that relation type is rendered, **Then** the aliases (and keywords, where present) are listed so the model can see them.

---

### User Story 3 - A genuinely unmodelled relation is preserved, not destroyed (Priority: P2)

An edge is extracted with relation type `ALSO_KNOWN_AS`, which is in neither the ontology's declared type list nor any alias. Instead of being silently dropped, its `relation_type` is set to `UNCLASSIFIED` and the original `ALSO_KNOWN_AS` label is preserved in the edge's `attributes` field, so no information is destroyed and the reclassification is deliberate and observable.

**Why this priority**: Lower priority than User Story 1/2 because it doesn't change what happens to the *modelled* 19% of drops, but it closes the visibility and data-loss gap for the remaining 81% (149 of 183 measured drops) that are genuinely outside the ontology today and will still need reclassification after alias-normalisation.

**Independent Test**: Run strict-mode extraction over a corpus containing at least one relation type outside the ontology's vocabulary and its aliases. Confirm the edge is retained with `relation_type = UNCLASSIFIED`, its original label recoverable from `attributes`, and the reclassification reflected in a per-run count.

**Acceptance Scenarios**:

1. **Given** a `strict`-mode ontology and an edge whose relation type is outside the vocabulary after alias normalisation, **When** extraction completes, **Then** the edge is retained with `relation_type` set to `UNCLASSIFIED` and its original relation type preserved in the edge's `attributes` field.
2. **Given** a run in which N edges hit this out-of-vocabulary path, **When** the run completes, **Then** the run's output surfaces a count of N (not merely N individual log lines).

---

### Edge Cases

- An alias declared under two different canonical relation types (a collision) — resolved however the existing `canonicalize.rs` alias map already resolves it today; this issue does not change collision-resolution semantics, only where the map is consulted.
- An alias string that is identical to another relation type's own canonical name.
- Case and separator variants (`launched_by` vs `LAUNCHED_BY`) — `normalize_relation_type` already exists and must be the single normalisation point used by both the alias map and the filter.
- An ontology with relation types but no declared aliases (the common case) — behavior must be unchanged from today for these edges.
- `open` and `freeform` ontology modes must see no behavior change from this issue at all (see SC-004).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST alias-normalise a relation type **before** the strict-mode filter evaluates it, reusing the existing alias→canonical map built in `canonicalize.rs` rather than constructing a second, parallel map.
- **FR-002**: `build_fact_types_section` MUST emit a mode-specific instruction under `match onto.mode`, mirroring the existing pattern in `build_entity_types_section`, so the rendered edge system prompt differs between `open` and `strict`.
- **FR-003**: The relation-type section of the edge prompt MUST expose each declared relation type's aliases (and keywords, where present), so the model can see the full set of accepted spellings and is more likely to emit the canonical name directly.
- **FR-004**: An edge whose relation type is outside the ontology's vocabulary **after** alias normalisation MUST be reclassified, not dropped: `relation_type` is set to `UNCLASSIFIED` (consistent with ADR-0033/ADR-0037), and the edge's original out-of-vocabulary relation type MUST be preserved in the edge's existing `attributes` field (`RelatesToNode_.attributes`, already a free-form `STRING` column — no schema change required). This preserves full recoverability (a future `knowledge_canonicalize_relations` pass can map preserved labels into the vocabulary as it grows) while keeping `relation_type` itself closed to the declared vocabulary. This decision MUST be recorded in an ADR numbered `0310`, including the "what `strict` actually buys" framing from Background (schema closure, not query success).
- **FR-005**: The count of edges reclassified per FR-004 MUST be observable as a per-run tally, not only as a per-edge debug/`eprintln` line.
- **FR-006**: The entity-side strict-mode filter (`episode.rs:175`) MUST be checked for the same alias-blindness defect described for the edge-side filter, and fixed consistently (per FR-001) if present. Where an entity is reclassified under the entity-side equivalent of FR-004, its original entity type MUST be preserved analogously (not overwritten and lost), consistent with the edge-side treatment.

### Key Entities *(if the feature involves data)*

- **Relation Type Alias Map**: The existing `exact` map built in `canonicalize.rs:106-107` (alias string → canonical relation-type name). This issue adds a second consumer (the strict-mode filter) rather than a new map.
- **Out-of-Vocabulary Disposition**: For an edge whose relation type does not match any declared canonical name or alias after normalisation, `relation_type` is set to `UNCLASSIFIED` and the original label is preserved in `attributes`.
- **Out-of-Vocabulary Tally**: A per-run count of edges that hit the out-of-vocabulary disposition path, per FR-005.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Re-running the strict-mode capture over the same corpus retains all 34 alias-mapped edges that are dropped today (`LAUNCHED_BY`, `USED`, `SERVED_UNDER`, `LAUNCHED_FROM`, `DOCKED_WITH`, `LAUNCHED_ON`, `TESTED_ON`, `LOCATED_AT`), stored under their canonical relation type.
- **SC-002**: The rendered edge system prompt differs between `open` and `strict` for the same ontology (today they are byte-identical — same sha256, 4486 bytes).
- **SC-003**: Out-of-vocabulary edge reclassification is reported as a count per run.
- **SC-004**: `open` and `freeform` ontology-mode behavior is unaffected — no change in extracted or stored edges for either mode, before or after this change.
- **SC-005**: For every edge reclassified to `UNCLASSIFIED` under FR-004, the original relation type is recoverable from the edge's `attributes` field — no information is destroyed by the reclassification.

## Assumptions

- The eval harness calls the extractor directly and bypasses `episode.rs`, so it cannot currently observe strict-mode filtering at all. Measuring SC-001 requires either an ingest-level test or exercising the filter directly in a unit test — the eval harness itself is out of scope for this change.
- `normalize_relation_type` (referenced by the Edge Cases section) already exists and is assumed to remain the single point of case/separator normalisation; this issue does not introduce a second normalisation routine.
- This issue is scoped to *ingest-time* behavior (the extraction → filter → store path in `episode.rs`). It does not change the manual `knowledge_canonicalize_relations` maintenance pass itself, beyond making its alias map reusable by the ingest-time filter.

## Out of Scope

- Any change to `knowledge_canonicalize_relations`'s own behavior as a manual maintenance IPC method — this issue only adds a second consumer of its alias map.
- Changes to entity-side `open`/`freeform` behavior, or any entity-type prompt content beyond what already exists.
- Resolving #307 (extraction-failure data discard) — related in spirit, not blocked by, and not resolved here.
- Any spend guard, budget tracking, or session-level machinery for managing extraction cost — that is #307's territory, not this issue's.
- Retroactively reprocessing edges already dropped by strict mode in production workspaces before this fix ships (no backfill/migration is included).

## Source References

- `crates/core/src/episode.rs:207` — the strict-mode edge filter (`vocab.contains(rt.as_str())`) that ignores aliases.
- `crates/core/src/episode.rs:175` — the entity-side strict-mode filter, to be checked per FR-006.
- `crates/core/src/canonicalize.rs:106-107` — the existing alias→canonical map (`exact.insert(alias.clone(), rt.name.clone())`) that FR-001 requires reuse of.
- `crates/core/src/prompts/mod.rs:47-57` — `build_entity_types_section`, the existing mode-aware pattern FR-002 mirrors.
- `crates/core/src/prompts/mod.rs:61-83` — `build_fact_types_section`, which FR-002/FR-003 modify.
- `crates/core/src/schema.rs` — `RelatesToNode_`'s existing free-form `attributes STRING` column, used by FR-004 to preserve the original relation type.
- `docs/adr/0033-noise-edges-reclassified-not-deleted.md` — precedent for reclassify-not-delete.
- `docs/adr/0037-relation-classification-abstention-writes-unclassified.md` — precedent for abstention writing `UNCLASSIFIED`.
- Issue #307 — related (extraction-failure data discard), not blocking.
