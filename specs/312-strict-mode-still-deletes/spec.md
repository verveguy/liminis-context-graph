# Feature Specification: Strict mode still deletes out-of-vocabulary entities, while edges are now preserved

**Feature Branch**: `fabrik/issue-312`
**Created**: 2026-08-02
**Status**: Draft
**Input**: User description: "core: strict mode still deletes out-of-vocabulary entities while edges are now preserved"

## Background

[#310](https://github.com/verveguy/liminis-context-graph/issues/310) (merged as [#311](https://github.com/verveguy/liminis-context-graph/pull/311), [ADR-0310](../../docs/adr/0310-strict-mode-reclassifies-not-drops.md)) established that strict ontology mode must never destroy an extracted relation: an out-of-vocabulary edge is reclassified to `UNCLASSIFIED` with its original label preserved in `attributes`, rather than dropped.

The entity side was audited during that work and deliberately left alone — correctly, for the reason ADR-0310 gives: `EntityTypeDef` (`crates/core/src/ontology.rs`) has no `aliases`/`keywords` fields, so the alias-blindness defect #310 fixed on the edge side cannot exist for entities. That audit is locked in by `strict_mode_entity_type_still_drops_not_reclassifies` in `crates/core/tests/ontology_integration.rs`.

But the alias question and the **delete-vs-reclassify** question are separate, and only the first was in scope for #310. ADR-0310 itself flags this explicitly, in its own "Alternatives Considered" section: extending reclassify-not-drop to entities was "rejected as out of scope... it requires a genuine new design decision (what an `UNCLASSIFIED` entity type sentinel would be) that deserves its own issue." This is that issue.

The result today is an asymmetry visible in a single function: `add_episode` preserves every edge it cannot type, and deletes every entity it cannot type.

`crates/core/src/episode.rs` (strict entity filter, `add_episode`, ~line 201):

```rust
extraction.entities.retain(|e| {
    let normalized = normalize_entity_type(&e.entity_type);
    if vocab.contains(&normalized) { true } else { /* eprintln + drop */ false }
});
```

Deleting an entity is worse than deleting an edge: entities are edge endpoints, so a dropped entity cascades into edge loss. The current code makes this cascade explicit — when the strict-mode entity filter empties `extraction.entities` for a chunk, it also unconditionally clears `extraction.edges` for that chunk ("edges to avoid wasted embedding work; endpoints are gone"), destroying edges that were never themselves out of vocabulary. This is the same silent-loss shape as two defects already fixed this cycle: #306 (truncation recorded as clean) and #310 (edges deleted rather than reclassified).

Measured context from the #310 investigation: entity-type conformance is already very high — 97.5–98.9% in freeform (no ontology configured), 99.9% under `open` mode — so the affected population is small. That makes this cheap to fix and cheap to get wrong quietly, since the volume is low enough that nobody notices the loss.

### Decision: entities reclassify, they do not drop

This spec resolves the issue's original "DECISION REQUIRED" flag on FR-001 as **option (b): reclassify to a closed-vocabulary fallback entity type, with the original type preserved** — mirroring ADR-0310's edge-side decision. The two options ADR-0310 also weighed for edges apply with the same outcome here, argued independently rather than assumed:

- **Option (a), drop, is rejected** for the reason already stated above and in the original issue: an entity is an edge endpoint, so dropping it is strictly worse than dropping an edge — it cascades into edge loss the entity's own out-of-vocabulary type had nothing to do with. Reclassify-not-drop is also the established precedent for every other automated deletion path in this service (ADR-0033, ADR-0037, ADR-0310); the entity-type filter is the one remaining path that still deletes outright.
- **Option (c), retain the original (non-canonical) type, flagged some other way, is rejected** for the same reason ADR-0310 rejected it for edges: it defeats the schema-closure guarantee `strict` mode exists to provide. `open` mode already pushes the extractor's raw (post-normalization) `entity_type` onto a node's `labels` with no vocabulary check at all — so `labels` is already open-ended under `open`. `strict` mode's entire distinguishing value, per ADR-0310's own "what `strict` actually buys" framing, is a closed type domain for tooling and aggregation to rely on. Retaining an arbitrary out-of-vocabulary type string under `strict` would make `strict` behaviorally indistinguishable from `open` for exactly the population this issue is about, which defeats the reason to run `strict` at all.

This mirrors ADR-0310's reasoning, but is not assumed from it — see the two paragraphs above for why the same conclusion in fact follows independently for entities. The formal decision record for this issue is `docs/adr/0312-<slug>.md`, written during the Plan/Implement stage per this repository's convention that ADR numbers are issue numbers (see `CLAUDE.md`); it must state this reasoning and cite ADR-0310 as precedent, not authority.

### Fallback type and preservation field

- **Fallback type**: entity types in this codebase are PascalCase (`Person`, `Spacecraft`; see `normalize_entity_type`), a different convention from relation types' `SCREAMING_SNAKE_CASE` (`WORKS_AT`, `UNCLASSIFIED`). The entity-side fallback introduced by this issue is a new sentinel, **`Unclassified`**, matching entity-type casing rather than reusing the relation-side `UNCLASSIFIED` literal. It is a genuinely new concept — there is no existing entity-side equivalent, per the issue's own FR-002.
- **Structural difference from the edge case**: `relation_type` is a single scalar column on `RelatesToNode_`, so ADR-0310 could simply overwrite it with the sentinel. An entity's type is not a single scalar column — it is represented as one or more entries in `EntityRow.labels` (`Entity` is always present; declared/ancestor types are appended per `add_episode`'s label-construction logic). Reclassification here means: the out-of-vocabulary type is not appended to `labels` as one of its own entries (which would reopen the closed set `strict` is meant to guarantee); instead the `Unclassified` sentinel is appended in its place, alongside the always-present base `Entity` label.
- **Preservation field**: `EntityRow` already carries a free-form `attributes: String` column (`Entity.attributes STRING`, `crates/core/src/schema.rs`), the same pattern `RelatesToEdge.attributes` used for `original_relation_type` in #310. `EntityRow.attributes` exists today but is unconditionally written as the literal `"{}"` for every entity (`crates/core/src/episode.rs`, `make_insert_row`) — this issue is its first real consumer. The original out-of-vocabulary type is preserved there as `{"original_entity_type": "<original>"}`, mirroring the edge-side `{"original_relation_type": "<original>"}` convention exactly. No schema change is required (answers FR-003).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - An entity with an unmodelled type is not destroyed (Priority: P1)

