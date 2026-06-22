# Feature Specification: Cross-Episode Entity Resolution — Fix Identical-Name Duplication at Ingest

**Feature Branch**: `fabrik/issue-164`
**Created**: 2026-06-22
**Status**: Draft
**Input**: User description: "Ingestion is creating duplicate entities for the same real-world thing (53 nodes named 'Brett', 15 'Tyto', 13 'Ben'…). graphiti normally dedupes extracted entities by name + embedding within a group at `add_episode`; these numbers mean that resolution is NOT collapsing across episodes/sessions in liminis-context-graph."

## Background

A functioning knowledge graph must converge on one node per real-world entity. liminis-context-graph's ingest pipeline instead accumulates duplicates: production workspaces show 53 nodes named "Brett", 15 named "Tyto", 13 named "Ben" — names that are clearly the same real-world person, not 53 different people.

The upstream Python `graphiti_core` service solves this with a `resolve_extracted_nodes` / `dedupe_nodes` step inside `add_episode`: before a newly-extracted entity is persisted, it is compared against existing nodes in the same group (by name and by embedding similarity). If a match above threshold is found, the episode is attached to the existing node rather than creating a new one.

This step is either absent from, or not working correctly in, liminis-context-graph's ingest path. The consequence is that every episode that mentions "Brett" mints a fresh node, making the graph grow proportional to mentions rather than to real-world entities.

This is a blocking issue for graph quality. Any cleanup of duplicate nodes is undone by the next ingestion run. Entity resolution must work at ingest time, durably and persistently across episodes, before any other graph-quality work can hold.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Same Name in Two Episodes Yields One Node (Priority: P1)

When a user ingests two separate episodes that each mention "Brett", the graph MUST contain exactly one entity node named "Brett" after both episodes are processed. The second episode attaches to the existing node rather than creating a new one.

**Why this priority**: This is the minimal bar for a correct knowledge graph. 53 "Brett" nodes is the observed symptom. Identical-name resolution is the highest-confidence case and must be fixed first.

**Independent Test**: In a clean group, ingest episode A containing "Brett met Alice for lunch." Then ingest episode B containing "Brett called Alice later." Query all entity nodes in the group. Assert exactly one node with `name = "Brett"` exists.

**Acceptance Scenarios**:

1. **Given** a clean group and no pre-existing entity nodes, **When** episode A ("Brett met Alice.") is ingested, **Then** one entity node named "Brett" is created.
2. **Given** the group now contains one "Brett" node, **When** episode B ("Brett called Alice.") is ingested, **Then** no new "Brett" node is created; the existing node is reused; the total "Brett" node count remains 1.
3. **Given** the group contains one "Brett" node, **When** episode B is ingested, **Then** episode B's facts/edges are correctly attached to the existing "Brett" node (not dropped).

---

### User Story 2 — High-Similarity Name Variants Resolve to One Node (Priority: P1)

When different episodes use slightly different phrasings of the same entity name (e.g., "Brett Adamson" vs. "Brett A."), embedding-based resolution MUST coalesce them to a single node when their name embeddings exceed the dedup threshold.

**Why this priority**: Real-world corpora use name variants. Name-exact-match alone leaves a large class of cross-episode duplicates unfixed. Embedding-based resolution is the second resolution tier, required for completeness.

**Independent Test**: Ingest episode A containing "Brett Adamson joined the team." Then ingest episode B containing "Brett A. attended the standup." Assert that the resulting graph contains at most one node whose name embedding is within dedup threshold of "Brett Adamson" — not two distinct nodes for the two phrasings.

**Acceptance Scenarios**:

1. **Given** a group with an existing "Brett Adamson" node, **When** an episode mentions "Brett A." and the embedding similarity between "Brett A." and "Brett Adamson" exceeds `DEDUP_THRESHOLD`, **Then** the extracted "Brett A." entity resolves to the existing "Brett Adamson" node.
2. **Given** a group with an existing "Alice Wang" node, **When** an episode mentions "Bob Chen" and the embedding similarity is below `DEDUP_THRESHOLD`, **Then** a new "Bob Chen" node is created (correct: no false collapse).

---

### User Story 3 — Resolution Works With and Without Ontology (Priority: P1)

Cross-episode entity resolution MUST be orthogonal to whether an ontology is configured. The dedup step runs regardless of whether the workspace has an active ontology.

**Why this priority**: The issue explicitly calls this out as a requirement. Ontology support is a separate feature; tying dedup behavior to it would create a silent regression for ontology-free workspaces.

**Acceptance Scenarios**:

