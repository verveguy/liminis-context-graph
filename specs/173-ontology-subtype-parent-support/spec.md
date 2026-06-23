# Feature Specification: Ontology Subtype/Parent Support — Hierarchical Entity Types via Additive Multi-Labeling

**Feature Branch**: `fabrik/issue-173`
**Created**: 2026-06-23
**Status**: Draft
**Input**: User description: "Add hierarchical entity types to the workspace ontology via a `parent` field, and have entity typing stamp each node with its specific type plus all ancestor types (additive multi-labeling)."

## Background

The workspace ontology (#83) is currently flat: each `EntityTypeDef` has a `name` and optional `description`, but no hierarchical relationship between types. Every entity node is stamped with exactly two labels: `Entity` (the lbug/Kuzu base node table label, always present) and the specific type assigned by the LLM extractor — e.g., `[Entity, RFC]`. There is no way to express that `RFC` is a subtype of `Document`, so a query for all `Document`-typed nodes misses `RFC`, `ADR`, `Specification`, and any other document-family subtypes.

The primary consumer of the context graph is an LLM doing semantic and exploratory retrieval ("explore this node", "find semantically similar nodes"), not structural type-queries. Rich, specific type labels (e.g., `RFC` instead of `Document`) preserve unrecoverable semantic distinction that coarse types lose. **Flattening specific kinds into a coarse parent (e.g., `RFC`/`ADR`/`Specification`/`Presentation` → `Document`) is the wrong direction.** The correct model is *additive structure layered over richness*: keep the specific type, **add** the supertype. This is the standard property-graph idiom for subtyping — no RDF/OWL/triplestore required.

This feature adds:
1. An optional `parent: <TypeName>` field to the ontology YAML for entity types, forming a tree structure (single parent per type; multi-parent DAGs are out of scope for v1).
2. Load-time validation of the parent tree: undeclared parents and cycles are handled gracefully (warn + degrade, no crash).
3. A label-stamping rule: each node gets its specific type **plus all ancestor types transitively**, in addition to the always-present `Entity` base label.
4. Content-hash coverage of parent edges so ontology drift detection (#98) captures hierarchy changes.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — A Node with a Declared Subtype Carries All Ancestor Labels (Priority: P1)

A user declares a hierarchy in `ontology.yaml` (e.g., `RFC` with `parent: Document`). When the graph extracts or re-types an `RFC` entity, the node's `labels` list contains `RFC`, `Document`, and `Entity` — enabling both specific queries (`WHERE 'RFC' IN e.labels`) and rollup queries (`WHERE 'Document' IN e.labels`).

**Why this priority**: This is the feature's entire value. Without it, hierarchy in the YAML has no effect on the graph.

**Independent Test**: Declare `RFC { parent: Document }` and `ADR { parent: Document }` in the ontology. Ingest a chunk mentioning an RFC and an ADR. Assert: `MATCH (e:Entity) WHERE 'RFC' IN e.labels` returns only the RFC node; `MATCH (e:Entity) WHERE 'Document' IN e.labels` returns both nodes.

**Acceptance Scenarios**:

1. **Given** an ontology with `RFC { parent: Document }`, **When** an `RFC` entity is extracted or reprocessed, **Then** the node's `labels` contains at minimum `['Entity', 'Document', 'RFC']`.
2. **Given** an ontology with `ADR { parent: Document }`, **When** an `ADR` entity is extracted, **Then** `MATCH (e:Entity) WHERE 'Document' IN e.labels` returns the ADR node.
3. **Given** an ontology with `RFC { parent: Document }`, **When** `MATCH (e:Entity) WHERE 'RFC' IN e.labels` runs, **Then** it returns RFC entities but NOT entities whose specific type is just `Document` (specificity is preserved).
4. **Given** an ontology with a 3-level chain `SubDoc { parent: RFC }`, `RFC { parent: Document }`, **When** a `SubDoc` entity is extracted, **Then** its labels contain all four: `['Entity', 'Document', 'RFC', 'SubDoc']`.

---

### User Story 2 — Existing Flat Ontologies Are Unaffected (Priority: P1)

A user with an existing ontology that declares no `parent` fields sees zero behavior change. All nodes continue to carry `[Entity, <SpecificType>]` labels, exactly as before. No data migration required.

**Why this priority**: Non-regression is load-bearing. Any breakage of existing workspaces is a blocking defect.

**Independent Test**: Load an ontology with three entity types and no `parent` fields. Run `reprocess_entity_types` on a seeded graph. Verify each node has exactly the same two-element label set it would have had before this change: `[Entity, <SpecificType>]`.

**Acceptance Scenarios**:

1. **Given** an ontology YAML with no `parent` fields, **When** the service starts and entities are extracted, **Then** labels are `[Entity, <SpecificType>]` — exactly as today. No extra labels, no missing labels.
2. **Given** a workspace with an ontology-hash sidecar written before this change, **When** the new service version starts with an unchanged flat ontology YAML, **Then** no drift is detected (`drifted: false`).

---

### User Story 3 — Invalid `parent` Declarations Are Handled Gracefully (Priority: P1)

An ontology that declares a `parent` referencing an undeclared type, or one that creates a cycle, is handled without crashing. The service logs a clear warning and degrades gracefully: the offending type is treated as having no parent.

**Why this priority**: A typo in the YAML must not take down the service.

**Acceptance Scenarios**:

1. **Given** an ontology where `RFC { parent: NonExistentType }`, **When** the service starts, **Then** it logs a warning naming the type and the undeclared parent; loads `RFC` without a parent; and the service starts normally.
2. **Given** an ontology with a 2-node cycle (`A { parent: B }`, `B { parent: A }`), **When** the service starts, **Then** a warning is logged identifying the cycle; both types are loaded with no parent; the service starts normally and does not crash.
3. **Given** an ontology with a 3-node cycle (`A → B → C → A`), **When** the service starts, **Then** all three types are loaded with their `parent` cleared; a single log line identifies the cycle.

---

### User Story 4 — Minted Types Have No Ancestor Labels (Priority: P2)

In `open` mode, the LLM may assign an entity type that is not declared in the ontology (a "minted" type). A minted type has no parent declared, so it gets only `[Entity, <MintedType>]` — no ancestors are inferred.

**Why this priority**: P2 because open-mode minted types are a corner case. Minted types still work correctly; they just don't gain hierarchy labels until declared.

**Acceptance Scenarios**:

1. **Given** an open-mode ontology where `RFC { parent: Document }` is declared but `Whitepaper` is not, **When** the LLM assigns `Whitepaper` to an entity, **Then** the node's labels are `['Entity', 'Whitepaper']` — no `Document` ancestor, because `Whitepaper` has no declared parent.
2. **Given** a user later adds `Whitepaper { parent: Document }` to the ontology and runs `reprocess_entity_types`, **When** the reclassification runs, **Then** `Whitepaper` nodes gain `Document` in their labels.

---

### User Story 5 — Ontology Hierarchy Changes Are Drift-Detected (Priority: P2)

Adding, removing, or changing a `parent` relationship changes the ontology's content hash, which triggers drift detection per #98. This notifies the user that `reprocess_entity_types` or a Recreate is needed to propagate the new hierarchy to existing nodes.

**Why this priority**: P2 because drift detection is existing infrastructure; this extends it. Missing it would leave users unaware that their graph's labels are incomplete after a hierarchy edit.

**Acceptance Scenarios**:

1. **Given** an ontology where `RFC` has no parent, **When** the user adds `parent: Document` to `RFC` and restarts, **Then** `knowledge_status` reports `ontology.drifted: true`.
2. **Given** an ontology where `RFC { parent: Document }` exists and nothing changes, **When** the service restarts, **Then** `drifted: false`.
3. **Given** drift is detected after a hierarchy change, **When** the user runs `reprocess_entity_types`, **Then** existing `RFC` nodes (previously stamped `[Entity, RFC]`) gain `Document` in their labels, and drift clears after the sidecar is updated.

---

### Edge Cases

- **Deep hierarchy (10+ levels).** Ancestor traversal terminates at the root (no parent). No depth limit is imposed; deep hierarchies are unusual but legal.
- **Two types with the same parent.** Allowed — forms a tree fan-out, not a cycle. Both subtypes independently gain the shared ancestor label.
- **`reprocess_entity_types` called after hierarchy is declared on an existing graph.** Nodes already stamped with `[Entity, RFC]` (before hierarchy was declared) must also be re-stamped to add ancestor labels. The reprocessor must handle two cases: (a) entities with only `Entity` label → reclassify via LLM; (b) entities with a specific type but missing ancestor labels → re-stamp without LLM call.
- **`reprocess_entity_types` called on a graph with no ontology loaded.** No ancestor labels to add; behavior identical to today.
- **Parent declared for a relation type.** Out of scope for v1; any such field in the YAML is ignored silently.
- **`Entity` declared as an explicit type in `entity_types`.** `Entity` is the implicit root and the lbug node table name. Declaring it explicitly is a no-op (it is always in the label set regardless). Log a warning at load time.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The `entity_types` section of `ontology.yaml` MUST support an optional `parent: <TypeName>` field on any entry. A missing or null `parent` means the type has no declared supertype beyond the implicit `Entity` root.
- **FR-002**: At load time, the ontology loader MUST validate all declared `parent` values against the set of declared type names. Any `parent` that references a name not present in the declared set MUST cause a log warning (identifying the type and the undeclared parent) and MUST be treated as absent. The type itself MUST still be loaded without a parent.
- **FR-003**: At load time, the ontology loader MUST detect cycles in the declared parent graph (e.g., A→B→A, A→B→C→A). On cycle detection: log a warning identifying the cycle, clear the `parent` field for all types involved in the cycle, and continue loading. The service MUST NOT crash or refuse to start due to a cycle.
- **FR-004**: The `content_hash` function in `ontology.rs` MUST include parent relationships in its canonical serialization. Adding, removing, or changing a `parent` field on any entity type MUST change the hash output. (This ensures #98's drift detection covers hierarchy changes with no sidecar schema change needed.)
- **FR-005**: The `Ontology` runtime struct MUST expose an ancestor-lookup capability (e.g., a `HashMap<String, Vec<String>>` precomputed at load time) that maps a type name to its ordered list of ancestor type names (supertype-to-subtype order, excluding the implicit `Entity` root). Types with no parent map to an empty vec. Undeclared type names return an empty vec. This lookup MUST be O(1) per type.
- **FR-006**: The label-stamping logic — applied both during initial extraction and during `reprocess_entity_types` — MUST compute the full ancestor chain for the assigned entity type and include all ancestor type names in the node's `labels` array. The `Entity` base label MUST always be present. The specific assigned type MUST always be present.
- **FR-007**: The ancestor chain MUST be computed transitively. For a 3-level hierarchy `C ⊑ B ⊑ A`, an entity typed `C` MUST receive labels that include all of `Entity`, `A`, `B`, and `C`.
- **FR-008**: Label stamping MUST be additive: the specific type MUST NOT be replaced by or lost to an ancestor. Even if the ancestor is broader, the specific type is always preserved.
- **FR-009**: Entities with a minted type (LLM-assigned type not declared in the ontology) MUST receive only `['Entity', <MintedType>]` — no ancestor labels. No warning is emitted for minted types; this is expected in open mode.
- **FR-010**: Flat ontologies (all entity types without `parent` fields) MUST produce label sets identical to the current behavior: `['Entity', <SpecificType>]` per entity. All existing tests MUST pass unchanged.
- **FR-011**: `reprocess_entity_types` MUST handle two cases when hierarchy is in effect: (a) entities with only the `Entity` label → reclassify via LLM (current behavior), then stamp with full ancestor chain; (b) entities with a specific type but missing ancestor labels in their current label set → re-stamp with full ancestor chain, NO LLM call required.
- **FR-012**: New unit tests MUST cover: 2-level ancestor chain, 3-level ancestor chain, flat type unchanged, undeclared parent graceful handling, cycle detection and graceful handling, content hash changes on parent add/modify/remove.

### Key Entities

- **`EntityTypeRaw`** (YAML deserialization struct in `ontology.rs`): gains `parent: Option<String>` field, deserialized from the YAML `parent:` key.
- **`EntityTypeDef`** (runtime struct in `ontology.rs`): gains `parent: Option<String>` field holding the validated (and potentially cleared) parent name after load-time checks.
- **`Ontology`** (runtime struct in `ontology.rs`): gains an ancestor map precomputed at load time — `HashMap<String, Vec<String>>` from type name to ordered ancestor list (not including `Entity`).
- **Parent forest**: the parent relationships form a forest (multiple disjoint trees) rooted at the implicit `Entity` root. The forest is validated for cycles at load time.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Given an ontology with `RFC { parent: Document }` and `ADR { parent: Document }`, after extraction or reprocessing, `MATCH (e:Entity) WHERE 'Document' IN e.labels` returns all RFC and ADR nodes; `MATCH (e:Entity) WHERE 'RFC' IN e.labels` returns only RFC nodes.
- **SC-002**: A 3-level hierarchy `SubDoc { parent: RFC }`, `RFC { parent: Document }` produces labels containing all four values — `Entity`, `Document`, `RFC`, `SubDoc` — on a `SubDoc` entity.
- **SC-003**: A flat ontology (no `parent` fields) produces `labels == ['Entity', <SpecificType>]` for every entity — identical to pre-#173 behavior. All existing tests pass unchanged.
- **SC-004**: An ontology with `RFC { parent: UndeclaredType }` loads without crashing; the RFC type loads with no parent; a log warning is emitted naming the undeclared type.
- **SC-005**: An ontology with a cycle `A { parent: B }`, `B { parent: A }` loads without crashing; both types load with no parent; a log warning is emitted identifying the cycle.
- **SC-006**: Adding `parent: Document` to `RFC` in an otherwise unchanged ontology changes the `content_hash` return value; consequently `compute_drift` reports `drifted: true` after restart.
- **SC-007**: Running `reprocess_entity_types` on a graph containing `RFC` nodes stamped `[Entity, RFC]` (pre-hierarchy) after adding `RFC { parent: Document }` results in those nodes carrying `Document` in their labels.
- **SC-008**: All new unit tests and all existing tests pass under `cargo test` and `cargo clippy --release --all-targets -- -D warnings`.

## Assumptions

- **A1.** Single-parent (tree) hierarchy is sufficient for v1. Multi-parent DAGs are an explicit non-goal.
- **A2.** Parent relationships apply only to entity types, not relation types, in v1.
- **A3.** The order of labels in the `labels STRING[]` array is not significant for querying (lbug/Kuzu's `IN` operator is order-independent). The implementation may choose any consistent ordering.
- **A4.** `Entity` is always the implicit top-level ancestor and is always present in the label set, regardless of hierarchy. It does not need to be declared in `entity_types` in the YAML.
- **A5.** The existing drift detection infrastructure (#98) requires no sidecar schema changes. The content hash captures parent relationships through FR-004; the hash stored in the sidecar will change when parents change, triggering correct drift detection.
- **A6.** The ancestor map precomputed at load time (FR-005) is the right tradeoff: it is computed once and makes label-stamping O(1) per entity, which matters during bulk reprocessing of large graphs.

## Out of Scope

- Multi-parent (DAG) hierarchy for entity types (v2).
- Parent relationships for relation types.
- Automatic re-stamping of all nodes when the hierarchy changes (the user runs `reprocess_entity_types` or Recreate; drift detection per #98 notifies them).
- A UI for visualizing the entity type hierarchy (renderer-side feature).
- Inference of subtype relationships from descriptions (LLM-assisted ontology structure inference).
- SPARQL/OWL compatibility; this is a pure property-graph implementation.

## Source References

- `crates/core/src/ontology.rs` — `EntityTypeRaw`, `EntityTypeDef`, `Ontology`, `load_ontology`, `content_hash`; all need modification.
- `crates/core/src/corrections.rs` — `apply_entity_type_labels` (~line 666); primary label-stamping site for `reprocess_entity_types`.
- `crates/core/src/ontology_sidecar.rs` — `content_hash` is called here; no schema changes needed; hash change flows through to drift detection automatically.
- `crates/core/src/schema.rs` — `labels STRING[]` node column; no schema changes required.
- Issue #83 — established ontology support, `EntityTypeRaw`, and the two-label stamping convention.
- Issue #98 — established drift detection via `content_hash` and `OntologySidecar`.
- Issue #30 — established `reprocess_entity_types` and `apply_entity_type_labels`.
