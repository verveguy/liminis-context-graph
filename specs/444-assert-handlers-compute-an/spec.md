# Feature Specification: Assert handlers compute embeddings before the existence check

**Feature Branch**: `fabrik/issue-444`
**Created**: 2026-08-23
**Status**: Specified
**Input**: User description: "`handle_assert_entity` computes a name embedding before it knows whether the entity already exists, and throws the result away when it does. `crates/core/src/handlers.rs:3303` — the embed happens before the DB is even opened... `update_entity_core` takes no embedding parameter — by design, since an update deliberately leaves the stored vector untouched. `name_embedding` is consumed only by the `None` (create) branch's `EntityRow`. On update it is computed and discarded. So every assert against an already-known entity pays a full embedder round-trip for a value that is never used. Because these are idempotent upsert APIs, re-asserting a known entity is the expected access pattern, not an edge case."

## Background

`knowledge_assert_entity` and `knowledge_assert_relationship` are idempotent upsert APIs (issue #379): given a `(name, group_id)` or `(source, predicate, target, group_id)` identity key, they create the row if it doesn't exist, or update it in place if it does. Re-asserting an already-known entity or relationship is the expected, common access pattern for these APIs, not an edge case — callers are encouraged to assert liberally rather than check-then-assert.

Both handlers currently compute their vector embedding(s) **before** resolving whether the target row already exists. On the update branch, the embedding is deliberately never stored — `update_entity_core` and `update_relates_to_core` take no embedding parameter by design, because once an HNSW vector index exists over `name_embedding` / `summary_embedding` / `fact_embedding`, a plain `SET` on that column is rejected by the database ("Try delete and then insert"); only the create path (`insert_entity` / `insert_relates_to_edge`, before any index exists over the row) can persist a freshly computed embedding. So on every update, the handler pays a full embedder round-trip for a value that is computed and then discarded.

Since this issue was opened, issue #470 added a second embedded field to `handle_assert_entity` — `summary_embedding` — built with the exact same embed-before-existence-check shape as `name_embedding` (see the comment at `handlers.rs:3549` noting "the same before-existence-check timing — #444 tracks fixing that for both fields together"). This spec covers both fields together, as that comment anticipates.

A third handler flagged in the original issue as structurally similar but unverified, `handle_add_cross_group_edge`, was investigated during specification: `cross_group::create_cross_group_edge` (`crates/core/src/cross_group.rs:200`) always inserts a new edge — it has no existence check and no update branch at all. Its `fact_embedding` is therefore always used, never discarded. **This handler does not have the bug and is confirmed out of scope.**

The fix is a reordering, not a caching change: resolve existence first (the lookup this needs already exists — `get_entity_by_name_ci_with_scan_fallback` / `resolve_entity_by_uuid` for entities, `find_active_relates_to_uuid` for relationships — no new index or scan required), and call the embedder only on the branch that will actually persist the result.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Re-asserting a known entity does not call the embedder (Priority: P1)

A caller repeatedly asserts the same entity name within a group (the normal idempotent-upsert usage pattern). Each re-assertion after the first should update the entity's mutable fields (labels, summary, attributes) without incurring an embedder round-trip for `name_embedding` or `summary_embedding`, since neither value will be persisted.

**Why this priority**: This is the exact waste described in the issue — the common case, not an edge case — and is the primary motivation for the fix.

**Independent Test**: Assert an entity twice with the same name in the same group. Using an embedder call-count instrumentation (mirroring the pattern already used in `crates/core/src/embedder.rs` test helpers), assert that the first call invokes the embedder for `name` (and for `summary` when non-empty) and the second call invokes it zero times, while both calls succeed and the second call's response reflects `created: false`.

**Acceptance Scenarios**:

1. **Given** an entity named "Alice" already exists in group `g1`, **When** `knowledge_assert_entity` is called again with `name: "Alice"`, `group_id: "g1"`, **Then** the response has `created: false`, the entity's `labels`/`summary`/`attributes` are updated per `update_entity_core`, and the embedder is not invoked for either `name` or `summary`.
2. **Given** no entity named "Bob" exists in group `g1`, **When** `knowledge_assert_entity` is called with `name: "Bob"`, `group_id: "g1"`, **Then** the response has `created: true`, a new entity row is inserted with `name_embedding` and `summary_embedding` computed from the supplied `name`/`summary`, and the embedder is invoked exactly once per non-empty field.
3. **Given** `entity_uuid` is supplied and resolves (directly, or by forwarding through a `Merged` tombstone) to an existing entity, **When** `knowledge_assert_entity` is called, **Then** the embedder is not invoked, matching the by-name update case.

---

### User Story 2 - Re-asserting a known relationship does not call the embedder (Priority: P1)

Symmetric to User Story 1, for `knowledge_assert_relationship`: re-asserting the same `(source, predicate, target, group_id)` edge should update `fact`/`valid_at`/`relation_type`/`attributes` via `update_relates_to_core` without calling the embedder for `fact`.

**Why this priority**: Same shape and same motivating waste as User Story 1, on the sibling handler explicitly named in the issue.

**Independent Test**: Assert the same `(source_name, predicate, target_name, group_id)` twice. Confirm the embedder is called once on the first (create) call and zero times on the second (update) call, and that the second call's response reflects `created: false`.

**Acceptance Scenarios**:

1. **Given** an active edge already exists for `(source, predicate, target, group_id)`, **When** `knowledge_assert_relationship` is called again with the same identity, **Then** the response has `created: false`, the edge's `fact`/`valid_at`/`relation_type`/`attributes` are updated per `update_relates_to_core`, and the embedder is not invoked for `fact`.
2. **Given** no such edge exists, **When** `knowledge_assert_relationship` is called, **Then** the response has `created: true`, a new edge is inserted with `fact_embedding` computed from `fact`, and the embedder is invoked exactly once.

---

### Edge Cases

- **Embedder failure on create**: if the embedder call fails while creating a new entity or relationship, the existing zero-vector fallback behavior (a same-dimension zero vector, plus an `embedding_warning` in the response noting a zero vector was stored) must be unchanged.
- **Embedder failure is unreachable on update**: since the embedder is no longer called on the update path, an update can never produce an embedding-related warning. The response must not fabricate one.
- **Empty `summary` on create**: already short-circuits to a zero vector without calling the embedder at all (existing behavior, unrelated to this reordering) — this must continue to hold and must not be misread as "the embedder failed."
- **Rename-collision error path**: `handle_assert_entity` can resolve an existing entity and then reject the call because the caller-supplied `name` collides with a different active entity. This is a variant of the update branch (an existing row was resolved) and must not invoke the embedder, matching the general "existence resolved → no embed" rule.
- **Dangling `Merged` forwarding error path**: both `resolve_entity_by_name`/`resolve_entity_by_uuid` (entities) and endpoint resolution (relationships) can hard-error when a `Merged` tombstone's forwarding chain doesn't reach a canonical row. This is also a resolved-existing-row path and must not invoke the embedder.
- **`handle_add_cross_group_edge` is unaffected**: confirmed to have no existence check or update branch; no behavior change is in scope for it.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `handle_assert_entity` MUST resolve whether the target entity already exists (via `entity_uuid`-based resolution or by-name resolution, including `Merged`-tombstone forwarding) before calling the embedder for `name`.
- **FR-002**: `handle_assert_entity` MUST call the embedder for `name` only on the branch that creates a new entity row.
- **FR-003**: `handle_assert_entity` MUST call the embedder for `summary` only on the branch that creates a new entity row, and only when `summary` is non-empty (preserving the existing empty-summary short circuit unchanged).
- **FR-004**: `handle_assert_relationship` MUST resolve whether an active matching edge already exists (via `find_active_relates_to_uuid`, after resolving both endpoints) before calling the embedder for `fact`.
- **FR-005**: `handle_assert_relationship` MUST call the embedder for `fact` only on the branch that creates a new edge row.
- **FR-006**: On any branch that resolves to an existing row (update, rename-collision error, dangling-`Merged` error), neither handler may call the embedder for any of its embedded fields.
- **FR-007**: On the create branch, the existing embedder-failure fallback (zero vector of `state.embedder.dim()` length, plus an `embedding_warning` noting a zero vector was stored) MUST be preserved unchanged, per field.
- **FR-008**: The response shape for both handlers (`entity_uuid`/`edge_uuid`, `created`, `embedding_warning`) MUST be unchanged — this is an internal reordering, not an API change.
- **FR-009**: `handle_add_cross_group_edge` is explicitly out of scope for behavior changes (see Out of Scope) — this requirement set does not apply to it.

### Key Entities *(if the feature involves data)*

- **Entity** (`EntityRow`): has `name_embedding` and `summary_embedding`, each populated only at creation.
- **RelatesToEdge**: has `fact_embedding`, populated only at creation.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: For a `knowledge_assert_entity` call that resolves to an existing entity (by name or by `entity_uuid`), the embedder is invoked zero times, verified via embedder call-count instrumentation in a test.
- **SC-002**: For a `knowledge_assert_relationship` call that resolves to an existing edge, the embedder is invoked zero times, verified the same way.
- **SC-003**: For calls that create a new entity or edge, embedder invocation count and the resulting stored embeddings/warnings are unchanged from current behavior (regression: existing create-path tests pass without modification).
- **SC-004**: All existing IPC parity and IPC-behavior tests covering `knowledge_assert_entity` / `knowledge_assert_relationship` continue to pass unmodified, confirming no observable response-shape change.

## Assumptions

- The existence-check lookups needed already exist and are sufficient: `get_entity_by_name_ci_with_scan_fallback` / `resolve_entity_by_uuid` (via the in-process `NameIndex`, with scan fallback) for entities, and `find_active_relates_to_uuid` for relationships. No new index, cache, or DB scan is introduced by this work — see the issue's explicit note that this is a reordering, not a caching change.
- Restructuring the async (embedder call) / blocking (DB connection, existence resolution) boundary needed to achieve this ordering is an implementation detail left to the Research/Plan stages, not specified here.
- This work is independent of #440 (WAL-replay embedding recompute) and does not block or depend on #445 (embedder batch API).

## Out of Scope

- `handle_add_cross_group_edge`: verified during specification to have no existence check or update branch (`cross_group::create_cross_group_edge` always inserts), so it is not affected by this bug and no change is made to it.
- A counter/telemetry field tracking how often the update branch is taken (i.e., how often an embed call is now being skipped). The issue's "Not measured" section raises this as worth considering but does not commit to it; deferred to a follow-up issue if wanted (see the existing `name_index_fallback_scans` counter in `crates/core/src/name_index.rs`, surfaced through `knowledge_status`, as a precedent for the pattern).
- #445 (embedder batch API) and #440 (WAL-replay embedding recompute): related but independent work, not part of this fix.

## Source References *(optional)*

- `crates/core/src/handlers.rs:3513` — `handle_assert_entity`
- `crates/core/src/handlers.rs:3669` — `handle_assert_relationship`
- `crates/core/src/handlers.rs:3395` — `handle_add_cross_group_edge` (confirmed out of scope)
- `crates/core/src/db.rs:2472` — `update_entity_core` (no embedding parameter, by design)
- `crates/core/src/db.rs:770` — `update_relates_to_core` (no embedding parameter, by design)
- `crates/core/src/cross_group.rs:200` — `create_cross_group_edge` (always inserts, confirming no discard on that path)
- `crates/core/src/assert.rs:45`, `:71` — `resolve_entity_by_name`, `resolve_entity_by_uuid`
- Issue #379 (original assert API), #470 (added `summary_embedding` with the same timing issue), #445, #440
