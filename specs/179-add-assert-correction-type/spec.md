# Feature Specification: Add `assert` Correction Type for Direct Entity/Relationship Injection

**Feature Branch**: `fabrik/issue-179`
**Created**: 2026-06-24
**Status**: Draft
**Input**: User description: "Add an `assert` correction type (and supporting MCP tools) that allows a writing agent to inject entities and relationships directly into the graph, bypassing the LLM extraction pipeline."

## Background

Currently all facts enter the graph via text episodes → LLM extraction. This creates a quality gap: the extraction LLM operates with far less context than the agent that authored the episode. When the writing agent already *knows* the precise entities and relationships to record — for example, because it just looked up an org chart or received structured data — routing that knowledge through a prose→extraction round-trip introduces unnecessary noise and potential errors.

**Concrete example**: An agent with full conversation context learns:
- Sirr Less is `Principal PM, Internal Developer Platform`
- Sirr Less `REPORTS_TO` Tulika Garg
- Tulika Garg `REPORTS_TO` Ben Cochran
- Sirr Less `IS_DRIVING` Experiment Zone proposal

The agent can state these facts exactly. Forcing them through text-episode extraction adds a layer of interpretation that can only reduce fidelity.

The `assert` mechanism closes this loop: high-context agents write facts directly; the extraction pipeline handles unstructured document ingestion. Both paths feed the same graph.

The corrections-authoring skill already notes `assert` as a planned type (`.claude/skills/correction-authoring/SKILL.md`). This issue implements it.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Agent asserts a known entity directly (Priority: P1)

A writing agent has ground-truth knowledge about a person, project, or concept and wants to record it without constructing an episode. It calls `knowledge_assert_entity` with the entity name, labels, and structured attributes.

**Why this priority**: This is the most direct unblock for high-fidelity knowledge injection. The tool is usable standalone before relationships are needed. Without it, agents must write prose episodes and accept extraction noise.

**Independent Test**: Call `knowledge_assert_entity(name="Sirr Less", labels=["Person"], attributes={"role": "Principal PM, Internal Developer Platform"}, group_id="liminis")`. Assert the response contains `entity_uuid`. Call `knowledge_find_entities(query="Sirr Less")` and assert the entity appears with the correct role attribute. Call `knowledge_assert_entity` again with the same name but updated `role` attribute; assert the same `entity_uuid` is returned (idempotency) and the attribute is updated.

**Acceptance Scenarios**:

1. **Given** no entity named "Sirr Less" exists in group "liminis", **When** `knowledge_assert_entity(name="Sirr Less", labels=["Person"], attributes={"role": "Principal PM"}, group_id="liminis")` is called, **Then** the entity is created and the response is `{entity_uuid: "<uuid>"}`.
2. **Given** entity "Sirr Less" already exists in group "liminis" with `role="Director"`, **When** `knowledge_assert_entity(name="Sirr Less", labels=["Person"], attributes={"role": "Principal PM"}, group_id="liminis")` is called, **Then** the response contains the same `entity_uuid` and the entity's attributes are updated to `role="Principal PM"` (upsert; no duplicate created).
3. **Given** an entity assertion with `labels: []` (empty), **When** called, **Then** the entity is stored with at least the default `Entity` label applied automatically.
4. **Given** an entity assertion with a known `entity_uuid` that already exists in the DB, **When** called, **Then** the existing entity's attributes and labels are updated rather than creating a new one.
5. **Given** `group_id` is omitted, **When** called, **Then** the entity is created in the default group `"liminis"`.

---

### User Story 2 — Agent asserts a known relationship directly (Priority: P1)

A writing agent knows two entities and the relationship between them and wants to record it without constructing an episode. It calls `knowledge_assert_relationship` with the subject entity name or UUID, a predicate, and the object entity name or UUID.

**Why this priority**: Relationship injection is the other half of direct knowledge writing. Together with entity assertion (Story 1), it covers the full structured-fact injection use case.

**Independent Test**: Pre-create entities "Sirr Less" and "Tulika Garg" via `knowledge_assert_entity`. Call `knowledge_assert_relationship(subject="Sirr Less", predicate="REPORTS_TO", object="Tulika Garg", fact="Sirr Less reports to Tulika Garg", valid_at="2026-06-24", group_id="liminis")`. Assert the response contains `edge_uuid`. Call `knowledge_find_relationships(query="Sirr Less")` and assert the edge appears. Call the same `knowledge_assert_relationship` again; assert the same `edge_uuid` is returned (idempotency, no duplicate edge).

**Acceptance Scenarios**:

1. **Given** entities "Sirr Less" and "Tulika Garg" exist in group "liminis", **When** `knowledge_assert_relationship(subject="Sirr Less", predicate="REPORTS_TO", object="Tulika Garg", fact="...", valid_at="2026-06-24")` is called, **Then** the edge is created and the response is `{edge_uuid: "<uuid>"}`.
2. **Given** the edge `("Sirr Less", "REPORTS_TO", "Tulika Garg")` already exists with an older `valid_at`, **When** `knowledge_assert_relationship` is called again with a newer `valid_at`, **Then** the same `edge_uuid` is returned and `valid_at` is updated (upsert; no duplicate edge created).
3. **Given** `subject` references an entity name that does not exist in the group, **When** called, **Then** the call fails with a structured error: `{error: "subject entity '<name>' not found in group '<group_id>'"}`.
4. **Given** `object` references an entity name that does not exist in the group, **When** called, **Then** the call fails with a structured error: `{error: "object entity '<name>' not found in group '<group_id>'"}`.
5. **Given** `valid_at` is omitted, **When** called, **Then** the current UTC timestamp is used as `valid_at`.
6. **Given** `fact` is omitted, **When** called, **Then** a synthetic fact string is generated from subject/predicate/object (e.g., `"Sirr Less REPORTS_TO Tulika Garg"`).
7. **Given** `subject` and `object` are provided as UUIDs rather than names, **When** called, **Then** the entities are resolved by UUID and the edge is created correctly.

---

### User Story 3 — Agent uses the corrections file to batch-assert facts offline (Priority: P2)

A corrections file author (human or agent) includes `assert` entries alongside `same_as` and `retract` entries. When `knowledge_apply_corrections` runs, it processes the assert entries idempotently, creating or updating entities and relationships in the graph.

**Why this priority**: The corrections-file path lets agents or operators author structured asserts offline, review them before applying, and replay them after a WAL rebuild. It extends the existing corrections workflow rather than requiring direct tool access.

**Independent Test**: Write a corrections file with two assert entries (one entity, one relationship). Call `knowledge_validate_corrections` and assert `valid: true, unapplied_corrections: 2`. Call `knowledge_apply_corrections`. Assert response shows `applied: 2`, the corrections file now has `applied_at` timestamps on both entries, and the graph contains the asserted entity and relationship. Call `knowledge_apply_corrections` a second time; assert `skipped: 2, applied: 0`.

**Acceptance Scenarios**:

1. **Given** a corrections file with valid `assert` entries (entity and relationship), **When** `knowledge_apply_corrections` runs, **Then** each assert is applied (upsert semantics), `applied_at` is written, and counts are correct.
2. **Given** an `assert` entry whose `applied_at` is already set, **When** `knowledge_apply_corrections` runs, **Then** the entry is skipped (`skipped += 1`).
3. **Given** an `assert` entry referencing a non-existent subject or object entity, **When** `knowledge_apply_corrections` runs, **Then** that entry is counted in `errors`, a descriptive error message is recorded, and processing continues with the remaining corrections.
4. **Given** `dry_run: true`, **When** `knowledge_apply_corrections` runs against a file with `assert` entries, **Then** nothing is written to the graph or corrections file; the response counts pending asserts in `details` with `action: "dry_run:assert"`.
5. **Given** `knowledge_validate_corrections` is called on a corrections file with `assert` entries that have valid payloads, **Then** `valid: true` and those entries do not appear in `issues`.
6. **Given** `knowledge_validate_corrections` is called on a corrections file with an `assert` entry missing both `entity:` and `relationship:` keys, **Then** `valid: false` and the entry's id appears in `issues`.

---

### Edge Cases

- `knowledge_assert_entity` called with an entity name that already exists under a different UUID — the upsert matches by `(name, group_id)` and updates the existing entity; it does NOT create a second entity.
- `knowledge_assert_entity` called with an explicit `entity_uuid` that does not exist in the DB — creates a new entity with that UUID (pre-assigned UUID support).
- `knowledge_assert_relationship` called when both `subject` and `object` are the same entity — creates a self-loop edge; no special handling (consistent with existing edge model).
- `knowledge_assert_relationship` called with a predicate that differs from existing edges between the same pair — creates a new distinct edge (different predicate = different edge).
- An `assert` corrections file entry that contains both `entity:` and `relationship:` keys — the entry is rejected during validation with a message: "assert entry must have exactly one of 'entity' or 'relationship'".
- `knowledge_assert_entity` with attributes containing values that are not JSON-serializable strings/numbers/booleans — the service serializes attributes as JSON; non-serializable values produce a descriptive error.
- A WAL rebuild (`knowledge_rebuild_from_wal`) after direct entity/relationship assertions — entities and relationships injected via `knowledge_assert_entity` / `knowledge_assert_relationship` are NOT replayed from the WAL. They persist in the lbug DB file. Operators who need assert-injected data to survive a full WAL rebuild should retain the corrections file and re-run `knowledge_apply_corrections` after the rebuild.