A workspace runs `strict` mode with an ontology that does not declare `Spacecraft`. The extractor returns an entity typed `Spacecraft`. Today that entity is silently dropped by the strict-mode filter. After this change, the entity is retained: it carries the `Unclassified` label instead of `Spacecraft`, and its original type is preserved and recoverable.

**Why this priority**: This is the core defect and the direct entity-side counterpart of #310's User Story 1 — the reclassify-not-drop precedent this service otherwise applies everywhere else is being violated at exactly one remaining point.

**Independent Test**: Run strict-mode extraction (or a full ingest) against an ontology that declares some entity types but not `Spacecraft`, feed it text that yields a `Spacecraft`-typed entity, and confirm the resulting stored entity is present — not absent — carrying `Unclassified` in its labels with the original type recoverable from `attributes`.

**Acceptance Scenarios**:

1. **Given** a `strict`-mode ontology that does not declare `Spacecraft`, **When** an entity is extracted with type `Spacecraft`, **Then** the entity is retained, its labels include `Unclassified` (not `Spacecraft`), and its `attributes` field contains `{"original_entity_type": "Spacecraft"}`.
2. **Given** the same ontology, **When** the extractor returns the same out-of-vocabulary type across multiple entities in one run, **Then** every one of them is retained under `Unclassified` — none are dropped.

---

### User Story 2 - Edges keep their endpoints (Priority: P1)

An edge references an entity whose type is outside the vocabulary. Because the entity now survives (User Story 1), the edge retains a resolvable endpoint instead of losing it. This also removes the current cascade where the strict-mode entity filter's "no entities remain" branch unconditionally clears all edges for the chunk — a chunk can lose entities' *types* to reclassification without its edges being cleared, because the entities themselves are never gone.

**Why this priority**: The whole reason entity deletion is worse than edge deletion (Background) is this cascade. Fixing User Story 1 without confirming this consequence would leave the worse half of the original defect in place.

**Independent Test**: Run strict-mode extraction over text that yields at least one out-of-vocabulary-typed entity and at least one edge referencing it. Confirm the edge is inserted and resolves to the (now-retained, `Unclassified`) entity as its endpoint, and that `edges_dropped_unresolvable` does not count it.

**Acceptance Scenarios**:

1. **Given** a `strict`-mode ontology and an edge whose subject or object entity has an out-of-vocabulary type, **When** extraction completes, **Then** the edge is inserted with both endpoints resolved — the out-of-vocabulary-typed entity is not treated as missing.
2. **Given** a chunk where every extracted entity is out-of-vocabulary, **When** extraction completes, **Then** entities are retained under `Unclassified` (not dropped) and edges referencing them are not cleared solely because of this filter.

---

### User Story 3 - The disposition is observable (Priority: P2)

The number of entities reclassified in a run is reported, in the same way `edges_reclassified_unclassified` now is.

