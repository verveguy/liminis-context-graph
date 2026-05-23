# Feature Specification: Tier 1b Bug — Edge-as-Node Schema Mismatch in Relationship Queries

**Feature Branch**: `fabrik/issue-38`
**Created**: 2026-05-22
**Status**: Draft
**Input**: User description: "list_relationships and get_entity_neighbors return Cypher binder errors against real workspaces because the queries use [r:RELATES_TO] with property access, but the production LadybugDB schema stores edges as RelatesToNode_ nodes."

## Background

When liminis-graph runs against a real workspace populated by the Python graphiti service (e.g., `~/dev/liminis-project/demo-notebook`), two Tier 1b JSON-RPC methods fail with:

```
Binder exception: Cannot find property uuid for r.
```

The root cause is an inconsistency between the LadybugDB schema as written by the Python graphiti service and the Cypher queries used by some read methods in `liminis-graph-core/src/db.rs`.

**Dual-encoding in `insert_relates_to_edge`**: The Rust write path creates both a `RelatesToNode_` shadow node (holding all edge properties: `uuid`, `name`, `fact`, `fact_embedding`, etc.) and a direct `[:RELATES_TO]` relationship between Entity nodes (with property copies). However, the Python graphiti service apparently does not create a direct `RELATES_TO` relationship with properties — it uses a different traversal pattern (likely `(src:Entity)-[:MENTIONS]->(n:RelatesToNode_)-[:MENTIONS]->(dst:Entity)`) that the Research stage must verify against the Python `EntityEdge` ORM and the live demo-notebook DB.

**The symptom**: Read queries that pattern-match `(src:Entity)-[r:RELATES_TO]->(dst:Entity)` and then access `r.uuid` / `r.name` crash on Python-populated workspaces because either (a) no `RELATES_TO` relationship exists, or (b) it exists without the expected properties. This affects:

- `list_relationships` (`db.rs` ~line 580–600) — confirmed failing
- `get_entity_neighbors` (`db.rs` ~line 620–631) — confirmed failing
- `get_edges_by_group_ids` (`db.rs` ~line 378–384) — same `[r:RELATES_TO]` pattern with property access; likely failing
- `get_edges_by_uuids` (`db.rs` ~line 393–398) — same; likely failing

**Known-correct precedent**: `count_relates_to_edges` (`db.rs` ~line 949–950) and `get_relates_to_by_uuids` (`db.rs` ~line 898–901) already use `RelatesToNode_` as the primary access pattern and succeed on live data.

This bug causes `knowledge_list_relationships` and `knowledge_get_entity_neighbors` to be completely broken against any workspace populated by the Python service, making Tier 1b effectively non-functional in production.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Browse relationships in a real workspace (Priority: P1)

A user navigates to the GraphitiPanel in liminis-app and requests the edge list. liminis-app calls `knowledge_list_relationships`. Against a real workspace, this currently returns a JSON-RPC error instead of the 231 edges reported by `knowledge_status`.

**Why this priority**: Core browsing is broken in production. The edge count is known (231), so the fix is verifiable against real data.

**Independent Test**: Run liminis-graph against demo-notebook, call `knowledge_list_relationships` with `num_results=10`, assert a non-empty `{facts: [...], count}` response with at least one edge whose `uuid`, `name`, `fact` fields are populated.

**Acceptance Scenarios**:

1. **Given** a workspace with 231 edges (per `knowledge_status`) and a live service, **When** `knowledge_list_relationships` is called with `num_results=10`, **Then** the response is `{facts: [10 edge objects], count: 10}` with each edge object containing `uuid`, `name`, `fact`, `group_id`, `valid_at`, `invalid_at`, and source/target entity UUIDs — not a JSON-RPC error.
2. **Given** `group_ids: ["some-group"]` that contains edges, **When** called, **Then** only edges belonging to that group are returned.
3. **Given** an empty DB, **When** called, **Then** `{facts: [], count: 0}` is returned — not an error.
4. **Given** a workspace populated entirely by the Python graphiti service (no Rust-written edges), **When** called, **Then** the method succeeds and returns edges — confirming schema compatibility.

---

### User Story 2 — Explore 1-hop graph from a known entity (Priority: P1)

