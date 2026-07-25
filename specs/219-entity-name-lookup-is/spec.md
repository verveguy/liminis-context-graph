# Feature Specification: Restore an Indexed Access Path for Entity Name Lookup

**Feature Branch**: `fabrik/issue-219`
**Created**: 2026-07-24
**Status**: Draft
**Input**: User description: "Entity name lookup is a full table scan on every ingest — restore an indexed access path (regression vs graphiti)"

## Background

`Conn::get_entity_by_name_ci` (`crates/core/src/db.rs:1020`) resolves an entity by group-scoped, case-insensitive exact name match via the Cypher predicate `lower(e.name) = $lower_name AND e.group_id = $gid`. Because `lower(e.name)` is a scalar-function expression rather than a bare property expression, lbug's filter push-down optimizer cannot route it through any index — every call performs a full `Entity` table scan. `Entity` only carries an FTS index (BM25, stemmed/stopword-filtered) and an HNSW vector index, neither of which answers an exact-equality question.

This function is called from four production sites in `crates/core/src/episode.rs`: Phase B entity dedup (once per extracted entity, per batch), the edge-validation fallback (once per unresolved endpoint), and commit-time endpoint resolution (once per unresolved edge, inside the per-edge loop). Ingest cost is therefore **O(edges × |Entity|)** per episode, and degrades linearly with graph size — directly harming the sustained-ingest workload this engine targets. This was surfaced by field reports #202 / #203 and discussion #207, and amplified (not caused) by the edge-endpoint work in #209.

This is a regression relative to the Python `graphiti` implementation being replaced, which never issues a name-equality query at all — it fetches a bounded candidate set through indexed FTS/HNSW paths and does the exact case-insensitive match in memory. ADR-0029 ("name-first entity resolution") took that in-memory match and pushed it into Cypher, which introduced the unindexed scan. ADR-0029 also asserts that a `name_lower` stored column plus a standard Kuzu `CREATE INDEX` would give O(1) lookups; that remedy is impossible on the current lbug 0.17.0 pin (`CREATE INDEX` on a table that already has a primary key produces a catalog entry with no physical structure, and the index-scan rewrite is PK-only) and would remain impossible on any version for a `lower(col) = $x` predicate specifically, since no functional/expression indexes or collation exist — the lowercased value must be materialized as a column regardless of index availability. Both the implementation and ADR-0029's stated remedy need correcting.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Entity name lookup no longer scans the table during ingest (Priority: P1)

As the ingest pipeline resolves entity names to UUIDs (entity dedup and edge-endpoint resolution), each lookup completes without scanning the `Entity` table or round-tripping to the database, so ingest throughput no longer degrades as the graph grows.

**Why this priority**: this is the reported regression itself — ingest cost is currently O(edges × |Entity|) per episode and dominates at realistic scale (≥100k entities), directly harming the sustained-ingest workload this engine targets.

**Independent Test**: Populate a graph with ≥10k entities, call `get_entity_by_name_ci` with names that do and do not exist, and confirm (a) correct results and (b) no `Entity` table scan occurs per call. Measure and compare ingest cost before and after the change on a ≥10k-entity graph.

**Acceptance Scenarios**:

1. **Given** an `Entity` table with ≥10k rows, **When** `get_entity_by_name_ci` is called with a name that exists (case-insensitively) within a given `group_id`, **Then** the correct UUID is returned without a full `Entity` table scan.
2. **Given** the same setup, **When** `get_entity_by_name_ci` is called with a name that does not exist in that `group_id`, **Then** no match is returned and no full `Entity` table scan occurs.
3. **Given** the fix is in place, **When** the four existing call sites in `crates/core/src/episode.rs` invoke `get_entity_by_name_ci`, **Then** their behavior and logic are unchanged — only the lookup's internal access path changes.

---

### User Story 2 - Lookup results stay correct under a stale or partially-updated internal index (Priority: P1)

As the entity-name lookup is served from an in-process structure maintained alongside the database rather than by querying the database directly, a result it produces is always verified against the database before being trusted, so that staleness in that structure can only ever produce a missed lookup, never a wrong entity.

