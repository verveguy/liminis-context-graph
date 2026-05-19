# Feature Specification: Fix FTS Query Syntax for lbug 0.16.1

**Feature Branch**: `fabrik/issue-14`
**Created**: 2026-05-19
**Status**: Draft
**Input**: Issue #14 — "FTS query syntax is Neo4j-style; lbug parser rejects it"

## Background

`liminis-graph-core` uses Neo4j-style Cypher procedures for full-text search (FTS) index creation and querying:

```cypher
CALL db.index.fulltext.createNodeFullTextIndex('entity_name_fts', ['Entity'], ['name'])
CALL db.index.fulltext.queryNodes('entity_name_fts', '<query>') YIELD node, score ...
```

lbug 0.16.1's Cypher parser rejects this syntax at offset 7 with `Invalid input <CALL db.>: expected rule oC_Statement`. lbug's FTS extension uses its own native Cypher procedures, not the Neo4j `db.index.fulltext.*` namespace.

This failure was previously masked by an earlier bug (the `-rdynamic` dlopen issue, fixed in commit `53aa7b0`). With the dlopen fix in place and `INSTALL`/`LOAD` moved to `Db::open` (commit `4ce9489`), the FTS parse error now surfaces and fails multiple integration tests.

**Constitution gate**: Principle III (LadybugDB Only) mandates using lbug-native syntax directly — no shim or abstraction layer. A `[LDB]`-tagged parity test is required for any change touching the LadybugDB driver layer.

## Affected Code

| File | Location | Description |
|------|----------|-------------|
| `liminis-graph-core/src/schema.rs` | Lines 85, 89 | `create_fts_indexes` — 2 index-creation call sites |
| `liminis-graph-core/src/db.rs` | Lines 367, 386 | `fts_search_entities`, `fts_search_edges` — 2 query call sites |

## Affected Tests

| Test file | Failure reason |
|-----------|---------------|
| `tests/db_dedup.rs` — both `hybrid_dedup_*` tests | FTS syntax rejected at index creation or query time |
| `tests/dedup_integration.rs` | Same |
| `tests/ldb_spike_ipc.rs` | Uses FTS indirectly via hybrid path |

## User Scenarios & Testing *(mandatory)*

### User Story 1 — BM25 FTS Search Returns Ranked Results (Priority: P1)

A caller invokes `fts_search_entities` or `fts_search_edges` against a populated graph. The call succeeds and returns entity UUIDs ranked by BM25 relevance score, descending.

**Why this priority**: This is the core capability broken by the bug. Hybrid dedup (and the hybrid search path more broadly) is inoperable without working FTS queries.

**Independent Test**: Insert a set of entity nodes with distinct names, call `fts_search_entities` with a keyword, and assert results are non-empty and ordered by score descending.

**Acceptance Scenarios**:

1. **Given** a fresh DB with seeded entities, **When** `fts_search_entities` runs with a keyword that matches one or more entities, **Then** matching entities are returned ranked by BM25 score in descending order.
2. **Given** the same DB, **When** `fts_search_edges` runs with a keyword that matches one or more facts, **Then** matching edges are returned ranked by BM25 score in descending order.

---

### User Story 2 — Idempotent Index Creation (Priority: P1)

The `build_indices_and_constraints` initialization function is called at startup. If the DB already has FTS indexes from a prior run, the second call must not error out.

**Why this priority**: Non-idempotent initialization would break every restart of a persistent graph. This is a correctness invariant, not a nice-to-have.

**Independent Test**: Call `build_indices_and_constraints` (or `create_fts_indexes` directly) twice against the same open DB and assert the second call returns `Ok(())`.

**Acceptance Scenarios**:

1. **Given** an initialized DB where FTS indexes already exist, **When** `build_indices_and_constraints` runs again, **Then** no error is returned.

---

### User Story 3 — Hybrid Dedup Tests Pass End-to-End (Priority: P2)

The existing integration tests that exercise the hybrid dedup path (`hybrid_dedup_*`, `dedup_integration`) compile and pass without modification to the test logic.

