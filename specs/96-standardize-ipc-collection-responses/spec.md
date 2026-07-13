# Feature Specification: Standardize IPC Collection Responses to `{<key>: [...], count: N}` Envelope Shape

**Feature Branch**: `fabrik/issue-96`
**Created**: 2026-05-26
**Status**: Draft
**Input**: Live bug 2026-05-26 — `liminis-app`'s UI entity search returned `resultCount: 0` for "Adrian Tchaikovsky" despite the entity being present in the graph and `knowledge_find_entities` IPC working correctly when called raw. Root cause: liminis-app's `parseMcpResult(result, 'nodes')` expects every collection response to be an object envelope with a named key, but `knowledge_find_entities` returns a bare array. The MCP semantic-search caller uses a different parser and doesn't hit the bug; the UI does and breaks silently.

## Background

liminis-graph IPC methods that return collections currently use inconsistent response shapes:

| Method | Current shape | Envelope? |
|---|---|---|
| `knowledge_list_entities` | `{nodes: [...], count: N}` | ✓ |
| `knowledge_list_relationships` | `{facts: [...], count: N}` | ✓ |
| `knowledge_get_entities_by_source` | object envelope | ✓ |
| `knowledge_get_entity_neighbors` | object envelope | ✓ |
| `knowledge_get_episodes` | object envelope | ✓ |
| **`knowledge_find_entities`** | **`[entity, entity, ...]` (bare array)** | **✗** |
| `knowledge_find_relationships` | **TBD — audit during this work** | ? |
| `knowledge_search_passages` | **TBD — audit during this work** | ? |
| `knowledge_get_edges_by_group` | **TBD — audit during this work** | ? |
| `knowledge_get_edges_by_uuids` | **TBD — audit during this work** | ? |
| `knowledge_get_nodes_by_group` | **TBD — audit during this work** | ? |
| `knowledge_list_sources` | (when implemented per cutover plan Stage 0) | should be envelope |
| `knowledge_preview_chunks` | (when implemented per cutover plan Stage 0) | should be envelope |
| `knowledge_suggest_duplicates` | (when implemented per cutover plan Stage 0) | should be envelope |
| `knowledge_entity_edge_analysis` | (when implemented per cutover plan Stage 0) | should be envelope |

Single-record / scalar responses are out of scope (they correctly return the record / scalar directly):
- `knowledge_status` returns a status object — fine as-is
- `health_check` returns a small object — fine
- `knowledge_close` returns `{"status": "closed"}` — fine

### Why this matters

- **Today's bug:** liminis-app's entity-search UI returns zero results for queries that semantically match present entities, silently. Users see "no matches" and assume their graph is empty.
- **Foot-gun for future methods:** every new collection-returning method has to remember which convention to use. The current split (`list_*` uses envelopes, `find_*` returns bare array) has no semantic justification — it's an accident.
- **Forward compatibility:** envelope shapes accept future fields without breaking clients (`scores`, `query_echo`, `pagination`, `total_count_before_limit`, `warnings`, ontology-filter telemetry). Bare arrays force breaking changes for every metadata addition.
- **Empty-vs-error clarity:** a parser seeing `{nodes: []}` knows the call succeeded with zero hits. A parser seeing `[]` has the same information with less structural confidence.
- **Symmetric clients:** liminis-app can write one parser for all collection responses, not per-method special-cases.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Entity-Search UI Returns Matching Entities (Priority: P1)

When the user types a query in the entity-search UI and the graph contains matching entities, the UI MUST display them. This is the immediate user-visible bug.

**Independent Test**: With a graph containing `Adrian Tchaikovsky`, send `graph:searchEntities` from the renderer with query `"Adrian Tchaikovsky"`. Assert `resultCount > 0` and the response contains the matching entity.

**Acceptance Scenarios**:

1. **Given** a graph containing entity X, **When** the UI search bar issues query matching X, **Then** the search response contains X (non-empty).
2. **Given** a graph with no matching entities, **When** the UI search bar issues a no-hit query, **Then** the search response is correctly empty (not an error).

---

### User Story 2 — All Collection-Returning IPC Methods Use the Same Envelope Shape (Priority: P1)