## Requirements *(mandatory)*

### Functional Requirements

**`knowledge_assert_entity` MCP tool**

- **FR-001**: Accepts params `name: string` (required), `labels: list[string]` (optional, default `["Entity"]`), `attributes: object` (optional, default `{}`), `group_id: string` (optional, default `"liminis"`), `entity_uuid: string` (optional — if provided, used as the UUID; if it matches an existing entity, that entity is updated).
- **FR-002**: Upserts the entity: if an entity with matching `(name, group_id)` or matching `entity_uuid` already exists, update its labels and attributes; otherwise create a new entity.
- **FR-003**: Stored attributes are serialized as a JSON string (matching the `EntityRow.attributes: String` field). The caller-supplied `attributes` object is JSON-serialized by the service.
- **FR-004**: If `labels` is empty or omitted, the stored entity MUST carry at least the `Entity` label.
- **FR-005**: Generates a name embedding via the configured embedder (same embedder used by extraction) so that vector search finds asserted entities. If the embedder is unavailable, the entity is stored with an empty embedding and a warning is logged; the call does NOT fail.
- **FR-006**: Response: `{entity_uuid: string}`. On error: JSON-RPC error object with descriptive message.
- **FR-007**: Acquires the writer lock (this is a WRITE method).

**`knowledge_assert_relationship` MCP tool**

- **FR-008**: Accepts params `subject: string` (required, entity name or UUID), `predicate: string` (required), `object: string` (required, entity name or UUID), `fact: string` (optional), `valid_at: string` (optional, ISO-8601; defaults to current UTC), `group_id: string` (optional, default `"liminis"`).
- **FR-009**: Resolves `subject` and `object`: if the value looks like a UUID (matches UUID v4 format), resolve by UUID; otherwise resolve by `(name, group_id)`. If either cannot be resolved, return a structured error — do NOT auto-create missing entities.
- **FR-010**: Upserts the edge: match on `(source_node_uuid, predicate, target_node_uuid)`. If a non-invalidated edge with that triple already exists, update its `fact`, `valid_at`, and `attributes`; otherwise insert a new edge.
- **FR-011**: If `fact` is omitted, the stored fact string is auto-generated as `"<subject_name> <predicate> <object_name>"`.
- **FR-012**: Generates a fact embedding via the configured embedder (same embedder used by extraction). Same fallback as FR-005 — store empty embedding on embedder unavailability, warn, do not fail.
- **FR-013**: The `relation_type` field on the inserted/updated edge is set to `predicate` (normalized uppercase).
- **FR-014**: Response: `{edge_uuid: string}`. On error: JSON-RPC error object with descriptive message.
- **FR-015**: Acquires the writer lock (this is a WRITE method).

**`assert` corrections file type**

- **FR-016**: A `CorrectionEntry` with `type: assert` MUST contain exactly one of `entity:` or `relationship:` sub-object. Both absent or both present is a validation error.
- **FR-017**: Entity assert payload: `name` (required), `labels` (optional), `attributes` (optional), `entity_uuid` (optional).
- **FR-018**: Relationship assert payload: `subject` (required, name or UUID), `predicate` (required), `object` (required, name or UUID), `fact` (optional), `valid_at` (optional ISO-8601).
- **FR-019**: `knowledge_validate_corrections` MUST accept `assert` entries as valid (not flag them as unknown type) when their payload is structurally correct. It MUST flag them with descriptive issues when the payload is malformed (missing required keys, both or neither sub-object).
- **FR-020**: `knowledge_apply_corrections` MUST process `assert` entries using the same upsert semantics as FR-002 and FR-010. On success, `applied_at` is stamped atomically. On error (e.g., subject entity not found), the entry is counted in `errors` and processing continues.
- **FR-021**: `knowledge_apply_corrections` with `dry_run: true` MUST NOT apply any assert; it MUST report each assert entry in `details` with `action: "dry_run:assert"`.

**Common**

- **FR-022**: Neither `knowledge_assert_entity` nor `knowledge_assert_relationship` writes a WAL record. Asserted data persists in the lbug DB directly. A WAL rebuild does not replay asserted entities or relationships; the corrections file (if used) provides a separate replay path.
- **FR-023**: Both new MCP tools are registered in the handler dispatch table (in `handlers.rs`) alongside existing `knowledge_*` methods.

### Key Entities