A user clicks an entity in the GraphitiPanel or asks "what does X relate to?" in chat. liminis-app calls `knowledge_get_entity_neighbors` with the entity's UUID. Against a real workspace, this currently returns a Cypher binder error.

**Why this priority**: Graph navigation is completely broken in production; 7 call sites in liminis-app depend on this method.

**Independent Test**: Run liminis-graph against demo-notebook, call `knowledge_get_entity_neighbors` with a known entity UUID (e.g., `ff65e71d-46a1-4a14-aefb-b4086d10964c`), assert a response with `center_uuid`, `nodes`, `edges`, `node_count`, `edge_count` — not an error.

**Acceptance Scenarios**:

1. **Given** entity UUID `ff65e71d-46a1-4a14-aefb-b4086d10964c` which exists in demo-notebook with at least one edge, **When** `knowledge_get_entity_neighbors` is called, **Then** response is `{center_uuid: "ff65e71d-...", nodes: [...], edges: [...], node_count: N, edge_count: N}` with N ≥ 1.
2. **Given** an entity UUID that exists but has no edges, **When** called, **Then** `{center_uuid, nodes: [], edges: [], node_count: 0, edge_count: 0}` — not an error.
3. **Given** a workspace populated entirely by the Python graphiti service, **When** called, **Then** the method succeeds — confirming schema compatibility.

---

### User Story 3 — Integration test prevents regression (Priority: P2)

No automated test currently catches this class of schema mismatch because existing Tier 1b test fixtures were either populated via a direct Rust write path or don't exercise edge queries. A future fixture created differently could reintroduce this bug silently.

**Why this priority**: Without a test that mirrors production storage, any future schema change or test-fixture drift will cause silent regressions, as happened here.

**Independent Test**: This is the test — it should fail before the fix and pass after.

**Acceptance Scenarios**:

1. **Given** a test that calls `add_episode` with content that causes graphiti to extract at least one entity pair and one edge, **When** the episode is added and then `knowledge_list_relationships` is called, **Then** the response contains at least one edge whose fields are fully populated — confirming that edges stored through the canonical `add_episode` path are queryable by the list method.
2. **Given** the same setup, **When** `knowledge_get_entity_neighbors` is called with the UUID of one of the extracted entities, **Then** the response contains at least one edge and one neighbor node.

---

### Edge Cases

- A workspace populated by the Python service may have `RelatesToNode_` nodes without any direct `[:RELATES_TO]` relationship at all. Queries MUST succeed in this case.
- A workspace populated by the Rust service has both the shadow node and the direct relationship. Queries MUST NOT double-count edges.
- `get_edges_by_group_ids` and `get_edges_by_uuids` use the same wrong `[r:RELATES_TO]` pattern as `list_relationships`. Both MUST be audited and fixed if they fail against production data.
- Any other method in `db.rs` that accesses properties via `[r:RELATES_TO]` (rather than via `RelatesToNode_`) MUST be audited and fixed or explicitly confirmed as working against Python-populated data.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `list_relationships` MUST return correct results against a LadybugDB populated by the Python graphiti service (i.e., where edges are stored as `RelatesToNode_` nodes using the Python traversal pattern). It MUST use `RelatesToNode_` as the primary node for edge-property access.
- **FR-002**: `get_entity_neighbors` MUST return correct results against a Python-populated DB. It MUST NOT access edge properties through a direct `[r:RELATES_TO]` relationship pattern that does not exist or lacks properties in the Python schema.
- **FR-003**: The Research stage MUST verify the exact Cypher traversal pattern used by the Python graphiti `EntityEdge` ORM (likely `(src:Entity)-[:MENTIONS]->(n:RelatesToNode_)-[:MENTIONS]->(dst:Entity)`) by inspecting the Python source and the live demo-notebook DB, and this pattern MUST inform the corrected Cypher in `db.rs`.
- **FR-004**: All `db.rs` methods that perform a Cypher `MATCH ... [r:RELATES_TO] ...` and access properties on `r` MUST be audited. Any method that fails against a Python-populated DB MUST be corrected.
- **FR-005**: Methods corrected under FR-001/FR-002/FR-004 MUST continue to work against a Rust-populated DB (where both the shadow node and the direct relationship exist). No double-counting of edges is permitted.
- **FR-006**: A new integration test MUST be added (or an existing Tier 1b test extended) that:
  - Uses `add_episode` to store an episode whose content results in at least one extracted entity pair and one edge stored via the canonical path
  - Calls `knowledge_list_relationships` and asserts a non-empty result with fully-populated edge fields
  - Calls `knowledge_get_entity_neighbors` with one of the extracted entity UUIDs and asserts a non-empty result