**Why this priority**: These tests are the parity gate for the LadybugDB driver layer (Principle III). Passing them confirms the fix is complete and no regression has been introduced.

**Independent Test**: Run the test suite; all three affected test files must pass.

**Acceptance Scenarios**:

1. **Given** the hybrid dedup path, **When** the existing `db_dedup` and `dedup_integration` tests run, **Then** they pass with no test-logic changes (only production-code changes are permitted).

---

### Edge Cases

- What happens when an FTS query is issued before the FTS index exists? The caller should receive a clear error (not a panic).
- What happens when the FTS query string is empty or contains only stopwords? The function should return an empty result set without error.
- What if `group_ids` filters eliminate all FTS matches? The function must return an empty `Vec`, not an error.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: FTS index creation MUST use lbug-native Cypher syntax compatible with lbug 0.16.1's parser; Neo4j-style `CALL db.index.fulltext.*` procedures are prohibited.
- **FR-002**: FTS queries MUST use lbug-native Cypher syntax and MUST return `(uuid, score)` pairs with BM25 ranking semantics preserved.
- **FR-003**: FTS index creation MUST be idempotent — calling `create_fts_indexes` against a DB that already has the indexes MUST return `Ok(())`.
- **FR-004**: The `group_id` filter applied after FTS retrieval MUST continue to work as before (filter rows by membership in the provided group ID list).
- **FR-005**: No shim, translation layer, or abstraction over lbug's FTS API is permitted (Constitution Principle III).
- **FR-006**: A `[LDB]`-tagged parity test covering the FTS creation and query path MUST exist or be added (Constitution development-workflow gate).

### Key Entities

- **FTS Index**: A named BM25 full-text index scoped to a node table (Entity or RelatesToNode_) and one or more string properties (name, fact). Two indexes are in use: `entity_name_fts` and `relates_to_fact_fts`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `cargo test` on the `liminis-graph-core` crate passes with zero failures in `db_dedup.rs`, `dedup_integration.rs`, and `ldb_spike_ipc.rs`.
- **SC-002**: `fts_search_entities` and `fts_search_edges` return non-empty, BM25-ranked results when the query matches seeded data.
- **SC-003**: Calling `create_fts_indexes` twice on the same open DB returns `Ok(())` both times.
- **SC-004**: No new abstraction layer or FTS shim is introduced; the diff is confined to translating the 4 call sites to lbug-native syntax.

## Assumptions

- lbug 0.16.1 is the pinned version; no version upgrade is in scope.
- The lbug FTS extension is already installed and loaded at `Db::open` time (per the fix in commit `4ce9489`); this issue covers only the Cypher syntax at the 4 call sites.
- The lbug-native FTS API (confirmed at `docs.ladybugdb.com/extensions/full-text-search/`) exposes `CALL CREATE_FTS_INDEX(table, index_name, [properties])` for creation and `CALL QUERY_FTS_INDEX(table, index_name, query) RETURN node, score` for querying — Research stage should verify against the installed extension version before implementing.
- `group_id` post-filtering logic in `fts_search_entities` and `fts_search_edges` does not change.

## Out of Scope

- Changes to the HNSW vector search path.
- Changes to WAL logic, IPC layer, or LLM/embedding adapters.
- Upgrading lbug beyond 0.16.1.
- Adding new search capabilities beyond what is currently implemented.

## Source References

- `liminis-graph-core/src/schema.rs:82–93` — `create_fts_indexes`
- `liminis-graph-core/src/db.rs:358–394` — `fts_search_entities`, `fts_search_edges`
- `liminis-graph-core/tests/db_dedup.rs`, `tests/dedup_integration.rs`, `tests/ldb_spike_ipc.rs`
- LadybugDB FTS extension docs: `https://docs.ladybugdb.com/extensions/full-text-search/`
- Commits `53aa7b0` (-rdynamic fix) and `4ce9489` (INSTALL/LOAD to Db::open) — prerequisite fixes already merged