- **Assert entity payload**: `{name, labels?, attributes?, entity_uuid?}` — describes an entity to be upserted into the graph.
- **Assert relationship payload**: `{subject, predicate, object, fact?, valid_at?}` — describes a directed edge to be upserted between two existing entities.
- **Upsert semantics (entity)**: Create if `(name, group_id)` match yields no existing entity; update attributes and labels otherwise. UUID-based lookup takes precedence if `entity_uuid` is provided.
- **Upsert semantics (relationship)**: Create if no non-invalidated edge exists with the same `(source_node_uuid, predicate, target_node_uuid)` triple; update `fact`, `valid_at`, `attributes` on the existing edge otherwise.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `knowledge_assert_entity` called twice with the same `(name, group_id)` returns the same `entity_uuid` both times and `knowledge_list_entities` shows exactly one entity for that name (no duplicates).
- **SC-002**: `knowledge_assert_relationship` called twice with the same `(subject, predicate, object)` triple returns the same `edge_uuid` both times and `knowledge_get_edges_by_group` shows exactly one non-invalidated edge for that triple.
- **SC-003**: An entity asserted via `knowledge_assert_entity` is discoverable by `knowledge_find_entities(query="<name>")` (FTS match).
- **SC-004**: A relationship asserted via `knowledge_assert_relationship` is discoverable by `knowledge_find_relationships(query="<fact>")`.
- **SC-005**: A corrections file with both `assert` entity and `assert` relationship entries passes `knowledge_validate_corrections` with `valid: true`. After `knowledge_apply_corrections`, both entries have `applied_at` set and the graph contains the asserted data.
- **SC-006**: A corrections file with a malformed `assert` entry (missing both `entity:` and `relationship:` keys) returns `valid: false` from `knowledge_validate_corrections` with the entry's id in `issues`.
- **SC-007**: `knowledge_apply_corrections` with `dry_run: true` on a file containing `assert` entries leaves the corrections file byte-identical before and after, and the graph is unchanged.
- **SC-008**: `knowledge_assert_relationship` with a non-existent subject entity returns a JSON-RPC error (not a panic or silent success), with the error message identifying the missing entity by name.

## Assumptions

- Entity name uniqueness within a group is the upsert key for `knowledge_assert_entity`. If multiple entities share the same name in the same group (which can arise from concurrent extraction), the upsert targets the one with the earliest `created_at` (consistent with the merge-entities canonical selection).
- Asserted data does NOT enter the WAL and is NOT replayed during `knowledge_rebuild_from_wal`. This is a deliberate trade-off: WAL replay would require a new WAL record type. The corrections file provides an alternative "replay" path for asserts when needed.
- The fact embedding and name embedding are generated synchronously by the configured embedder at assert time. The embedder is already required for `knowledge_add_episode` so it will be initialized by the time assert tools are called.
- `knowledge_assert_entity` with `entity_uuid` provided and pointing to a UUID that does not exist creates a new entity with that UUID. This allows agents that have already resolved a UUID (e.g., via `knowledge_find_entities`) to assert updates deterministically.
- The corrections YAML schema extension (`entity:` and `relationship:` sub-objects under an `assert` entry) requires adding new optional fields to `CorrectionEntry` or a new discriminated union; the schema design is an implementation decision for the Plan stage.
- The `predicate` in an asserted relationship is stored as-is in the `name` field of `RelatesToEdge`, and also stored as `relation_type` (normalized uppercase). This matches the existing edge model.

## Out of Scope

- Bulk assertion (asserting many entities/relationships in a single call) — each call asserts one entity or one relationship; batching is the caller's responsibility.
- Asserting episode nodes directly — episodes remain the province of `knowledge_add_episode` and the extraction pipeline.
- Undo / retract for asserted data via a new tool — existing `retract` corrections apply to asserted edges the same as extracted edges (matched by `edge_uuid`).
- GUI or corrections-UI changes in liminis-app — the tools are MCP-only in this issue.
- WAL record type for assert events — deferred; the corrections-file path provides offline replay.

## Source References

- `crates/core/src/corrections.rs` — existing corrections engine; `CorrectionEntry`, `apply_corrections_file`, `validate_corrections_file`
- `crates/core/src/handlers.rs` — handler dispatch table; existing `handle_apply_corrections`, `handle_validate_corrections`, `handle_merge_entities`
- `crates/core/src/types.rs` — `EntityRow`, `RelatesToEdge` structs
- `crates/core/src/db.rs` — `insert_entity`, `insert_relates_to_edge`, `get_entity_by_name`, `get_entity_by_uuid`
- `specs/30-tier-3-corrections/spec.md` — existing corrections spec; `same_as`, `retract` types
- Issue #162 (`specs/162-knowledge-merge-entities-collapse/spec.md`) — merge-entities implementation (upsert and edge-rewrite patterns)