1. **Given** a workspace with no ontology configured, **When** two episodes mentioning "Brett" are ingested, **Then** one "Brett" node results (same as US1).
2. **Given** a workspace with an ontology that types "Brett" as `Person`, **When** two episodes mentioning "Brett" are ingested, **Then** one "Brett" node results and the ontology typing is applied to that single node.

---

### User Story 4 — Resolution Is Durable Across Sessions (Priority: P1)

A "Brett" node persisted by episode A in session 1 MUST be found and reused when episode B is ingested in a later, separate session (after service restart). Resolution reads from the persisted graph state, not from an in-process cache or per-episode-local state.

**Why this priority**: The observed symptom (53 "Brett" nodes) accumulated across many ingestion sessions. Per-session dedup would reduce the rate of duplication but not eliminate it. The fix must be durable.

**Acceptance Scenarios**:

1. **Given** episode A has been ingested and the service restarted, **When** episode B (also mentioning "Brett") is ingested in the new session, **Then** the existing "Brett" node from the pre-restart graph is found and reused.

---

### Edge Cases

- **Entity extracted with empty name**: Skip resolution; treat as an extraction failure and drop the entity.
- **Group has a very large number of existing entities (50k+)**: Resolution MUST use the same HNSW/BM25 hybrid dedup path as the existing dedup infrastructure (issue #5, fixed by #16) for embedding-based lookups, not a linear scan.
- **Two concurrent ingest calls create the same entity**: Resolution is susceptible to a TOCTOU race under concurrent episode processing. The fix should handle or document this, but strict serializability within a group is out of scope for v1.
- **Name match is case-differing** (e.g., "brett" vs "Brett"): Case-insensitive name comparison MUST be used.
- **Name match includes leading/trailing whitespace**: Whitespace normalization MUST be applied before name comparison.
- **Group is unset / null**: Resolution is scoped to group_id. A null group_id resolves against the null-group entities (consistent with current group semantics).
- **Existing node found but has a different type under ontology**: The type on the existing node takes precedence; do not overwrite the existing node's type. Log a warning if the extracted type conflicts.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: After entity extraction for an episode, the ingest pipeline MUST query the persisted graph for existing entity nodes in the same `group_id` whose `name` matches the extracted entity name (case-insensitive, whitespace-normalized). If a match is found, the episode MUST be attached to the existing node instead of creating a new one.

- **FR-002**: For extracted entities that are not resolved by name match (FR-001), the ingest pipeline MUST query the persisted graph by embedding similarity using the existing dedup infrastructure (same `DEDUP_THRESHOLD` and query path as `brute_force_similar_entity` / `hybrid_dedup_similar_entity`). A match at or above threshold MUST cause the extracted entity to resolve to the existing node.

- **FR-003**: Resolution MUST read from the persisted LadybugDB graph state. In-process caches or per-episode-local state MUST NOT be the sole resolution source — a node created in a previous session MUST be discoverable.

- **FR-004**: Resolution MUST be scoped to the same `group_id` as the episode. Entities in different groups MUST NOT be resolved to each other.

- **FR-005**: The resolution step MUST execute regardless of whether an `Ontology` is configured for the workspace. Ontology configuration is orthogonal to entity dedup.

- **FR-006**: When an extracted entity resolves to an existing node (by either name or embedding), the episode's extracted facts and edges MUST be attached to the existing node's UUID. The episode MUST NOT be silently dropped.

- **FR-007**: A regression test MUST be added covering at minimum: (a) two episodes with identical entity names in the same group yield one node, and (b) cross-session durability (the resolving node was created in a prior ingest run).

- **FR-008**: An embedding-based resolution regression test MUST be added: two episodes with high-similarity (above `DEDUP_THRESHOLD`) but non-identical entity names in the same group yield at most one node.

- **FR-009**: The performance cost of the resolution step per episode MUST be bounded to at most one name-lookup query plus one dedup query (using the existing hybrid path) per extracted entity. O(N × M) queries where N = entities per episode and M = total entities in group are not acceptable.

### Key Entities

- **EntityNode**: A persisted node in the graph representing a real-world entity. Has `uuid`, `name`, `name_embedding`, `group_id`, and optional type and attribute fields.
- **Episode**: A unit of ingested content. After processing, its extracted entities are attached to EntityNodes (existing or newly created).
- **Resolution step**: The pipeline stage between entity extraction and entity persistence that queries the persisted graph for matches and collapses duplicates. Corresponds to graphiti's `resolve_extracted_nodes` / `dedupe_nodes`.
- **`DEDUP_THRESHOLD`**: The cosine similarity threshold above which two entity embeddings are considered the same real-world entity. Already defined by issue #5; this issue reuses that constant for cross-episode resolution.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Ingesting two episodes in the same group that each mention "Brett" results in exactly **one** entity node named "Brett" in the persisted graph, verifiable via `knowledge_find_entities` or a direct Cypher count query.

- **SC-002**: Ingesting two episodes in the same group that mention "Brett Adamson" and "Brett A." (embeddings above `DEDUP_THRESHOLD`) results in at most **one** entity node, not two.

- **SC-003**: A service restart between episode A and episode B does not prevent resolution — episode B's "Brett" resolves to the node created by episode A.

- **SC-004**: All existing tests pass with no regression.

- **SC-005**: The new regression tests (FR-007 and FR-008) pass.

- **SC-006**: The fix passes `cargo clippy --release --all-targets -- -D warnings` and `cargo fmt --all`.

## Assumptions

- **A1**: The dedup algorithm (HNSW/BM25 hybrid from #5, recall fixed by #16) is the correct infrastructure for embedding-based cross-episode resolution. This issue assumes issue #16 is merged or will be merged before this issue's implementation. Embedding-based resolution (FR-002) depends on adequate recall from the hybrid dedup path.

- **A2**: The `DEDUP_THRESHOLD` constant defined by issue #5 is the appropriate threshold for cross-episode embedding-based resolution. If Research finds graphiti uses a different threshold, adjusting the constant is in scope for the Plan stage.

- **A3**: Group scoping follows graphiti's behavior: entities are resolved within a group, never across groups. If the workspace uses a single default group, all entities resolve against the single-group pool.

- **A4**: "Case-insensitive name match" means Unicode case-folding (or at minimum ASCII case-fold). Exact byte match after lowercasing is sufficient for v1.

- **A5**: Resolution is a "prevent future duplicates" fix, not a "clean up existing duplicates" fix. Existing 53 "Brett" nodes from past ingest runs are not automatically merged by this change.

- **A6**: The IPC surface does not need to change. Resolution is an internal ingest-pipeline behavior; no new IPC methods are required.

- **A7**: WAL Principle IV (WAL is authoritative) applies: entity node creation continues to be WAL-logged. Entity node reuse (resolving to an existing node) does not require a new WAL entry if no mutation occurs on the existing node — but the episode's facts/edges that attach to the existing node must still be WAL-logged per their normal paths.

## Out of Scope

- **Deduplication of existing duplicate nodes** already in the graph. Cleaning up the 53 historical "Brett" nodes is a separate issue; this fix only prevents new duplicates from being created.
- **Cross-group entity resolution.** Entities in different groups are intentionally isolated.
- **Fuzzy name matching** beyond embedding similarity (e.g., Levenshtein distance, Soundex, phonetic matching). The two resolution tiers are name-exact and embedding-similarity.
- **IPC surface changes.** No new wire methods are needed.
- **WAL format changes.** WAL entries for entity creation are unchanged.
- **TOCTOU handling for concurrent ingestion.** Strict serializability under concurrent episode processing is deferred.
- **Merging attributes from duplicate nodes.** When resolving to an existing node, the existing node's attributes are not modified. Attribute merge is a separate concern.

## Source References

- **graphiti upstream**: `graphiti_core/graph.py` — `resolve_extracted_nodes`, `dedupe_nodes` (the upstream pipeline model for this step)
- **Issue #5 / spec `005-hnsw-bm25-dedup-plan.md`**: The existing dedup infrastructure this fix builds on (`brute_force_similar_entity`, `hybrid_dedup_similar_entity`, `DEDUP_THRESHOLD`)
- **Issue #16 / spec `specs/16-hybrid-dedup-overlap-fails/spec.md`**: The dedup recall fix (prerequisite for embedding-based resolution to work reliably)
- **Issue #92 / spec `specs/92-port-graphiti-s-extraction/spec.md`**: Extraction quality improvements (orthogonal, but improves entity-name fidelity that resolution depends on)
- **`crates/core/src/extractor.rs`**: The extraction pipeline entry point; the resolution step inserts between extraction and entity persistence
- **`crates/core/src/db.rs`**: Entity lookup functions (`get_entity_by_name`, `brute_force_similar_entity`, `hybrid_dedup_similar_entity`) — the query building blocks for the resolution step
- **`crates/core/src/handlers.rs`**: The `add_episode` / `process_chunk` IPC handler — the ingest path where the resolution step must be wired in
