# Feature Specification: knowledge_merge_entities — Collapse Duplicate Entities

**Feature Branch**: `fabrik/issue-162`
**Created**: 2026-06-22
**Status**: Draft
**Input**: User description: "Add an entity-merge capability that can collapse identical-name and arbitrary duplicate entities into one canonical node, by generalizing the already-tested apply_same_as edge-rewrite logic."

## Background

Cross-episode entity resolution in production has produced massive identical-name duplication: **53 nodes literally named "Brett"**, 15 "Tyto", 13 "Ben", 12 "Brett Adam", 11 "Kevin", 9 "Alex"/"Matt". A track-1 analysis found ~**2,230 nodes (17%)** are duplicates across 1,329 clusters.

The existing `same_as` correction mechanism (`knowledge_apply_corrections`, Issue #30) cannot fix this class of problem. `apply_same_as` resolves each alias via `get_entity_by_name(name, group)`, which uses `MATCH (e:Entity) WHERE e.name = $name … LIMIT 1`. With 53 nodes named "Brett", one `same_as` entry merges ONE node and leaves 52 untouched. `same_as` is built for name-*variant* dedup (e.g., "Brett Adam" → "Brett"), not identical-name clusters.

This feature adds a dedicated IPC method `knowledge_merge_entities` that:
- Takes a canonical entity (by UUID or name) plus a set of aliases to merge (by name — matching ALL entities with that name — and/or by explicit UUID list)
- Reuses the proven `apply_same_as` edge-rewrite, edge-dedup, and alias-invalidation machinery from `crates/core/src/corrections.rs`
- Writes WAL-durable merge records so the merged state survives rebuild and recovery
- Supports `dry_run` for reviewing the merge plan before applying

The 1,329-cluster `dedup_candidates.csv` from the track-1 analysis provides ready-made merge sets for the immediate production use case.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Merge All Identical-Name Entities Into One Canonical (Priority: P1)

A user has 53 nodes all named "Brett" and wants to collapse them into a single canonical. They call `knowledge_merge_entities` with `canonical_name: "Brett"` and `merge_all_by_name: true`. The service finds all 53 nodes, selects the canonical (earliest `created_at`), rewrites all edges from the 52 aliases onto the canonical (deduplicating directed edges), and marks the aliases as merged. The canonical retains the earliest `created_at` across all 53 nodes.

**Why this priority**: This is the primary production problem. 17% of the graph is duplicates, and this story directly addresses the identical-name cluster case. Without it, the track-1 dedup effort is manual and the graph degrades further on every ingest.

**Independent Test**: Seed a test graph with 5 nodes all named "Brett", each with distinct outgoing and incoming edges. Call `knowledge_merge_entities { canonical_name: "Brett", merge_all_by_name: true }`. Assert: exactly 1 entity named "Brett" remains, all edges previously pointing to any of the 5 are now on the canonical, 0 duplicate directed edges, 4 nodes marked as merged (not hard-deleted). Replay the WAL from scratch; assert the merged state is reproduced.

**Acceptance Scenarios**:

1. **Given** N entities with identical `name` in the same group, **When** `knowledge_merge_entities { canonical_name: "<name>", merge_all_by_name: true }` is called, **Then** the response reports `canonical_uuid`, `merged_count: N-1`, `edges_rewritten: <int>`, `edges_deduplicated: <int>`, and the graph has exactly 1 entity with that name.
2. **Given** the same call on a graph where only 1 entity has that name, **When** called, **Then** `merged_count: 0`, no mutations, `success: true`.
3. **Given** a merge has already been applied, **When** the same call is made again, **Then** aliases are already marked merged and skipped; `merged_count: 0`, `skipped: N-1`, idempotent.

---

### User Story 2 — Merge an Explicit UUID Set Into a Canonical (Priority: P1)

A user has run dedup analysis and has `dedup_candidates.csv` listing cluster members by UUID. They want to merge a cluster of 7 "Brett Adam" nodes (each with slightly different provenance but representing the same person). They call `knowledge_merge_entities { canonical_uuid: "<uuid>", alias_uuids: ["<uuid2>", …, "<uuid7>"] }`.

**Why this priority**: The UUID-based path is the programmatic interface for batch-processing `dedup_candidates.csv` clusters. Without it, the 1,329-cluster dedup job requires manual curation rather than scripted execution.

**Independent Test**: Seed 3 entities with distinct UUIDs and arbitrary names. Designate one as canonical. Call `knowledge_merge_entities { canonical_uuid: "<canonical>", alias_uuids: ["<alias1>", "<alias2>"] }`. Assert 1 entity remains; alias edges rewritten; aliases marked merged; WAL replay reproduces the state.

**Acceptance Scenarios**:

1. **Given** a canonical UUID and a list of alias UUIDs, **When** `knowledge_merge_entities { canonical_uuid, alias_uuids }` is called, **Then** all alias entities' edges are rewritten to the canonical, aliases are marked as merged, `merged_count: len(alias_uuids)`.
2. **Given** `alias_uuids` contains the same UUID as `canonical_uuid` (self-merge), **When** called, **Then** that UUID is silently skipped; no self-loop edges are created; remaining aliases merge normally.
3. **Given** an alias UUID that does not exist in the graph, **When** called, **Then** the error is captured in `errors[]`, processing continues for the remaining aliases.
4. **Given** an alias UUID that is already marked as merged (from a prior merge), **When** called, **Then** it is silently skipped and counted in `skipped`.

---

### User Story 3 — Preview a Merge Without Applying (Priority: P1)

A user is unsure which entities and edges will be affected. They call `knowledge_merge_entities { canonical_name: "Brett", merge_all_by_name: true, dry_run: true }`. The service computes and returns the full merge plan — which UUIDs become aliases, how many edges will be rewritten, which duplicate edges will be collapsed — without mutating the graph or writing any WAL record.

**Why this priority**: With 53+ node clusters, a bad merge could corrupt large graph regions. A dry run is a critical safety gate before any bulk operation.

**Independent Test**: Set up a test graph. Call `knowledge_merge_entities` with `dry_run: true`. Assert: (a) response contains the full merge plan with per-alias edge counts, (b) entity and edge counts in the graph are identical before and after the call, (c) no WAL records were written.

**Acceptance Scenarios**:

1. **Given** `dry_run: true`, **When** called, **Then** the response contains `plan: { canonical_uuid, aliases: [{ uuid, name, active_edges: int, duplicate_edges: int }], total_edges_rewritten: int, total_edges_collapsed: int }` and NO graph mutations occur.
2. **Given** `dry_run: true`, **When** called, **Then** no WAL record is emitted and no entity is marked as merged.
3. **Given** an invalid merge (e.g., canonical UUID not found), **When** `dry_run: true`, **Then** `success: false` with an error message — same validation as a live merge.

---

### Edge Cases

- Merging into a canonical that is itself already marked as merged → `success: false`, error: "canonical entity is already merged — cannot use as merge target".
- `alias_names` containing a name that matches 0 entities → warning in `errors[]`, skipped; other aliases still processed.
- `alias_names` and `alias_uuids` may be combined in a single call (union of both sets, deduped before processing).
- All aliases are already merged (idempotent re-run) → `merged_count: 0`, `skipped: N`, `success: true`.
- An edge being rewritten is itself invalidated/retracted → the invalidated edge MUST NOT be rewritten (only active edges are moved to the canonical).
- Two entities with the same name but different `group_id` → name lookups are scoped to the specified `group_id`; cross-group merges are not performed.
- A directed edge from alias A to canonical C (or C to A) → would produce a self-loop after merge; MUST be dropped, not written to the canonical.
- Very large clusters (hundreds of entities) → must not time out or OOM; no per-cluster size limit is imposed.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Implement a new JSON-RPC 2.0 method `knowledge_merge_entities` on the write path; it MUST acquire the writer lock.
- **FR-002**: Accept params: `canonical_uuid?` (string), `canonical_name?` (string), `alias_uuids?` (string[]), `alias_names?` (string[]), `merge_all_by_name?` (bool, default `false`), `group_id?` (string, default `"liminis"`), `dry_run?` (bool, default `false`). At least one of `canonical_uuid` or `canonical_name` MUST be provided; at least one of `alias_uuids`, `alias_names`, or `merge_all_by_name: true` MUST be provided. Violating either MUST return a structured error.
- **FR-003**: When `canonical_name` is provided without `canonical_uuid`, the canonical is the entity with that name and the earliest `created_at` in the specified `group_id`; ties broken by lexicographic UUID order.
- **FR-004**: When `merge_all_by_name: true` is set, ALL entities sharing the canonical's `name` in the specified `group_id` are treated as alias candidates (not `LIMIT 1`).
- **FR-005**: When `alias_names` is provided, ALL entities matching each listed name in the specified `group_id` are included as aliases.
- **FR-006**: The edge-rewrite, edge-dedup, and alias-invalidation logic MUST reuse the proven `apply_same_as` machinery in `crates/core/src/corrections.rs` — do not reimplement the edge-move logic.
- **FR-007**: After merge, the canonical entity MUST have `created_at` equal to the earliest value across the canonical and all aliases.
- **FR-008**: Merged alias entities MUST be marked as merged (invalidated) — NOT hard-deleted — preserving provenance.
- **FR-009**: Directed edges that would be duplicated on the canonical after merging (same source/target/relation type) MUST be deduplicated; one copy retained.
- **FR-010**: Self-loop edges that would arise from merging two ends of an existing edge MUST be dropped, not written to the canonical.
- **FR-011**: Invalidated/retracted edges on alias entities MUST NOT be rewritten to the canonical — only active edges are moved.
- **FR-012**: Self-UUID in `alias_uuids` (canonical in its own alias list) MUST be silently skipped.
- **FR-013**: Alias UUIDs already marked as merged MUST be silently skipped and counted in `skipped`.
- **FR-014**: Non-dry-run merges MUST write a WAL-durable merge record and flush (`drain_mutations()`) before returning.
- **FR-015**: WAL replay of a merge record MUST reproduce the same merged graph state as the original apply.
- **FR-016**: `dry_run: true` MUST return the full merge plan without mutating the graph or writing any WAL record.
- **FR-017**: If the canonical entity does not exist in the graph, return `{ success: false, error: "canonical entity not found" }` immediately.
- **FR-018**: Errors on individual aliases (not found, already merged, etc.) MUST be captured in `errors[]`; processing MUST continue for the remaining aliases.
- **FR-019**: Non-dry-run response: `{ success: bool, canonical_uuid: string, merged_count: int, skipped: int, edges_rewritten: int, edges_deduplicated: int, errors: [string] }`.
- **FR-020**: Dry-run response: same shape as FR-019, plus `plan: { aliases: [{ uuid, name, active_edges: int, duplicate_edges: int }], total_edges_rewritten: int, total_edges_collapsed: int }`.

### Key Entities

- **Canonical entity**: The surviving entity after a merge. All alias edges are rewritten to point to the canonical; its `created_at` is set to the earliest value across the merge set.
- **Alias entity**: An entity being merged into the canonical. After merge, it is marked as merged (invalidated) but not hard-deleted.
- **Merge WAL record**: A durable record of the merge operation (canonical UUID, alias UUIDs, timestamp) enabling WAL replay and recovery.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Merging the production "Brett" cluster (53 nodes): after `knowledge_merge_entities { canonical_name: "Brett", merge_all_by_name: true }`, exactly 1 "Brett" entity remains, 0 orphaned edges, no duplicate directed edges, 52 aliases marked as merged.
- **SC-002**: WAL round-trip: a merge applied to graph G, followed by WAL dump and full replay from scratch, produces graph G' where entity count and edge counts match G post-merge.
- **SC-003**: Regression test: 10 entities with identical names each connected to 3 distinct other nodes → after merge: 1 canonical, up to 30 edges on canonical (minus exact-duplicate collapses), 9 nodes marked as merged.
- **SC-004**: `dry_run` leaves the graph byte-identical (same entity count, same edge count, same UUIDs) before and after the call.
- **SC-005**: Self-merge (canonical UUID in `alias_uuids`) produces no self-loop edges and no error; `merged_count: 0` for the self-referenced UUID.
- **SC-006**: Idempotent re-run: calling the same merge twice produces the same merged graph; second call returns `merged_count: 0`, `skipped: N-1`.
- **SC-007**: Bulk job using one `knowledge_merge_entities` call per cluster across all 1,329 clusters completes without OOM or per-cluster timeout.

## Assumptions

- The `apply_same_as` edge-rewrite and alias-invalidation logic in `crates/core/src/corrections.rs` is production-proven; this feature reuses it rather than reimplementing it.
- `group_id` defaults to `"liminis"`; name-based lookups are always scoped to a single group.
- There is no single-call bulk API — callers (liminis-app or scripts) issue one `knowledge_merge_entities` call per cluster.
- The WAL durability requirement implies a new WAL record type (or extension of an existing corrections record); the exact encoding is an implementation decision for the Plan stage.
- "Earliest `created_at`" for canonical selection is deterministic even with millisecond-identical timestamps (tiebreak: lexicographic UUID order).
- Hard-delete of aliases is NOT acceptable — consistent with the `same_as` / `retract` semantics from Issue #30.
- This method does NOT update `.liminis/knowledge-corrections.yaml` — it operates on the graph and WAL directly. Corrections-file-based merges (via `knowledge_apply_corrections`) remain a separate code path.

## Out of Scope

- A single-call bulk-merge API accepting the full `dedup_candidates.csv` — callers batch-call this method per cluster.
- Undo / reverse-merge — no `unapply_merge` method in this spec.
- Cross-group merges (entities in different `group_id`s).
- Corrections-file integration — merges are WAL-durable but do not write to `.liminis/knowledge-corrections.yaml`.
- A UI for the merge operation (liminis-app's concern).
- Fuzzy / embedding-based alias discovery — callers are responsible for identifying which entities to merge.

## Source References

- `crates/core/src/corrections.rs` — `apply_same_as`, `resolve_canonical` (reuse targets)
- `crates/core/src/db.rs` — `get_entity_by_name`, `get_full_edges_for_entity`, `has_directed_edge`, `invalidate_edge`
- Issue #30 — Tier 3 Corrections: established `same_as` merge semantics, alias-invalidation model, and WAL durability pattern
- `dedup_candidates.csv` — track-1 analysis: 1,329 clusters, 2,230 duplicate nodes (ready merge input)
