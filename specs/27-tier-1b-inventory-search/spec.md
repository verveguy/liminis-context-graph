# Feature Specification: Tier 1b — Inventory + Semantic Search + Neighbors

**Feature Branch**: `fabrik/issue-27`
**Created**: 2026-05-22
**Status**: Draft
**Input**: User description: "Tier 1b of the liminis-graph ↔ liminis integration. Five read-only methods that power the entity/relationship browsing, semantic passage search, and 1-hop graph navigation that liminis-app's GraphitiPanel + chat surfaces depend on."

## Background

This is Tier 1b of the staged Rust reimplementation of the Python `graphiti_service.py`. It adds five read-only JSON-RPC methods to the liminis-graph daemon that power the core browsing, search, and navigation surfaces in liminis-app:

- **`knowledge_search_passages`** — vector-cosine passage search (11 call sites in liminis-app; drives the chat "find relevant context" UX and the panel's search box)
- **`knowledge_list_entities`** — entity inventory with optional group filter (used by GraphitiPanel and the corrections workflow)
- **`knowledge_list_relationships`** — edge inventory with optional group filter (companion to list_entities)
- **`knowledge_get_entity_neighbors`** — 1-hop graph traversal from a named entity (7 call sites; drives "what does X relate to?" in chat and click-to-explore in panel)
- **`knowledge_get_entities_by_source`** — filter entities by source document (powers "delete everything I learned from this file")

All five are in the Python service's `READ_METHODS` set and require no write lock. The canonical response shapes are defined by the Python implementation at `graphiti_service.py` lines 1390–1620 (Principle I: IPC Parity). This issue is **blocked by Tier 1a (#26)**, which establishes the handler-dispatch pattern and JSON-RPC error shape these methods follow.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — User searches for a topic across all indexed content (Priority: P1)

A user types a search query in the chat or panel; liminis-app calls `knowledge_search_passages` to find the most relevant indexed document chunks. Without this method, the chat's "find relevant context" UX and the panel's search box both go dark.

**Why this priority**: 11 call sites in liminis-app; this is the highest-volume read method in the integration and the one most visible to end users. It blocks the primary search flow.

**Independent Test**: With a LadybugDB containing fixture episodes (e.g., 100 chunks across 10 source docs), send `knowledge_search_passages` with a query whose seed-set semantically matches 3 known chunks; assert those 3 appear in the top results with score above the threshold.

**Acceptance Scenarios**:

1. **Given** a populated DB and a non-empty query, **When** the client sends `knowledge_search_passages` with `query`, optional `num_results`, optional `min_score`, optional `group_ids`, **Then** response is `{passages: [{uuid, name, content, source_description, group_id, created_at, valid_at, score}], count}` ordered by descending score.
2. **Given** `min_score=0.9` and no passages above that threshold, **When** the request runs, **Then** `{passages: [], count: 0}` is returned — not an error.
3. **Given** an empty `query`, **When** the request runs, **Then** a structured JSON-RPC error is returned and no embedding call is made.
4. **Given** `num_results` outside valid bounds (negative, zero, or >100), **When** the request runs, **Then** the value is clamped to `[1, 100]` (matches Python implementation).
5. **Given** an empty DB, **When** `knowledge_search_passages` is called, **Then** `{passages: [], count: 0}` is returned — not an error.

---

### User Story 2 — User browses indexed entities and relationships (Priority: P2)

The GraphitiPanel's entity list and the corrections workflow call `knowledge_list_entities` to enumerate entities. The companion `knowledge_list_relationships` enumerates edges. Both support `num_results` capping and optional `group_ids` filter.

**Why this priority**: Core panel browsing; without this, operators cannot inspect the graph state or diagnose entity quality issues.

**Independent Test**: Insert 50 known entities (or 50 known relationships), call the corresponding list method with `num_results=20`, assert response contains 20 entries ordered most-recent-first with all expected fields populated.

**Acceptance Scenarios**:

1. **Given** 50 entities in DB and `num_results=20`, **When** `knowledge_list_entities` is called, **Then** response is `{nodes: [{uuid, name, group_id, created_at, summary, labels, ...source info}], count: 20}` ordered by `uuid DESC`.
2. **Given** `group_ids: ["foo", "bar"]`, **When** `knowledge_list_entities` is called, **Then** only entities belonging to those groups are returned.
3. **Given** absent `group_ids`, **When** `knowledge_list_entities` is called, **Then** all groups are returned — no implicit group filter is applied.
4. **Given** 50 relationships in DB and `num_results=20`, **When** `knowledge_list_relationships` is called, **Then** response is `{facts: [{...edge fields + source info}], count: 20}` ordered by `uuid DESC`.
5. **Given** `num_results=0` or a negative value, **When** either list method is called, **Then** a structured JSON-RPC error is returned (reject, not clamp — Python silently passes through to `LIMIT 0`; Rust MUST validate).

---

### User Story 3 — User explores the graph from a known entity (Priority: P2)

The chat surface calls `knowledge_get_entity_neighbors` when the user clicks an entity name or asks "what does X relate to?" — returns a 1-hop subgraph centred on that entity.

**Why this priority**: 7 call sites; drives the core "explore" UX in both chat and panel. Without it, the graph is browse-only by flat list — no navigation.

**Independent Test**: With a DB containing entity A linked to entities B, C, D via 3 distinct edges, call `knowledge_get_entity_neighbors` with `entity_uuid=A`; assert response contains center A, all 3 edges, and exactly nodes B/C/D as neighbors.

**Acceptance Scenarios**:

1. **Given** entity A with 3 neighbor edges, **When** `knowledge_get_entity_neighbors` is called with `entity_uuid=A`, **Then** response is `{center_uuid: A, nodes: [B, C, D], edges: [3 edges], node_count: 3, edge_count: 3}`.
2. **Given** an `entity_uuid` that does not exist, **When** called, **Then** response is `{center_uuid, nodes: [], edges: [], node_count: 0, edge_count: 0}` — empty, not an error.
3. **Given** missing `entity_uuid`, **When** called, **Then** a structured JSON-RPC error is returned.
4. **Given** `num_results=5` on an entity with 20 edges, **When** called, **Then** at most 5 edges are returned (matches Python's edge-uuid slice); duplicate neighbor nodes are deduplicated.
5. **Given** an entity that exists but has no edges, **When** called, **Then** response is `{center_uuid, nodes: [], edges: [], node_count: 0, edge_count: 0}`.

---

### User Story 4 — User filters entities by source document (Priority: P3)

The corrections and delete workflows call `knowledge_get_entities_by_source` to find every entity extracted from a given document path. This is the surface that powers "delete everything I learned from this file."

**Why this priority**: 1 call site but load-bearing for the deletion workflow — lower call frequency does not reduce importance.

**Independent Test**: With chunks from source `docs/a.md` mentioning entities X/Y and chunks from `docs/b.md` mentioning Z, call with `source="docs/a.md"`; assert X and Y returned, Z not.

**Acceptance Scenarios**:

1. **Given** entities X/Y from source A and Z from source B, **When** `knowledge_get_entities_by_source` is called with `source=A`, **Then** response is `{source: A, nodes: [X, Y], node_count: 2}`.
2. **Given** a partial `source` string (e.g., `"a.md"`), **When** called, **Then** matching uses `CONTAINS` (substring) semantics applied to `Episodic.source_description` — partial matches are returned. ⚠️ Callers MUST NOT assume exact-match behaviour.
3. **Given** missing or empty `source`, **When** called, **Then** a structured JSON-RPC error is returned.

---

### Edge Cases

- `knowledge_search_passages` on an empty DB → `{passages: [], count: 0}`, not an error.
- `knowledge_list_entities` / `knowledge_list_relationships` with `num_results=0` or negative → structured JSON-RPC error (reject, not silent pass-through to `LIMIT 0`).
- `knowledge_get_entity_neighbors` on an entity that exists but has no edges → `{center_uuid, nodes: [], edges: [], node_count: 0, edge_count: 0}`.
- `knowledge_get_entities_by_source` where `source` is a very short string (e.g., `"a"`) matches every document whose `source_description` contains the letter "a" — this is a known sharp edge of CONTAINS semantics; it is preserved from Python intentionally and is out of scope to fix here. The method's response or docstring MUST document this substring behaviour.
- All five methods called concurrently while a write is in progress → MUST NOT block; reads use the reader handle established in Tier 1a (#26), not the writer lock.
- Episodes with a missing `content_embedding` → skipped by `knowledge_search_passages` (not crashed on); count discrepancy tracked in a deferred follow-up issue.

## Requirements *(mandatory)*

### Functional Requirements

**Common**

- **FR-001**: All five methods are READ methods. They MUST acquire the reader handle established by Tier 1a (#26) and MUST NOT acquire the writer lock.
- **FR-002**: All five methods MUST return errors as JSON-RPC error objects (per the error shape established by Tier 1a). They MUST NOT crash the daemon or return partial results on error.

**`knowledge_search_passages`**

- **FR-003**: Accepts: `query` (string, required), `num_results` (int, default 10, clamped to `[1, 100]`), `min_score` (float, default 0.5, clamped to `[0.0, 1.0]`), `group_ids` (optional list of strings).
- **FR-004**: Response shape: `{passages: [{uuid, name, content, source_description, group_id, created_at (ISO-8601 or null), valid_at (ISO-8601 or null), score (4 decimal places)}], count}` ordered by descending score.
- **FR-005**: The search vector is computed by embedding `query` using the configured embedder (BAAI/bge-base-en-v1.5, 768-dim); matched against `Episodic.content_embedding` via HNSW or brute-force cosine using the same hybrid path as `add_episode`.
- **FR-006**: An empty `query` MUST return a structured error with no embedding call made.

**`knowledge_list_entities`**

- **FR-007**: Accepts: `num_results` (int, default 500), `group_ids` (optional list). Response: `{nodes: [{uuid, name, group_id, created_at, summary, labels, ...source info}], count}` ordered by `uuid DESC`.
- **FR-008**: When `group_ids` is absent or empty, no group filter is applied. When present, only members of those groups are returned.
- **FR-009**: `num_results` ≤ 0 MUST return a structured error.

**`knowledge_list_relationships`**

- **FR-010**: Accepts: `num_results` (int, default 1000), `group_ids` (optional list). Response: `{facts: [{...edge fields + source info}], count}` ordered by `uuid DESC`.
- **FR-011**: Same `group_ids` and `num_results` validation rules as FR-008 and FR-009.

**`knowledge_get_entity_neighbors`**

- **FR-012**: Accepts: `entity_uuid` (string, required), `num_results` (int, default 50), `group_ids` (optional list). Response: `{center_uuid, nodes: [neighbor entities], edges: [edge details], node_count, edge_count}`.
- **FR-013**: Returns both incoming and outgoing edges to/from the center entity (undirected perspective). Neighbor node UUIDs are the opposite endpoint of each edge.
- **FR-014**: `num_results` bounds the **edge** count (not node count). Duplicate neighbor nodes MUST be deduplicated.
- **FR-015**: A non-existent `entity_uuid` returns an empty result (not an error). A missing `entity_uuid` returns a structured error.

**`knowledge_get_entities_by_source`**

- **FR-016**: Accepts: `source` (string, required), `num_results` (int, default 100), `group_ids` (optional list). Response: `{source, nodes, node_count}`.
- **FR-017**: Source matching uses `CONTAINS` (substring) semantics applied to `Episodic.source_description`. This behaviour MUST be documented in the method's docstring or spec — callers MUST NOT assume exact-match.
- **FR-018**: A missing or empty `source` MUST return a structured error.

### Key Entities

- **`Episodic`**: A stored document chunk node in LadybugDB. Has `content_embedding` (768-dim float vector), `source_description` (string path/URL), `group_id`, `created_at`, `valid_at`, `name`, `content`.
- **`Entity` (node)**: A named concept extracted from episodes. Fields include `uuid`, `name`, `group_id`, `created_at`, `summary`, `labels`, and per-node source info.
- **`Relationship` (edge/fact)**: A directed edge between two entities. Fields include `uuid`, source/target node UUIDs, relationship type, `group_id`, `created_at`, and per-edge source info.
- **Reader handle**: The concurrent-read-capable DB connection established by Tier 1a (#26) per ADR-042. Required by all five methods.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Unmodified Python `reader_server.py` can call all five methods against liminis-graph and parse responses with no code changes (IPC Parity, Principle I).
- **SC-002**: `knowledge_search_passages` p95 latency < 200 ms over 50 queries on a 1k-episode fixture.
- **SC-003**: `knowledge_list_entities` and `knowledge_list_relationships` p95 latency < 100 ms for `num_results ≤ 100` on a 10k-entity fixture.
- **SC-004**: `knowledge_get_entity_neighbors` p95 latency < 50 ms for entities with ≤ 100 edges.
- **SC-005**: For 5 hand-picked queries on a fixture corpus, `knowledge_search_passages` returns the same top-3 passage UUIDs (in any order) as the Python implementation, with score deltas ≤ 0.05.
- **SC-006**: Every field present in each Python response is either (a) present in the Rust response with equivalent meaning, or (b) explicitly listed in release notes as deferred with a tracking issue.

## Assumptions

- `Episodic.content_embedding` is already populated for all episodes (per WAL embedding enrichment work merged 2026-04-23). Episodes missing embeddings are skipped by `knowledge_search_passages` and counted in a deferred follow-up issue.
- The reader handle (per ADR-042 reader/writer split) exists after Tier 1a (#26) merges and supports concurrent reads while a writer holds the write lock.
- The `_serialize_nodes_with_sources` / `_serialize_edges_with_sources` Python helpers attach per-node/edge source info. The Rust port should attach equivalent source info, matched empirically against Python output during validation. If full parity is hard to achieve in this tier, ship with just node/edge fields and defer per-result source-info enrichment to a follow-up issue.
- The embedder used by `knowledge_search_passages` is the same instance/config used by `add_episode` (BAAI/bge-base-en-v1.5, 768-dim, per `[[project-llm-routing]]`).
- `CONTAINS`-based source matching is intentional in Python and preserved here. A future spec may revisit this with exact-match or glob semantics.

## Out of Scope

- Cursor-based or offset-based pagination — current Python uses `num_results` cap only; that contract is preserved.
- Fuzzy / typo-tolerant search in `knowledge_search_passages` — pure vector cosine, no query rewriting.
- Source-pattern matching beyond `CONTAINS` — file a separate issue if needed.
- Full parity of the `_serialize_nodes_with_sources` / `_serialize_edges_with_sources` nested source info if Rust port cannot achieve it in this tier — document the deferred enrichment as a follow-up issue rather than blocking this tier.

## Source References

- `graphiti_service.py` lines 1390–1620 — canonical Python response shapes for all five methods
- Issue #26 — Tier 1a: establishes handler-dispatch pattern, JSON-RPC error shape, and reader handle
- ADR-042 — reader/writer split (referenced by FR-001)
- `.specify/memory/constitution.md` — Principle I (IPC Parity), Principle V (LLM adapters out-of-process)