**Why this priority**: the chosen approach's one real hazard is a missed invalidation site causing silent, incorrect dedup or edge resolution. This behavior is what bounds that risk to "slower" rather than "wrong."

**Independent Test**: Force a deliberately stale entry into the lookup structure (pointing at a UUID that no longer matches the expected entity) and confirm the lookup either returns the correct current result or a miss — never the stale/incorrect one.

**Acceptance Scenarios**:

1. **Given** an internal lookup structure entry that no longer matches the database, **When** `get_entity_by_name_ci` is called for that name, **Then** the call never returns the stale UUID as if it were correct.
2. **Given** every mutation path that can change entity name→UUID mappings (entity insert, entity merge, delete-by-source, delete-episode, clear-all, correction application, rebuild-from-WAL, and startup/crash recovery), **When** each path executes, **Then** subsequent lookups reflect the post-mutation state.

---

### User Story 3 - ADR-0029 reflects the true indexing constraints and the corrected design (Priority: P2)

As a maintainer reading ADR-0029, the documented remedy for the scan problem matches what is actually being implemented and why the previously-stated remedy (a standard Kuzu property index) does not work, so future contributors don't repeat the same mistaken assumption.

**Why this priority**: this is a documentation correction, not a runtime behavior change — it doesn't gate the functional fix, but it's required so the historical record doesn't continue to point future work at an impossible remedy.

**Independent Test**: Read ADR-0029 after the change and confirm it no longer recommends `CREATE INDEX` on a `name_lower` column as a viable remedy, and that a new ADR documents the lookup-structure decision, including its invalidation contract.

**Acceptance Scenarios**:

1. **Given** ADR-0029 currently recommends a standard Kuzu property index as the path to O(1) name lookups, **When** this work is complete, **Then** that recommendation is struck and replaced with an explanation of why it does not work on the current or any lbug version for a `lower()` predicate.
2. **Given** the chosen lookup approach, **When** this work is complete, **Then** a new ADR exists documenting the decision and the specific mutation paths under which the internal lookup structure must be kept coherent with the database.

---

### Edge Cases

- **Name not present anywhere.** Lookup returns no match, exactly as today — this fix changes the access path, not the matching semantics.
- **Multiple entities share the same case-insensitive name within a `group_id`.** The existing determinism (earliest-created wins, UUID as tiebreaker) is preserved exactly.
- **A lookup structure entry becomes stale between a mutation and the next lookup.** The verify-on-hit behavior in User Story 2 guarantees this degrades to a miss, not a wrong answer.
- **Lookup and a concurrent mutation race.** This reopens the same TOCTOU window ADR-0029 already documents (read outside the write lock, mutation inside) — this fix does not need to close that window, only avoid making it worse.
- **Recovery or WAL rebuild after a crash.** The internal lookup structure must be fully repopulated as part of that path before lookups are trusted again.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `get_entity_by_name_ci` MUST return results without performing a full `Entity` table scan or a database round trip when the answer is already known internally.
- **FR-002**: `get_entity_by_name_ci`'s external signature and semantics (group-scoped, case-insensitive exact match, earliest-created-wins determinism with UUID tiebreak) MUST be unchanged, so all four existing call sites continue to work without modification.
- **FR-003**: The four existing call sites in `crates/core/src/episode.rs` MUST NOT have their logic changed by this work — only the implementation behind `get_entity_by_name_ci` changes.
- **FR-004**: The internal lookup structure MUST be populated at service startup and after recovery/WAL rebuild, via the existing single-scan mechanism.
- **FR-005**: The internal lookup structure MUST be kept coherent with the database on every mutation path that can add, change, or remove an entity's name→UUID mapping, at minimum: entity insert, `merge_entities`, `delete_by_source`, `delete_episode`, `clear_all`, `apply_corrections`, and `rebuild_from_wal`.
- **FR-006**: A result served from the internal lookup structure MUST be verified against the database (via the existing UUID primary-key lookup) before being returned; if verification fails, the call MUST behave as a miss rather than returning the unverified result.
- **FR-007**: Existing correctness test suites `cross_episode_dedup.rs`, `dedup_integration.rs`, and `edge_endpoint_resolution.rs` MUST continue to pass with no behavior regressions.
- **FR-008**: A test MUST assert coherence between the internal lookup structure and the database across every mutation path listed in FR-005, plus recovery.
- **FR-009**: ADR-0029 MUST be amended to strike the "standard Kuzu property index" remedy and record why a `lower()` predicate is not indexable on the current or any lbug version.
- **FR-010**: A new ADR MUST document the chosen lookup approach and its invalidation contract (the mutation paths from FR-005).
- **FR-011**: A benchmark or measurement comparing lookup/ingest cost before and after this change, at ≥10k entities, MUST be captured and reported alongside this work.

