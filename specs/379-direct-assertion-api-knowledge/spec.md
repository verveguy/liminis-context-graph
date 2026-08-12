# Feature Specification: Direct assertion API — `knowledge_assert_entity` / `knowledge_assert_relationship`

**Feature Branch**: `fabrik/issue-379`
**Created**: 2026-08-12
**Status**: Draft
**Input**: User description: "Add a direct write path so an agent that already knows a fact can record it without routing it through prose → LLM extraction. Two new WRITE-scope tools: `knowledge_assert_entity` (upsert an entity by name or explicit uuid within a group) and `knowledge_assert_relationship` (upsert an edge between two entities within one group). Supersedes #179, which specified the same feature and was closed unimplemented because two of its requirements are now actively wrong given #368/#369/#371."

## Background

Every fact currently enters the graph via a text episode that an LLM extracts entities and relationships from. The extraction model runs with far less context than the agent that authored the episode text. When an agent already knows the precise entities and relationships it wants recorded — because it just read an org chart, received structured data, or is composing a layer graph deliberately (#369 shipped cross-group edges; #378 will make each `group_id` its own WAL stream) — routing that knowledge through prose can only lose information relative to the agent's own understanding, never add to it.

Direct assertion is also the last missing piece for a deterministic structural test of multi-stream WAL. `knowledge_add_cross_group_edge` (#369), per-stream checkpoints (#365), and group-scoped merge (#368/#371) all already exist; only a way to populate a group's graph without depending on live LLM extraction output is missing. `knowledge_query_cypher` is not a substitute — it bypasses the WAL and embedding invariants the structured write handlers maintain.

This issue supersedes #179, which specified the same feature on 2026-06-24 and was closed unimplemented before reaching Research. Two of its requirements are now actively wrong in light of work that shipped after it was written:

- #179's edge-upsert match ignored `group_id`, matching solely on `(source_node_uuid, predicate, target_node_uuid)`. That is exactly the shape ADR-0368 identified as unsafe: an unscoped match lets a write in one group silently destroy another group's edge that happens to share a name and endpoints. ADR-0371 established the governing rule for this codebase — **a write in group G touches only G's data** — and this spec's edge upsert must honor it.
- #179 resolved relationship endpoints by name within `group_id` and errored when either endpoint could not be found, but left that as an implicit consequence of the algorithm rather than a stated requirement. An implementer could reasonably "improve" unresolved-name handling into a cross-group search, which would silently write the bare cross-group UUID foreign key that #369's `knowledge_add_cross_group_edge` (with its resolvable pointer fields) exists specifically to replace. This spec states the restriction explicitly.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Record a known fact without a prose round-trip (Priority: P1)

An agent has just read a structured source (an org chart, a config file, a table) and knows precisely which entities and relationship it wants recorded. Instead of writing a sentence and hoping extraction reconstructs the same entities, it calls `knowledge_assert_entity` for each entity and `knowledge_assert_relationship` for the connection between them.

**Why this priority**: This is the core capability the issue exists to deliver — a direct write path that avoids the fidelity loss of LLM extraction. Without it, nothing else in this spec matters.

**Independent Test**: Call `knowledge_assert_entity` twice to create two entities in the same group, then `knowledge_assert_relationship` between them by name. Verify a single edge exists connecting the two entities' UUIDs, with no episode or extraction step involved.

**Acceptance Scenarios**:

1. **Given** an empty group, **When** an agent calls `knowledge_assert_entity` with a name and no `entity_uuid`, **Then** a new entity is created in that group and its UUID is returned.
2. **Given** two entities already asserted into the same group, **When** an agent calls `knowledge_assert_relationship` naming both by their entity names and a predicate, **Then** a single directed edge is created between their UUIDs, scoped to that group.
3. **Given** an entity was already asserted with a given name in a group, **When** `knowledge_assert_entity` is called again with the same name and group, **Then** the same `entity_uuid` is returned and the entity's fields are updated in place rather than a duplicate being created.

---

### User Story 2 - Compose a layer graph deliberately, entity by entity (Priority: P2)

An agent building a layer graph (a `group_id` composed intentionally rather than via extraction) asserts each entity and relationship it wants in that layer, one call at a time, with full control over labels, attributes, and predicate — instead of writing text and hoping the extraction model classifies things the way the layer's design intends.

**Why this priority**: This is the second motivating use case named in the issue (layer graphs from #369/#378) and depends on the same two tools as Story 1, but specifically exercises `group_id` scoping and cross-group refusal, which Story 1 does not.

**Independent Test**: Assert entity A into group `"layer-x"` and entity B into group `"liminis"`. Attempt `knowledge_assert_relationship` between A and B within group `"layer-x"`. Verify the call is rejected rather than silently creating a cross-group edge, and that the error names `knowledge_add_cross_group_edge` as the correct tool.

**Acceptance Scenarios**:

1. **Given** an entity named "IBM" exists only in group `"liminis"`, **When** an agent calls `knowledge_assert_relationship` in group `"layer-x"` naming "IBM" as an endpoint, **Then** the call fails with an error identifying the unresolved name and directing the caller to `knowledge_add_cross_group_edge` for cross-group connections, and no edge is created.
2. **Given** an entity in group `"liminis"` was merged into a canonical entity (also in group `"liminis"`) via existing merge functionality, **When** an agent calls `knowledge_assert_entity` naming the pre-merge (tombstoned) name in that group, **Then** the assertion resolves forward to the canonical entity and updates it, rather than operating on the stale tombstone or erroring.

---

### User Story 3 - Deterministic multi-group WAL test fixture (Priority: P3)

A test author needs to construct a multi-group graph state with entities and cross-group edges in a fully deterministic way, with no dependency on live LLM extraction output, in order to write a structural test of the WAL that is not flaky.

**Why this priority**: This is the concrete unblock named in the issue for #378's test suite. It is lower priority than Stories 1–2 because it exercises no new tool behavior beyond what those stories already cover — it is a validation that the tools are sufficient for this purpose, not a distinct requirement.

**Independent Test**: Using only `knowledge_assert_entity`, `knowledge_assert_relationship`, and the existing `knowledge_add_cross_group_edge`, construct a fixture spanning two groups with a cross-group edge between them, with no episode or LLM call involved, and confirm the resulting graph state is identical across repeated runs.

**Acceptance Scenarios**:

1. **Given** a fresh graph, **When** a test asserts the same sequence of entities and relationships twice in two separate graph instances, **Then** both instances produce identical entity and edge UUIDs, names, and attributes (modulo timestamps), with no non-determinism introduced by name resolution.

---

### Edge Cases

- Asserting an entity whose name does not yet exist in the group creates a new entity.
- Asserting an entity by `entity_uuid` that does not exist in the target `group_id` fails with an error; assertion never silently falls back to creating a new entity under a caller-chosen UUID or to searching other groups for that UUID.
- Asserting an entity whose name (or explicit `entity_uuid`) resolves to a `Merged` tombstone forwards to the canonical entity (following the existing fixpoint `merged_into` chain with its existing cycle guard) and updates the canonical, rather than erroring or writing a new entity under the stale name.
- If forward resolution through a `Merged` tombstone cannot reach a canonical entity (dangling target, or the chain resolves as `Unbound`/`Ambiguous` per existing merge semantics), the assertion fails with an error rather than guessing.
- Re-asserting an identical entity (same name, same `group_id`) any number of times is idempotent: the same `entity_uuid` is returned every time, and the entity is updated in place — never duplicated.
- Re-asserting an identical relationship (same source, predicate, target, and `group_id`) any number of times is idempotent: a single edge is created and subsequent calls update it in place — never duplicated, and never confused with a same-named edge sharing endpoints in a different `group_id` (the upsert match is `group_id`-scoped per ADR-0368/ADR-0371).
- `knowledge_assert_relationship`'s endpoint name resolution is confined to the call's own `group_id`. If a name is not found within that group, the call fails even if an entity with that exact name exists in a different group — it never falls back to a cross-group search. The error names `knowledge_add_cross_group_edge` as the tool to use for connecting entities across groups.
- Name resolution for both tools uses exact (case-insensitive, whitespace-normalized) matching within the target `group_id` — not embedding-similarity fuzzy matching. A caller that names an entity gets exactly that entity, not a near-duplicate collapsed in by similarity, keeping assertion deterministic for test fixtures (Story 3) and predictable for direct callers generally.
- `group_id` defaults to `"liminis"` when omitted, matching existing write-tool conventions.
- `labels` defaults to `["Entity"]` when omitted or empty.
- `attributes`, when supplied, is accepted as a JSON object and stored JSON-serialized, matching the existing `attributes: String` convention on entity and edge rows.
- `fact`, when omitted on `knowledge_assert_relationship`, is auto-generated as `"<source_name> <predicate> <target_name>"`.
- `valid_at`, when supplied, accepts both RFC-3339 and lbug's space-delimited read-back timestamp format (e.g. `"2026-06-24 10:00:00"`); an unparseable value fails the call rather than causing a lbug `Binder exception`.
- If the configured embedder is unavailable when generating name/fact embeddings, the assertion still succeeds — an empty embedding is stored and a warning is surfaced, matching the existing embedder-unavailability behavior elsewhere in the write path.
- Entities and relationships created via assertion are flushed through the same WAL path as every other write handler and survive `knowledge_rebuild_from_wal`.
- Both tools acquire the writer lock, so an assert call is serialized against concurrent writes exactly like existing write handlers.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST provide a WRITE-scope tool `knowledge_assert_entity` that creates or updates a single entity identified by name (or by explicit `entity_uuid`) within a `group_id`.
- **FR-002**: The system MUST provide a WRITE-scope tool `knowledge_assert_relationship` that creates or updates a single directed edge between two entities resolved by name within a single `group_id`.
- **FR-003**: `knowledge_assert_entity` MUST default `group_id` to `"liminis"` when omitted.
- **FR-004**: `knowledge_assert_entity` MUST default `labels` to `["Entity"]` when omitted or empty.
- **FR-005**: `knowledge_assert_entity` MUST accept an `attributes` object and store it JSON-serialized, consistent with the existing `attributes: String` field on entity rows.
- **FR-006**: When `knowledge_assert_entity` is called without `entity_uuid`, the system MUST resolve an existing entity by exact (case-insensitive, whitespace-normalized) name match within the given `group_id`. This resolution MUST NOT use embedding-similarity matching.
- **FR-007**: When `knowledge_assert_entity` is called with `entity_uuid`, the system MUST look up that UUID within the given `group_id` and fail the call if no entity with that UUID exists in that group.
- **FR-008**: If entity resolution (by name per FR-006 or by UUID per FR-007) finds an entity carrying the `Merged` label, the system MUST follow the existing forward `merged_into` resolution (fixpoint, with cycle guard, as introduced for merge forwarding) to the canonical entity and apply the assertion to the canonical entity, rather than to the tombstone.
- **FR-009**: If forward resolution per FR-008 cannot reach a canonical entity within the same `group_id` (dangling target, or the chain resolves as `Unbound`/`Ambiguous`), the call MUST fail with an error rather than creating a new entity or guessing a target.
- **FR-010**: If no existing entity is resolved (per FR-006/FR-007/FR-008/FR-009), `knowledge_assert_entity` MUST create a new entity with the given `name`, `group_id`, `labels`, and `attributes`.
- **FR-011**: Calling `knowledge_assert_entity` repeatedly with the same `name` and `group_id` MUST be idempotent: it MUST return the same `entity_uuid` each time and update the entity's fields in place, never creating a duplicate entity.
- **FR-012**: `knowledge_assert_entity` MUST generate a name embedding via the configured embedder. If the embedder is unavailable, the call MUST still succeed, storing an empty embedding and surfacing a warning, rather than failing.
- **FR-013**: `knowledge_assert_relationship` MUST resolve both its source and target entities by exact (case-insensitive, whitespace-normalized) name match strictly within the call's own `group_id` — the same resolution rule as FR-006, including forward resolution through `Merged` tombstones per FR-008/FR-009.
- **FR-014**: `knowledge_assert_relationship` MUST fail with an error if either the source or target name cannot be resolved within the call's own `group_id`. This resolution MUST NOT search or fall back to any other `group_id`, even if an entity with the given name exists elsewhere.
- **FR-015**: The error raised per FR-014 MUST name `knowledge_add_cross_group_edge` as the tool to use for connecting entities across different groups.
- **FR-016**: `knowledge_assert_relationship`'s edge upsert match MUST be scoped to `(source_node_uuid, predicate, target_node_uuid, group_id)` — all four components, including `group_id` — so that asserting an edge in one group can never match, update, or overwrite an edge with the same predicate and endpoint UUIDs in a different group.
- **FR-017**: Calling `knowledge_assert_relationship` repeatedly with source, predicate, target, and `group_id` that all match a previously asserted edge MUST be idempotent: it MUST update that edge in place, never creating a duplicate.
- **FR-018**: `knowledge_assert_relationship` MUST default `group_id` to `"liminis"` when omitted.
- **FR-019**: `knowledge_assert_relationship` MUST accept an optional `fact` string; when omitted, the system MUST auto-generate it as `"<source_name> <predicate> <target_name>"`.
- **FR-020**: `knowledge_assert_relationship` MUST accept an optional `attributes` object and store it JSON-serialized, consistent with the existing `attributes: String` field on edge rows.
- **FR-021**: `knowledge_assert_relationship` MUST generate a fact embedding via the configured embedder, with the same embedder-unavailable fallback behavior as FR-012.
- **FR-022**: Both tools MUST accept an optional `valid_at` timestamp, accepting both RFC-3339 and lbug's space-delimited read-back format, without producing a lbug `Binder exception` on either accepted format.
- **FR-023**: Both tools MUST flush their writes through the same WAL mechanism used by existing write handlers, such that entities and relationships they create or update survive `knowledge_rebuild_from_wal`.
- **FR-024**: Both tools MUST acquire the writer lock for the duration of their write, consistent with existing write-handler concurrency behavior.
- **FR-025**: Both tools MUST be registered in the WRITE scope bucket of the MCP tool surface, alongside their handler dispatch, per this project's existing dispatch-table/tool-registry pairing convention.

### Key Entities

- **Entity**: A node in the knowledge graph, identified by `uuid`, `name`, `group_id`, `labels`, and free-form `attributes`. `knowledge_assert_entity` creates or updates one.
- **Relationship (edge)**: A directed, named connection between two entities within one `group_id`, carrying a `fact` string and optional `attributes`. `knowledge_assert_relationship` creates or updates one.
- **Group (`group_id`)**: The namespace an entity or edge belongs to. Both tools' name resolution and the edge upsert match are confined to a single `group_id`; only `knowledge_add_cross_group_edge` may connect across groups.
- **`Merged` tombstone**: An entity that has been superseded by a canonical entity via existing merge functionality, pointing to its canonical via `merged_into`. Both assert tools must resolve through this forward to the canonical rather than operating on the tombstone.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An agent can create a new entity and connect it to another entity via a single relationship, using only `knowledge_assert_entity` and `knowledge_assert_relationship`, with no episode text and no LLM extraction call involved.
- **SC-002**: Asserting the same entity (same name, same `group_id`) any number of times always results in exactly one entity node with a stable `entity_uuid`.
- **SC-003**: Asserting the same relationship (same source, predicate, target, `group_id`) any number of times always results in exactly one edge, never a duplicate.
- **SC-004**: Asserting a relationship whose source or target name exists only in a different `group_id` always fails, never creates an edge, and the resulting error names `knowledge_add_cross_group_edge`.
- **SC-005**: Asserting an edge in one `group_id` never modifies or removes an edge with the same predicate and endpoint UUIDs that exists in a different `group_id`.
- **SC-006**: Entities and relationships created via either assert tool are present, with identical field values, after a `knowledge_rebuild_from_wal`.
- **SC-007**: Asserting a fact naming an entity that has since been merged into a canonical entity updates the canonical entity's data, not the pre-merge tombstone.
- **SC-008**: A test suite can construct a deterministic, repeatable multi-group graph fixture (entities, in-group edges, and cross-group edges) using only assert tools plus the existing `knowledge_add_cross_group_edge`, with no dependency on live LLM output.

## Assumptions

- Forward resolution through `Merged` tombstones (FR-008) reuses the existing fixpoint-with-cycle-guard algorithm introduced for merge forwarding (#371); this spec does not require a new resolution algorithm, only that both assert tools apply it.
- Name resolution for both tools is exact-match only (no embedding-similarity fuzzy matching), by decision recorded in this spec: #179 had left this as a recommendation rather than a requirement, and this spec resolves it in favor of determinism, since a caller that names an entity explicitly wants that entity, and the primary motivating test case (#378) requires reproducible resolution. Embeddings are still generated and stored on every asserted entity/edge for use by other search paths (e.g. `knowledge_find_entities`), even though assertion itself does not use them for resolution.
- `knowledge_assert_relationship` resolves both endpoints by name only (no `entity_uuid` endpoint form); this matches the corrected behavior carried forward from #179's FR-009 and keeps the tool's cross-group refusal (FR-014/FR-015) simple to reason about. An explicit-UUID endpoint form for relationships, if wanted later, is a separate enhancement.
- The exact JSON input schemas (field names beyond those named in this spec, required/optional markers at the wire level) are a Plan/Implement-stage decision, to be made consistent with the existing `ToolSpec` conventions in `crates/service/src/mcp/tools.rs`.

## Out of Scope

- Cross-group edges — already covered by `knowledge_add_cross_group_edge` (#369, shipped). This spec's tools explicitly refuse cross-group endpoints rather than duplicating that functionality.
- Per-group WAL routing — tracked separately in #378.
- Bulk or batch assertion (asserting many entities/relationships in one call) — out of scope for this issue; the single-fact tools ship first, and batching is a separate question once this shape is validated in use.
- Any change to existing episode-based extraction (`knowledge_add_episode`) or to existing merge (`knowledge_merge_entities`) behavior. This spec adds new tools; it does not modify how those existing tools resolve or store data.

## Source References

- Issue #179 (superseded, closed unimplemented) — original 24-FR draft, useful for context but not authoritative; two of its requirements are corrected here as described in Background.
- ADR-0368 (`docs/adr/0368-group-scoped-edge-dedup-in-merge.md`) — governs FR-016's group-scoped edge upsert match.
- ADR-0369 (`docs/adr/0369-resolvable-cross-group-pointers.md`) — defines `knowledge_add_cross_group_edge`, the tool named in FR-015's error.
- ADR-0371 (`docs/adr/0371-merge-never-writes-foreign-group-data.md`) — governs the "a write in group G touches only G's data" rule underlying FR-013/FR-014/FR-016, and the `Merged`-tombstone forward-resolution semantics underlying FR-008/FR-009.
- `crates/core/src/cross_group.rs` — existing forward-resolution-through-`Merged` implementation to be reused per FR-008.
- `crates/core/src/db.rs` (`has_directed_edge`, `get_entity_by_name_ci_with_scan_fallback`) — existing group-scoped lookup primitives relevant to FR-006/FR-007/FR-013/FR-016.