**Why this priority**: Lower priority than User Story 1/2 because it doesn't change what gets persisted — it makes an already-correct disposition visible and auditable, mirroring the observability #310 delivered for edges.

**Independent Test**: Run strict-mode extraction over a corpus containing at least one out-of-vocabulary entity type. Confirm the run's result surfaces a count of reclassified entities (not merely per-entity `eprintln` lines).

**Acceptance Scenarios**:

1. **Given** a run in which N entities hit the out-of-vocabulary reclassification path, **When** the run completes, **Then** the run's result exposes a count of N.
2. **Given** a run with zero out-of-vocabulary entities, **When** the run completes, **Then** the count is 0, distinguishable from "not reported."

---

### Edge Cases

- An entity type that normalizes to a declared type only after `normalize_entity_type` (e.g. case/separator variants) — already handled by the existing normalization-before-vocab-check order, and must keep working unchanged: normalization happens before this issue's reclassify branch, so a normalizable type never reaches it.
- An entity whose type is empty or absent: today, empty type (and the literal type `"Entity"`) already means "no specific type" everywhere else in `add_episode` — no type label beyond the base `Entity` label is appended, in every ontology mode. Under `strict`, this case must **not** be routed through the new `Unclassified` reclassification path (which would incorrectly claim "there was a type and it didn't match"); it must continue to resolve as a plain, untyped `Entity`, consistent with `open`-mode behavior for the same input.
- The dedup path: a reclassified entity (labels include `Unclassified`, original type recoverable from `attributes`) must still dedup correctly against an existing entity of the same subject — including one already stored under its correct declared type, if the same real-world entity was previously extracted with an in-vocabulary type. Reclassification must not create a second, permanently-separate copy of an entity that already exists in the graph under its declared type.
- An ontology with entity types but no relation types, and vice versa: `has_entity_types()` gates the entity-side filter today and must continue to; an ontology declaring no entity types leaves all entities unfiltered (unaffected by this issue), independent of whether relation types are declared.
- A chunk where reclassification affects every entity: previously this emptied `extraction.entities` and cascaded into `extraction.edges.clear()`. After this change that branch should no longer be reachable via type-vocabulary rejection alone, since entities are no longer removed for their type — see User Story 2.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Under `strict` mode, an entity extracted with a type outside the declared vocabulary (after normalization) MUST be retained, not dropped, per the Decision above (option (b), reclassify).
- **FR-002**: A retained out-of-vocabulary entity MUST carry a new closed-vocabulary fallback label, `Unclassified`, in place of the rejected type — not the raw out-of-vocabulary string — so `strict` mode's closed-label-domain guarantee (declared types + `Unclassified`, the entity-side counterpart of `UNCLASSIFIED`) is preserved. `Unclassified` is introduced by this issue; it has no existing precedent to reuse.
- **FR-003**: The entity's original out-of-vocabulary type MUST be preserved and recoverable, in `EntityRow.attributes` (the existing free-form `STRING` column already present on the `Entity` node table — no schema change) as `{"original_entity_type": "<original>"}`, mirroring the edge-side `attributes.original_relation_type` convention from #310.
- **FR-004**: The count of entities reclassified per FR-001–003 MUST be observable as a per-run tally (e.g. `entities_reclassified_unclassified` on `AddEpisodeResult`), mirroring `edges_reclassified_unclassified`, not only as a per-entity debug/`eprintln` line.
- **FR-005**: `strict_mode_entity_type_still_drops_not_reclassifies` (`crates/core/tests/ontology_integration.rs`) MUST be updated or replaced to assert the new reclassify behavior instead of today's drop behavior — it currently locks in the behavior this issue changes.
- **FR-006**: The strict-mode entity filter's current all-entities-empty cascade (unconditionally clearing `extraction.edges` for the chunk) MUST no longer be reachable via type-vocabulary rejection alone, since entities are no longer removed for their type under this change.
- **FR-007**: An entity whose type is empty or absent MUST continue to resolve as a plain, untyped `Entity` (no `Unclassified` label, no reclassification tally increment) under `strict`, consistent with its treatment in `open`/freeform — per the Edge Cases entry above.
- **FR-008**: A reclassified entity MUST remain eligible for the existing dedup path on the same terms as any other entity, including matching against an existing entity of the same subject already stored under a declared type.

### Key Entities *(if the feature involves data)*