### Key Entities

- **Entity**: A node in the persisted knowledge graph, scoped to a `group_id`, identified by `name`, `uuid`, and `created_at`; the subject of the name lookup this issue addresses.
- **Entity name lookup**: The group-scoped, case-insensitive, exact-match resolution of an entity name to its UUID, currently implemented as an unindexed database scan and the subject of this fix.
- **Internal lookup structure**: The in-process mechanism (implementation detail, not prescribed by this spec beyond the requirements above) that serves name lookups without a table scan, kept coherent with the database across all mutation paths.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Entity name lookups no longer perform an `Entity` table scan, confirmed by measurement.
- **SC-002**: A measured ingest-cost comparison on a ≥10k-entity graph shows improvement over the current full-scan behavior, with before/after figures reported.
- **SC-003**: Coherence between the internal lookup structure and the database holds across 100% of the tested mutation paths (insert, merge, delete-by-source, delete-episode, clear-all, apply-corrections, rebuild-from-WAL, recovery).
- **SC-004**: A stale or incorrect internal lookup entry never produces a wrong entity result in testing — only a correct result or a miss.
- **SC-005**: `cross_episode_dedup.rs`, `dedup_integration.rs`, and `edge_endpoint_resolution.rs` all pass with no behavior regressions.
- **SC-006**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` all pass.
- **SC-007**: ADR-0029 is amended and a new ADR documenting the lookup-structure decision exists.

## Assumptions

- The service's existing single-writer architecture (one process holding an exclusive write lock over the embedded database) is available to keep the internal lookup structure coherent; this fix does not need to introduce new cross-process coordination.
- Exact-name, case-insensitive matching semantics are correct and are not being revisited by this issue — only the access path is in scope.
- This fix is deliberately implemented against the current lbug 0.17.0 pin and is not blocked on a future lbug upgrade.
- A signature-preserving fix is required specifically so that a later index-backed implementation (once the underlying database supports it) can replace this one as a drop-in without touching call sites again.

## Out of Scope

- Changing the logic of the four existing call sites (`crates/core/src/episode.rs`) — they are correct as-is.
- Upgrading the pinned lbug version, tracked separately as **liminis-context-graph#220**.
- Replacing this fix's internal lookup structure with a materialized column plus secondary index once the lbug upgrade lands — tracked separately as **liminis-context-graph#221**, designed as a drop-in swap behind the unchanged `get_entity_by_name_ci` signature.
- Relation-typing work.
- Closing the pre-existing TOCTOU window between a lookup and a concurrent mutation — this fix does not need to leave that window any worse than it is today, but closing it is not in scope.

## Source References

- **liminis-context-graph#202 / #203**: field reports of ingest cost degrading at scale, which first surfaced this regression.
- **liminis-context-graph#207**: discussion amplifying the field reports above.
- **liminis-context-graph#209**: edge-endpoint resolution work that added additional call sites through the unindexed lookup, amplifying (not causing) this issue's impact.
- **liminis-context-graph#220**: follow-on issue tracking the lbug 0.18.x upgrade, which adds non-PK secondary ART indexes.
- **liminis-context-graph#221**: follow-on issue tracking the drop-in replacement of this fix's internal lookup structure with a materialized column plus secondary ART index, once #220 lands.
- **ADR-0029**: "name-first entity resolution" — corrected by this work.