Every IPC method that returns a collection of records MUST return a JSON object with a named key for the collection plus a `count` field. The shape is `{<collection_key>: [...], count: N, ...optional_metadata}`.

**Why this priority**: this is the principled fix that closes User Story 1 and prevents recurrence in every future collection method.

**Independent Test**: For each method in the audit table above marked "TBD" or `find_*`, send a real request, verify the response is an object with the expected collection key and a `count` field. Assert no method returns a bare array.

**Acceptance Scenarios**:

1. **Given** the audit table from Background, **When** each method is called via raw IPC, **Then** every collection-returning response is a JSON object with a named array field + `count`.
2. **Given** the standardized contract, **When** a new collection-returning method is added in the future, **Then** following the envelope convention is the natural choice (codified by reviewer convention + an architecture note).

---

### User Story 3 — Existing Callers Don't Break (Priority: P1)

All existing callers of methods whose shape changes (notably `knowledge_find_entities` consumers — MCP semantic-search caller, liminis-app's `parseMcpResult` user) MUST continue to work after the change.

**Why this priority**: a contract change that breaks production callers is worse than the bug it fixes.

**Acceptance Scenarios**:

1. **Given** the MCP semantic-search caller (currently working against the bare-array response), **When** the response shape changes to envelope, **Then** the caller is updated as part of this PR to read `result.nodes` and continues to function.
2. **Given** any other in-tree consumer of an updated method, **When** that consumer is identified during the audit, **Then** it's updated in the same PR.
3. **Given** liminis-app's `parseMcpResult`, **When** the Rust server returns envelopes consistently, **Then** the parser works correctly without special-casing.

---

### User Story 4 — `count` Reflects the Returned Set, Not Pre-Filter Total (Priority: P2)

The `count` field in every envelope MUST equal `len(collection_key_array)` after all server-side filtering (limit, query, ontology) is applied. It is the size of what's being returned, not a pre-filter total.

**Why this priority**: ambiguity here breaks downstream UI logic (e.g. "show 5 more if count > limit").

If a separate "total before limit" is useful (e.g. for pagination), it goes in a different field (`total_before_limit`) — out of scope for this issue but documented as the convention.

**Acceptance Scenarios**:

1. **Given** any collection method called with `limit: N`, **When** there are M ≥ N matches, **Then** the response has `count: N` (not M).
2. **Given** the same method called with `limit: N`, **When** there are M < N matches, **Then** the response has `count: M`.

## Requirements *(mandatory)*

- **FR-001.** Audit every IPC method in `liminis-graph-core/src/handlers.rs` to identify all collection-returning methods. Produce a comprehensive list (the Background table is a starting point; assume there are more).
- **FR-002.** For each collection-returning method, the response MUST be a JSON object of shape `{<collection_key>: [...], count: <N>, ...optional}`. The collection key is the natural plural noun for the records (`nodes`, `facts`, `episodes`, `chunks`, `passages`, etc.) — match what's already used by sibling methods.
- **FR-003.** `knowledge_find_entities` MUST return `{nodes: [...], count: N}` instead of `[...]`. (The immediate-bug fix.)
- **FR-004.** Any other audit-discovered method returning a bare array MUST be updated to match.
- **FR-005.** All in-tree consumers of changed methods MUST be updated in the same PR — MCP semantic-search caller, any liminis-app handler, any test helper. No silent break.
- **FR-006.** An ADR MUST be added at `docs/adr/0020-ipc-collection-envelope-contract.md` documenting the envelope convention as binding for all current and future collection methods, with a brief rationale.
- **FR-007.** A schema-conformance test MUST be added (e.g. `liminis-graph-core/tests/ipc_response_shapes.rs`) that calls every collection-returning method against a small test fixture and asserts the response is an object with the expected collection-key and a `count` field. This catches regressions when new methods are added.
- **FR-008.** `count` MUST equal the length of the returned collection array. Pre-filter / pre-limit totals, if added later, live in a separate field name (`total_before_limit` is the suggested convention; not required for this issue).

## Success Criteria *(mandatory)*

- **SC-001.** Calling `knowledge_find_entities` via raw IPC returns `{nodes: [...], count: N}` shape — not a bare array. Verified by inspecting the response with `jq '. | type == "object"'`.
- **SC-002.** liminis-app's UI entity search returns matching entities for a query against a populated graph. Specifically: `graph:searchEntities` for "Adrian Tchaikovsky" in demo-notebook returns `resultCount > 0`. (This is the immediate-bug closing criterion.)
- **SC-003.** Every method in the audit table (and any others discovered during FR-001) returns an envelope response. Verified by the new conformance test in FR-007 — it asserts `response.is_object() && "count".in(response.keys())` for each.
- **SC-004.** No existing tests break. All in-tree consumers updated in the same PR.
- **SC-005.** ADR exists at `docs/adr/0020-ipc-collection-envelope-contract.md` documenting the convention.
- **SC-006.** A grep for `Result<Value, Error>` returning bare-array `serde_json::Value::Array(...)` from any handler returns zero hits after this work lands.

## Edge Cases

- **A method that returns a single record** (not a collection). Out of scope; current shape is correct.
- **A method that returns a count but no records** (e.g. a future `knowledge_count_entities`). Returns `{count: N}` — no collection key needed because there are no records.
- **A method that returns multiple parallel arrays** (e.g. nodes + scores). Both go inside the envelope: `{nodes: [...], scores: [...], count: N}`. Don't fragment into two response objects.
- **Empty collection.** Return `{<collection_key>: [], count: 0}`. Never return `{}` or `null` — explicit empty array, explicit zero count.
- **Server error during collection retrieval.** JSON-RPC error path (unchanged); no envelope involved.
- **An out-of-tree consumer relying on bare-array shape.** None exist today (verified by grepping in-tree). If external consumers ever appear (post-OSS), this contract change is documented in the ADR for future reference.

## Assumptions

- **A1.** The list of collection-returning methods is finite and discoverable from `handlers.rs`'s dispatch table — no dynamic / plugin-style method registration to worry about.
- **A2.** All in-tree consumers of the affected methods are also in-tree (no submodules, no external workspace packages depending on the IPC at the bare-array shape). Grep finds them all.
- **A3.** The `count` field is cheap to compute (it's just `array.len()`). No DB-level total-count work required.
- **A4.** The MCP servers (knowledge-reader, knowledge-writer, semantic-search) wrap their tool responses in their own format on top of the IPC response. They unwrap the envelope cleanly the same way they unwrap other enveloped methods today.
- **A5.** No external API consumers exist yet (this is pre-OSS). Contract correction is free.

## Out of Scope

- Changing scalar / single-record response shapes (`knowledge_status`, `health_check`, single-entity getters — they're not collections).
- Adding pagination cursors, scoring fields, or other new metadata to envelopes (separate issues per need).
- Changes to the liminis-app `parseMcpResult` parser — once envelopes are universal, it works without changes. (If you want to make it defensively accept bare arrays too, that's an independent hardening issue.)
- Versioning the IPC contract (e.g. `v2.knowledge_find_entities`). v1 contract just gets corrected; this is the only opportunity to do so before external OSS consumers exist.
- Changing the WAL JSONL format (separate concern — this issue is only about IPC response shape).
- Adding ontology-filter telemetry to envelopes (a possible future addition, but not now).

## Source References

- **Live bug:** demo-notebook 2026-05-26 — UI entity search shows `resultCount: 0` for present entities because the parser at `liminis-app/src/main/ipc/graph-handlers.ts:144` (`parseMcpResult`) assumes envelope shape, gets bare array, returns empty.
- **Graphiti Python service:** the original service generally used object envelopes for collection responses (verifiable from `graphiti_service.py` — useful as cross-check during audit). This work brings the Rust port back into alignment with that convention.
- **Cutover plan:** `ideas/cutover-plan.md` Stage 0 includes filing issues for the missing methods (`list_sources`, `preview_chunks`, `suggest_duplicates`, `entity_edge_analysis`). Those issues should reference ADR-0050 so the new methods ship correct from day one.
- **Sibling work:** liminis-graph#94 (relation_type field addition) and #92 (prompts port) have made the response shape of `knowledge_list_relationships` richer. This issue ensures other methods are equally well-shaped.
- **OSS-launch readiness:** `ideas/oss-launch-architecture.md` — fixing contract inconsistencies now, before external consumers, is much cheaper than after.
