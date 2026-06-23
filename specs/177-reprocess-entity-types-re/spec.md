# Feature Specification: Extend `reprocess_entity_types` — Off-Ontology and Full-Graph Re-typing with Additive Labels

**Feature Branch**: `fabrik/issue-177`
**Created**: 2026-06-23
**Status**: Draft
**Input**: User description: "Extend `knowledge_reprocess_entity_types` to re-type entities beyond the untyped/generic-only set — so it can fix mis-typed entities and rationalize the whole graph to the configured ontology. Add a `scope` parameter; apply labels additively per the subtype model (#173)."

## Background

`knowledge_reprocess_entity_types` (established in #30) currently only reclassifies entities whose sole label is the generic `Entity` — entities that were never assigned a specific type. It does not touch entities that already carry a specific type label, even if that label is wrong, noisy, or not part of the configured ontology.

Two gaps make this a practical problem for users:

1. **Mis-typed nodes cannot be fixed.** Entities ingested before the ontology was configured, or before the extractor was tuned, may carry types like `Process`, `Council`, or `Agreement` that reflect LLM hallucination rather than semantic intent. A person mislabeled `Process` stays `Process` forever; the current button cannot help.

2. **Over-granular free-form type tails cannot be rationalized.** In a real-world graph with 367 distinct entity types but only ~14 untyped entities, the current "Reclassify" button is nearly a no-op. The other 353+ typed entities accumulate over time, polluting the label space with one-off LLM coinages (`Technologist`, `CouncilMember`, `PartnershipAgreement`) that map to well-known ontology types but will never be re-examined under today's logic.

The downstream consumer of the graph is an LLM doing semantic and exploratory retrieval. Correct, ontology-aligned types are a first-class quality signal — not cosmetic. With #173 delivering additive ancestor labeling (e.g., `RFC` → `[Entity, Document, RFC]`), there is now a path to a clean, hierarchy-aware graph if reclassification can reach beyond the untyped tail.

This feature adds a `scope` parameter so operators can choose how broadly to apply reclassification: just the untyped tail (current behavior), entities whose type is absent from the declared ontology, or every entity in the graph.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Fix Mis-Typed Entities With `scope=off_ontology` (Priority: P1)

A user has an ontology configured with a set of declared entity types (e.g., `Person`, `Organization`, `Document`). Over months of ingestion, some entities picked up free-form LLM-coined types that don't appear in the ontology (`Process`, `Council`, `Technologist`, `PartnershipAgreement`). The user triggers `knowledge_reprocess_entity_types` with `scope=off_ontology`. The service identifies all entities whose current specific type is not in the declared ontology, runs LLM classification constrained to the ontology's declared types, and updates only those nodes — replacing the wrong label with the correct ontology type (plus its ancestors per #173).

**Why this priority**: This is the primary use case. The "Reclassify" button in liminis-app becomes genuinely useful on real-world graphs only when it can reach beyond the ~14 untyped entities to the hundreds of off-ontology-typed ones. Fixing `Process → Person` is exactly the correctness signal users need.

**Independent Test**: Seed a graph with: (a) 5 entities labelled only `Entity`; (b) 5 entities with a type not in the ontology (e.g., `Council`); (c) 3 entities with a correct ontology type (e.g., `Person`). Configure an ontology with `Person`, `Organization`, `Document`. Call `knowledge_reprocess_entity_types {scope: "off_ontology"}`. Assert: the 5 off-ontology-typed entities are reclassified; the 5 untyped entities and 3 correctly-typed entities are NOT touched. Response reports counts correctly.

**Acceptance Scenarios**:

1. **Given** an entity with label `['Entity', 'Council']` and an ontology that declares `Person` but not `Council`, **When** `knowledge_reprocess_entity_types {scope: "off_ontology"}` runs, **Then** the entity is reclassified; if the LLM assigns `Person`, the node's labels become `['Entity', 'Person']` (or with ancestors if declared).
2. **Given** an entity with label `['Entity', 'Person']` and an ontology that declares `Person`, **When** `scope=off_ontology` runs, **Then** the entity is NOT re-classified (its current type is in the ontology) and its labels are unchanged.
3. **Given** an entity with label `['Entity']` only (untyped), **When** `scope=off_ontology` runs, **Then** that entity IS re-typed (it has no specific type, so it is treated as off-ontology for this scope — same as `scope=untyped`).
4. **Given** an LLM that returns a low-confidence assignment for an off-ontology entity, **When** `scope=off_ontology` runs, **Then** that entity is left unchanged; the response counts it neither as reclassified nor as an error.
5. **Given** `dry_run: true` with `scope=off_ontology`, **When** called, **Then** the response lists the planned `{entity_id, entity_name, old_type, new_type}` per entity and mutates nothing; entity labels in the graph are byte-identical before and after the call.

---

### User Story 2 — Preserve Today's Behavior With `scope=untyped` (Priority: P1)

A user who has been relying on the existing reclassification flow calls `knowledge_reprocess_entity_types` with no `scope` argument (or `scope=untyped`). The behavior is identical to what existed before this feature: only entities with no specific type are reclassified.

**Why this priority**: Back-compat is load-bearing. liminis-app calls this method from two sites; neither currently passes `scope`. Adding a non-breaking default must not alter existing behavior.

**Independent Test**: Run `knowledge_reprocess_entity_types` with no params on a graph with mixed entity types. Assert the result is identical to what the pre-#177 handler would have produced: only untyped entities are reclassified.

**Acceptance Scenarios**:

1. **Given** a call to `knowledge_reprocess_entity_types` with no `scope` field, **When** it runs, **Then** behavior is identical to `scope=untyped`: only entities with no specific label are reclassified.
2. **Given** `scope=untyped` explicitly, **When** it runs, **Then** entities with any specific type label (even off-ontology ones) are NOT touched.
3. **Given** all entities already have specific type labels, **When** `scope=untyped` runs, **Then** `{success: true, reclassified_count: 0}`.

---

### User Story 3 — Full Graph Re-typing With `scope=all` (Priority: P2)

A user wants to rationalize the entire graph after a major ontology overhaul. They call `knowledge_reprocess_entity_types {scope: "all"}`. Every entity — typed or untyped, ontology or off-ontology — is fed to the LLM classifier constrained to the current ontology. Entities already correctly classified are re-confirmed (LLM re-confirms, no label change if type matches). Entities with wrong or noisy types are corrected.

**Why this priority**: P2 because this is expensive (every entity is classified), appropriate only for large ontology overhauls, and less common than `off_ontology`. The user explicitly opts into the cost.

**Independent Test**: Seed a graph with entities carrying both ontology-aligned and off-ontology types. Run `scope=all`. Assert all entities are fed to the LLM; those already correctly typed remain correct; those incorrectly typed are corrected.

**Acceptance Scenarios**:

1. **Given** an entity with `['Entity', 'Person']` and an ontology that declares `Person`, **When** `scope=all` runs and the LLM re-confirms `Person`, **Then** the entity's labels are unchanged (idempotent — no churn if the assignment matches).
2. **Given** an entity with `['Entity', 'Council']` and no `Council` in the ontology, **When** `scope=all` runs, **Then** the entity is reclassified (same as `off_ontology` behavior for this entity).
3. **Given** `scope=all` and no ontology configured, **When** called, **Then** the service returns a structured error indicating an ontology is required for `scope=all`; no entities are modified.

---

### User Story 4 — Dry Run Preview Before Applying (Priority: P2)

A user wants to see what would change before committing. They call `knowledge_reprocess_entity_types {scope: "off_ontology", dry_run: true}`. The service runs LLM classification and returns a full plan — old type → new type per entity, plus aggregate counts — but writes nothing to the graph.

**Why this priority**: P2 because dry_run is a quality-of-life feature for careful operators. The primary value is correctness assurance before a potentially large mutation.

**Acceptance Scenarios**:

1. **Given** `dry_run: true` with any scope, **When** called, **Then** the graph state is identical before and after the call (no label mutations, no WAL entries written).
2. **Given** `dry_run: true` with `scope=off_ontology`, **When** the response is returned, **Then** it includes a `plan` array of `{entity_id, entity_name, old_type, new_type}` entries for all entities that would be reclassified, plus `would_reclassify_count`.
3. **Given** no entities would change under the given scope, **When** `dry_run: true` is called, **Then** `plan: []` and `would_reclassify_count: 0`.

---

### Edge Cases

- **`scope=off_ontology` with no ontology configured**: No declared entity types exist → every specifically-typed entity is off-ontology. Since LLM classification must be constrained to ontology types and there are none, the service returns a structured error: `{success: false, error: "scope=off_ontology requires a configured ontology"}`.
- **`scope=all` with no ontology configured**: Same as above — LLM cannot be constrained to declared types. Return structured error. (`scope=untyped` works without an ontology, as it did before this feature.)
- **Entity whose LLM-assigned re-type matches its current type (idempotent).** Labels are unchanged, the entity is NOT counted in `reclassified_count`. No WAL entry is written for no-ops.
- **Entity with ancestor labels already stamped by #173 (e.g., `['Entity', 'Document', 'RFC']`).** If `RFC` is in the ontology and `scope=off_ontology`, the entity is skipped entirely (its leaf type is in the ontology). If `scope=all`, it is re-classified; if the LLM re-confirms `RFC`, labels are unchanged.
- **Re-type produces a type with declared ancestors (per #173).** Labels are stamped additively: if `RFC { parent: Document }` and re-type is `RFC`, resulting labels are `['Entity', 'Document', 'RFC']` — NOT just `['Entity', 'RFC']`.
- **Re-type replaces a wrong specific type.** Old specific label is removed; new specific label + ancestors are added. `name`, `summary`, edges, and `fact` values are never touched. Only the `labels` array changes.
- **`scope=all` on a very large graph.** Processing is batched (reusing existing `classify_entities` batch logic). The batch size is configurable or a documented constant. OOM must not occur.
- **LLM unavailable during reprocessing.** Returns structured error `{success: false, error: "..."}`. Any entity already reclassified before the failure keeps its new labels (WAL-durable); remaining entities are unchanged. The partial progress is visible in the WAL.
- **`scope=off_ontology` and entity has minted type (assigned by open-mode LLM, not in ontology).** Such entities are treated identically to off-ontology entities — they qualify for reclassification under `scope=off_ontology`.
- **Concurrent call with active ingestion.** Method acquires the writer lock, same as today; concurrent `add_episode` calls queue behind it.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_reprocess_entity_types` MUST accept an optional `scope` field. Valid values: `"untyped"` (default), `"off_ontology"`, `"all"`. An unrecognized value MUST return a structured error.
- **FR-002**: `scope=untyped` MUST preserve existing behavior exactly: only entities whose sole label is the generic `Entity` (no specific type) are reclassified. All existing callers that omit `scope` MUST receive identical results to the pre-#177 behavior.
- **FR-003**: `scope=off_ontology` MUST classify entities whose current specific type is NOT a declared `entity_type` in the loaded ontology, including both truly untyped entities (`['Entity']` only) and entities with a type not present in the ontology.
- **FR-004**: `scope=off_ontology` and `scope=all` MUST return a structured error `{success: false, error: "..."}` when no ontology is configured, rather than treating all entities as off-ontology or performing unconstrained classification.
- **FR-005**: `scope=all` MUST run LLM classification on every entity in the named `group_id`, regardless of current type.
- **FR-006**: LLM classification for `scope=off_ontology` and `scope=all` MUST be constrained to the ontology's declared `entity_types` (same constraint already implied by `classify_entities` in ontology-mode — no free-form types allowed in these scopes).
- **FR-007**: When a re-type changes an entity's specific label, the old specific type label MUST be removed and the new specific type label (plus all its declared ancestor labels per #173) MUST be added. The `Entity` base label MUST always remain.
- **FR-008**: `name`, `summary`, edges, and `fact` values on reclassified entities MUST NOT be modified. Only the `labels` array changes.
- **FR-009**: If the LLM assigns the same type as the entity's current specific type, the entity's labels MUST NOT be modified (no-op). No WAL entry is written for no-ops. The entity is NOT counted in `reclassified_count`.
- **FR-010**: If the LLM returns low-confidence or no assignment for an entity, the entity MUST be left unchanged. Low-confidence is defined as the LLM returning no type or a type outside the constrained list. The entity is NOT counted in `reclassified_count` or in `errors`.
- **FR-011**: `knowledge_reprocess_entity_types` MUST accept an optional `dry_run` field (bool, default `false`). When `dry_run: true`, no mutations are written to the graph and no WAL entries are created.
- **FR-012**: The `dry_run: true` response MUST include a `plan` array of objects `{entity_id: string, entity_name: string, old_type: string | null, new_type: string}` listing each entity that would be reclassified, plus `would_reclassify_count: int`.
- **FR-013**: The non-dry-run success response MUST include at minimum `{success: true, reclassified_count: int}`. The response MAY also include `unchanged_count` (entities scoped but not changed due to idempotency or low confidence) for observability.
- **FR-014**: Processing MUST be batched to avoid OOM on large graphs. The batch size MUST be the same as or configurable alongside the existing `classify_entities` batch logic.
- **FR-015**: All reclassification mutations MUST be WAL-durable: each label change is written to the WAL before being applied to the graph, consistent with how other write operations are handled.
- **FR-016**: Re-running `knowledge_reprocess_entity_types` with the same scope on an already-correctly-typed graph MUST be idempotent: `reclassified_count: 0`, no WAL entries written, no label changes.
- **FR-017**: The `group_id` parameter (string, optional, default `"liminis"`) MUST be preserved from the existing method signature. All scopes filter by `group_id`.
- **FR-018**: The method MUST acquire the writer lock (same as today), ensuring concurrent `add_episode` or `apply_corrections` calls are serialized.

### Key Entities

- **`scope`** (new parameter): Controls which entities are candidates for reclassification. `"untyped"` — only entities with no specific type; `"off_ontology"` — entities with a specific type not declared in the loaded ontology (plus untyped); `"all"` — every entity in the group.
- **`dry_run`** (new parameter): When true, runs classification but writes nothing; returns a plan for review.
- **`plan`** (dry_run response field): Array of `{entity_id, entity_name, old_type, new_type}` describing what would change.
- **Entity label set**: The `labels STRING[]` array on each entity node; the only field mutated by this operation. Ancestor labels per #173 are applied when the new type has declared parents.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Given a graph with 10 entities typed `Council` (not in ontology declaring `Person`, `Organization`) and `scope=off_ontology`, after calling `knowledge_reprocess_entity_types`, those 10 entities carry ontology-aligned types (e.g., `Person`) with correct ancestor labels; entities already typed correctly are unchanged.
- **SC-002**: Given a call with no `scope` param (or `scope=untyped`), the response is byte-identical to what the pre-#177 handler would have returned — all existing tests pass unchanged.
- **SC-003**: Given `scope=off_ontology` with no ontology configured, the response is `{success: false, error: "..."}` and no entities are modified.
- **SC-004**: Given `scope=off_ontology, dry_run: true`, the response includes a `plan` array listing old and new types; after the call, a graph query confirms all labels are unchanged.
- **SC-005**: Given an entity with `['Entity', 'Process']` (off-ontology) that the LLM reclassifies as `Person` (ontology-declared with `parent: Human`), after the operation the entity's labels are `['Entity', 'Human', 'Person']` — old specific label removed, new type + ancestors added.
- **SC-006**: Running `scope=off_ontology` twice on a graph where the first run already corrected all off-ontology entities produces `reclassified_count: 0` on the second run (idempotent).
- **SC-007**: Given a batch of 500 off-ontology entities, processing completes without OOM and `reclassified_count` equals the number successfully reclassified; partial progress is WAL-durable if the LLM fails mid-batch.
- **SC-008**: All new unit and integration tests pass under `cargo test` and `cargo clippy --release --all-targets -- -D warnings`.

## Assumptions

- **A1.** `scope=untyped` default ensures zero behavioral change for all existing callers that do not pass `scope`. liminis-app's two call sites continue to work without modification.
- **A2.** Ancestor labeling during re-type follows #173's `apply_entity_type_labels` behavior exactly: if the ontology declares `RFC { parent: Document }` and the new type is `RFC`, the resulting labels include `Document` and `RFC` (plus `Entity`).
- **A3.** "Low-confidence" is defined operationally as: the LLM returns no type, or a type not in the constrained list. The classifier (`classify_entities`) is expected to enforce the constraint; any non-matching response is treated as no assignment.
- **A4.** Only the `labels` array on entity nodes is modified. Node properties (`name`, `summary`, `fact`, `group_id`, etc.) and all incident edges are untouched.
- **A5.** WAL durability means label-change mutations follow the same WAL write path as other mutations (e.g., from `add_episode`). Dry-run mode writes no WAL entries.
- **A6.** `group_id` scoping is inherited from the existing method. All three scope values filter by `group_id` before identifying candidates.
- **A7.** `scope=off_ontology` treats untyped entities (just `Entity` label) as qualifying, so the scope is a strict superset of `scope=untyped`. There is no case where `off_ontology` would skip an entity that `untyped` would reclassify.
- **A8.** The classification batch size used by `classify_entities` is shared or reused. No new batch-size configuration is introduced in this feature.

## Out of Scope

- Reclassifying relation types (edge `relation_type` labels) — covered by the separate #175 analog.
- Automatic re-typing triggered by ontology changes (drift detection per #98 notifies the user; they run reprocess manually or via Recreate).
- Ancestor label backfill for entities already correctly typed but missing ancestor labels due to #173 being newly deployed — that case is handled by #173's FR-011 separately.
- New UI changes in liminis-app (the existing "Reclassify" button can pass `scope`; wire-up is a liminis-app concern).
- LLM cost tracking or per-call cost reporting.
- `scope=off_ontology` with configurable "ontology match" rules beyond exact type-name membership.
- Multi-group reprocessing in a single call (call per group if needed).

## Source References

- `crates/core/src/handlers.rs` — `handle_reprocess_entity_types`: add `scope` and `dry_run` params; dispatch to new helper.
- `crates/core/src/corrections.rs` — `list_all_generic_entities` (candidates for `scope=untyped`); `apply_entity_type_labels` (label-stamping site; must apply ancestor chain per #173).
- `crates/core/src/ontology.rs` — `Ontology::entity_type_names()` or equivalent for off-ontology membership test; ancestor map from #173.
- `extractor.classify_entities` — LLM classification call; must be constrained to `entity_types` from the loaded ontology for `off_ontology` and `all` scopes.
- Issue #30 — established `reprocess_entity_types` and `apply_entity_type_labels`; this feature extends both.
- Issue #173 — established additive ancestor labeling; `apply_entity_type_labels` is the integration point.
- Issue #175 — relation `relation_type` backfill (entity-side analog; same scope/dry_run pattern).
- Issue #83 — established optional ontology support.
- Issue #98 — established drift detection; drift notification prompts user to run this command.