- **FR-007**: The response shapes for `list_relationships` and `get_entity_neighbors` MUST remain unchanged — the fix is to the Cypher query, not the JSON-RPC response contract.

### Key Entities

- **`RelatesToNode_`**: The shadow node in LadybugDB that holds all edge properties (`uuid`, `name`, `fact`, `fact_embedding`, `group_id`, `valid_at`, `invalid_at`, `attributes`). Created by both Rust (`insert_relates_to_edge`) and Python graphiti. The canonical source of truth for edge data.
- **`[:RELATES_TO]`**: A direct relationship in LadybugDB between two `Entity` nodes. Created by Rust `insert_relates_to_edge` with property copies, but absent or property-free in Python-populated workspaces.
- **`[:MENTIONS]`**: A candidate relationship type used in the Python schema to link `Entity` nodes to `RelatesToNode_` (Research must confirm). Present in the Python graphiti traversal pattern.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `knowledge_list_relationships` called against demo-notebook (with 231 known edges per `knowledge_status`) returns a non-empty `facts` array with no JSON-RPC error.
- **SC-002**: `knowledge_get_entity_neighbors` called against demo-notebook with UUID `ff65e71d-46a1-4a14-aefb-b4086d10964c` returns a non-empty response with at least one edge and neighbor — no JSON-RPC error.
- **SC-003**: All `[r:RELATES_TO]`-with-property-access patterns in `db.rs` are either (a) corrected to use `RelatesToNode_`, or (b) explicitly verified to work against a Python-populated DB by direct testing (not assumption).
- **SC-004**: The new integration test (FR-006) passes in CI.
- **SC-005**: Existing Tier 1b tests continue to pass (no regression against Rust-populated fixtures).

## Assumptions

- The Python graphiti service does NOT create a direct `[:RELATES_TO]` relationship with properties between `Entity` nodes; it uses only the `RelatesToNode_` shadow node (and possibly `[:MENTIONS]` links). This must be confirmed by Research.
- The demo-notebook DB at `~/dev/liminis-project/demo-notebook/.graphiti/db` is accessible and contains real Python-populated data including the 231 edges reported by `knowledge_status`.
- `count_relates_to_edges` and `get_relates_to_by_uuids` (the known-working methods) are correct models for the access pattern. Research should derive the fix from these.
- The fix does not change the JSON-RPC response contract for `knowledge_list_relationships` or `knowledge_get_entity_neighbors` — only the internal Cypher changes.
- `add_episode` stores edges via `insert_relates_to_edge`, which creates both the shadow node and the direct relationship. This means a test using `add_episode` will exercise both — the fix must handle both storage patterns.

## Out of Scope

- Changing the Rust write path (`insert_relates_to_edge`) to match the Python schema — that is a separate consistency decision with WAL/replication implications.
- Removing the dual-encoding (shadow node + direct relationship) from the Rust write path — out of scope for this bug fix.
- Fixing `get_entities_by_source` — not identified as failing; not in scope unless the audit (FR-004) finds a broken `[r:RELATES_TO]` access there.
- Changing response shapes for any `knowledge_*` method.
- Performance optimization of the corrected queries.

## Source References

- `liminis-graph-core/src/db.rs` ~line 580–600 — `list_relationships` (wrong pattern)
- `liminis-graph-core/src/db.rs` ~line 620–631 — `get_entity_neighbors` (wrong pattern)
- `liminis-graph-core/src/db.rs` ~line 378–384 — `get_edges_by_group_ids` (same pattern; audit target)
- `liminis-graph-core/src/db.rs` ~line 393–398 — `get_edges_by_uuids` (same pattern; audit target)
- `liminis-graph-core/src/db.rs` ~line 898–901 — `get_relates_to_by_uuids` (correct; use as model)
- `liminis-graph-core/src/db.rs` ~line 949–950 — `count_relates_to_edges` (correct; use as model)
- Issue #27 — Tier 1b original spec (defines the five read methods this bug affects)