- **`Unclassified` entity-type sentinel**: the new closed-vocabulary fallback label assigned, per FR-002, when strict-mode extraction returns an entity type outside the declared vocabulary. The entity-side counterpart of the relation-side `UNCLASSIFIED` sentinel (ADR-0033/ADR-0037/ADR-0310), but a distinct value with PascalCase casing matching entity-type conventions rather than a reuse of the relation-side literal.
- **Entity Out-of-Vocabulary Disposition**: for an entity whose type does not match any declared name after normalization, its labels include `Unclassified` in place of the raw type, and `attributes` carries `{"original_entity_type": "<original>"}`.
- **Entity Out-of-Vocabulary Tally**: a per-run count of entities that hit the out-of-vocabulary disposition path, per FR-004.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Under `strict`, no extracted entity is removed from the pipeline solely because its type is outside the vocabulary.
- **SC-002**: Entity dispositions (the FR-004 tally) are reported as a per-run count, retrievable from the same result structure `edges_reclassified_unclassified` is retrievable from.
- **SC-003**: No change to `open` or `freeform` behavior, and no change to the rendered entity-type prompts in those modes — the `open` edge/entity prompts stay byte-identical to today, per the same constraint #310 required, so existing cassettes stay valid.
- **SC-004**: No regression in `edges_dropped_unresolvable`: with entities surviving that previously caused endpoint loss, this count must fall or hold for any given corpus, never rise.
- **SC-005**: For every entity reclassified under FR-001–003, the original entity type is recoverable from `attributes.original_entity_type` — no information is destroyed by the reclassification.
- **SC-006**: A reclassified entity still dedups correctly against a pre-existing entity of the same subject stored under a declared type (FR-008) — verified by at least one test covering this path.

## Assumptions

- Entity-type conformance is high (97.5–99.9% per the #310 investigation), so the blast radius is small; this is a correctness and consistency fix, not a recall win.
- ADR-0310's reasoning is the starting point but not binding; the Decision section above argues the entity case independently rather than assuming symmetry, and concludes the same outcome (reclassify, not drop) for reasons specific to entities (edge-endpoint cascade) as well as the shared schema-closure argument.
- Quantifying how many edges today lose an endpoint specifically because their entity was dropped by this filter (the original issue's FR-006) is empirical measurement work — analogous to the corpus measurement ADR-0310 performed for edges — and is left to the Research stage rather than decided here; this spec's SC-004 makes the required direction of change (fewer or equal drops, never more) a testable regression gate regardless of the exact baseline count.
- The formal ADR (`docs/adr/0312-<slug>.md`) is written during the Plan/Implement stage per this repository's issue-numbered-ADR convention (`CLAUDE.md`); this spec records the decision and reasoning it must capture, not the ADR document itself.
- `Entity.attributes` is currently always written as the literal `"{}"` for every entity (`crates/core/src/episode.rs`); this issue is its first real consumer, so there is no existing content to preserve or merge with when writing `original_entity_type`.

## Out of Scope

- Any change to `knowledge_canonicalize_relations` or any equivalent entity-side "canonicalize" pass — none exists today, and creating one is not part of this issue.
- Retroactively reclassifying entities already dropped in production workspaces before this fix ships (no backfill/migration is included).
- Changing dedup's matching *algorithm* — FR-008/SC-006 require that reclassified entities remain eligible for the existing dedup path, not that dedup behavior itself changes.
- Any change to `open` or `freeform` mode entity handling, or to entity-type prompt content in those modes (see SC-003).
- Introducing `aliases`/`keywords` fields to `EntityTypeDef` — ADR-0310 confirmed no such fields exist and none are needed for this issue; the defect here is delete-vs-reclassify, not alias-blindness.

## Source References

- `crates/core/src/episode.rs` — strict-mode entity filter in `add_episode` (~line 201) and the edge-clearing cascade in its "no entities remain" branch; `make_insert_row`'s `attributes: "{}".to_string()` (the field FR-003 reuses); `AddEpisodeResult` (the struct FR-004 extends).
- `crates/core/src/ontology.rs` — `EntityTypeDef`, `normalize_entity_type`, `has_entity_types`, `entity_type_names`.
- `crates/core/src/schema.rs` — `Entity` node table's existing `attributes STRING` column.
- `crates/core/src/types.rs` — `EntityRow`.
- `crates/core/tests/ontology_integration.rs` — `strict_mode_entity_type_still_drops_not_reclassifies` (FR-005).
- `docs/adr/0310-strict-mode-reclassifies-not-drops.md` — the edge-side precedent this issue mirrors and independently re-argues for entities.
- `docs/adr/0033-noise-edges-reclassified-not-deleted.md`, `docs/adr/0037-relation-classification-abstention-writes-unclassified.md` — the reclassify-not-delete precedent line this issue is the last remaining installment of.
- Issue #310 / PR #311 — the edge-side fix this issue completes the symmetry for.
- Issue #306 — the other silent-data-loss defect fixed this cycle (truncation recorded as clean), cited in Background as the same failure shape.
