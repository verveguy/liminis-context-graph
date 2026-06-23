# Feature Specification: Relations Must Consistently Carry a Semantic `relation_type` (Extractor Fix + Additive Backfill; Never Delete Arrow Edges)

**Feature Branch**: `fabrik/issue-175`
**Created**: 2026-06-23
**Status**: Draft
**Input**: User description: "Make every relationship consistently carry a semantic `relation_type`, additively (preserve rich values; never collapse, never delete). Two parts: fix the extractor so new edges always get a clean `relation_type` (going-forward), and a one-time additive backfill for existing edges that lack one. Plus a correction to `canonicalize_relations`' delete path."

## Background

A production workspace analysis revealed that the semantic predicate is **inconsistently placed** across two fields on existing edges:

- **~22,843 edges have a lazy `name` like `"Brett → Seattle"`** but a **populated, meaningful `relation_type`** (`LOCATED_IN`, `PARTICIPATED_IN`, `HAD_COACHING_SESSION_WITH`, `ON_LEAVE_FROM`, `OCCURRED_BEFORE`, …). 94% of arrow-named edges are already correctly typed. The rich semantic content lives in `relation_type` + `fact`.
- **Other edges put the predicate in `name`** (e.g. `ATTENDED`, `WORKS_ON`) with an **empty `relation_type`**.

The graph is not missing semantics — they are split between `name` and `relation_type` depending on when and how the edge was ingested. The **current** extractor still produces the `"source → target"` arrow naming (confirmed in ingestion after the latest rebuild, e.g. `"Brett Adam → PSET Architecture Council"`), so this is **ongoing**, not a legacy-only problem.

The immediate consumer of this graph is an LLM doing semantic/exploratory retrieval. **Rich, consistently placed `relation_type` values are therefore an asset** — the goal is *presence + consistency*, not collapsing to a small controlled set (that remains a separate, deprioritized concern).

**Conflict with #163 — critical correction:**
The #163 spec (`canonicalize_relations`) classified `X → Y` arrow-named edges as co-occurrence noise eligible for deletion (FR-004, FR-005). This classification is **incorrect** based on the track-2 analysis above: 94% of those edges carry rich, real `relation_type` values (real `LOCATED_IN` / `ATTENDED` / coaching relationships). The `canonicalize_relations` delete path MUST be corrected; until fixed, `canonicalize_relations` MUST NOT be run on production.

This issue addresses three orthogonal but related problems:
1. **Going-forward (extractor)**: New edges from future ingestion must always get a populated `relation_type`.
2. **Backfill**: Existing edges with an empty/missing `relation_type` need one derived additively.
3. **Safety gate**: `canonicalize_relations` must stop treating arrow-named edges as deletion candidates.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — New Ingestion Always Produces a Populated `relation_type` (Priority: P1)

After this fix, every edge extracted from a new episode MUST have a non-empty `relation_type` field. The `"source → target"` arrow pattern MUST NOT be the only semantic carrier. The `name` field continues to carry whatever human-readable label the extractor produces; `relation_type` is separately populated with the semantic predicate.

**Why this priority**: This is the going-forward correctness requirement. Without it, every subsequent ingest continues to grow the population of edges with empty `relation_type`, making future backfill passes larger and the inconsistency worse.

**Independent Test**: Ingest a single episode containing multiple entity relationships. Query all edges produced. Assert that every edge has a non-empty `relation_type` value, and that none of those values are themselves `"source → target"` arrow-pattern strings.

**Acceptance Scenarios**:

1. **Given** a fresh episode containing multiple relationships, **When** it is ingested, **Then** every resulting edge has `relation_type` populated (non-null, non-empty).
2. **Given** an ingested episode where the extracted edge `name` would have been `"Brett Adam → PSET Architecture Council"`, **When** that edge is stored, **Then** `relation_type` carries a semantic predicate (e.g. `PARTICIPATED_IN`) separately from the `name`.
3. **Given** an extractor that previously returned an empty or null `relation_type`, **When** the same episode is re-ingested after this fix, **Then** the resulting edge has `relation_type` populated.
4. **Given** multiple edges extracted from the same episode, **When** those edges are stored, **Then** 100% of them have non-empty `relation_type`; no edge silently falls through with an empty value.

---

### User Story 2 — Backfill Dry-Run Reports Missing-`relation_type` Count (Priority: P1)

An operator calls the backfill method with `dry_run: true` and receives an accurate count of edges that would be backfilled, without mutating the graph or writing to the WAL.

**Why this priority**: Operators need to understand scope before committing a potentially long-running write pass on a large graph. The dry-run is the mandatory first step before committing the live pass.

**Independent Test**: On a test graph with a mix of edges (some with populated `relation_type`, some with empty `relation_type`), call the backfill method with `dry_run: true`. Assert: the response reports the correct count of edges with empty `relation_type`; the graph is unchanged after the call; no WAL entries were written.

**Acceptance Scenarios**:

1. **Given** a graph with 100 edges, 40 of which have empty `relation_type`, **When** the backfill is called with `dry_run: true`, **Then** the response reports `{would_backfill: 40, total_edges: 100, dry_run: true}` and no mutations are made.
2. **Given** a dry-run completes, **When** any edge is inspected, **Then** no `relation_type` values have changed.
3. **Given** `dry_run: true` is called, **When** the WAL file is inspected, **Then** no new mutation lines were appended.
4. **Given** a graph where all edges already have `relation_type` populated, **When** `dry_run: true` is called, **Then** the response reports `{would_backfill: 0}`.

---

### User Story 3 — Additive Backfill Fills Missing `relation_type` Values Without Touching Existing Ones (Priority: P1)

An operator runs the backfill method in live mode. For each edge with an empty `relation_type`, a `relation_type` is derived from the `name` field (when informative) or the `fact` field (when `name` is an arrow pattern or otherwise uninformative). Edges that already have a populated `relation_type` are untouched. No edge is deleted. The pass is WAL-durable.

**Why this priority**: This is the core backfill value: making the existing corpus consistently queryable by `relation_type` without losing any data or overwriting rich existing values. It is strictly additive.

**Independent Test**: On a test graph with 10 edges — 5 with populated `relation_type` (e.g. `LOCATED_IN`, `ATTENDED`) and 5 with empty `relation_type` (with meaningful `name` or `fact`) — run the backfill in live mode. Assert: the 5 previously-empty edges now have a non-empty `relation_type` derived from their content; the 5 previously-populated edges are unchanged; no edge was deleted; the WAL contains exactly 5 new mutation entries.

**Acceptance Scenarios**:

1. **Given** an edge with `name = "ATTENDED"` and empty `relation_type`, **When** the backfill runs, **Then** `relation_type` is set to `ATTENDED` (or normalized equivalent) and `name`/`fact` are unchanged.
2. **Given** an edge with `name = "Brett → Seattle"` and empty `relation_type` but `fact = "Brett lives in Seattle"`, **When** the backfill runs, **Then** `relation_type` is derived from the `fact` content and is not itself an arrow-pattern string.
3. **Given** an edge with `relation_type = "LOCATED_IN"` already populated, **When** the backfill runs, **Then** the `relation_type` remains `LOCATED_IN` — unchanged.
4. **Given** the backfill runs on a live graph, **When** the WAL is inspected, **Then** each backfilled edge has a corresponding WAL mutation entry capturing the new `relation_type`.
5. **Given** the backfill runs, **When** any edge is inspected, **Then** no edge has been deleted.
6. **Given** an edge with no usable signal in either `name` or `fact`, **When** the backfill runs, **Then** `relation_type` is set to a defined sentinel value (e.g. `UNCLASSIFIED`) rather than being left empty, and the edge is not deleted.

---

### User Story 4 — Backfill Pass Is Idempotent and WAL Round-Trip–Faithful (Priority: P1)

Running the backfill a second time on an already-backfilled graph produces no additional mutations. After a `knowledge_rebuild_from_wal`, the graph reproduces the same post-backfill `relation_type` values.

**Why this priority**: WAL is the canonical backup. If the backfilled values live only in the live DB and not in the WAL, a rebuild would silently undo all backfill work. Idempotency ensures safe re-runs.

**Acceptance Scenarios**:

1. **Given** the backfill has already run, **When** it is run again, **Then** no new WAL mutations are written (delta = 0).
2. **Given** the backfill ran and WAL mutations were written, **When** the DB is wiped and `knowledge_rebuild_from_wal` replays the WAL, **Then** every edge has the same `relation_type` as the post-backfill state (backfilled values preserved, pre-existing values preserved).

---

### User Story 5 — `canonicalize_relations` Never Deletes Arrow-Named Edges (Priority: P1)

After the safety correction to `canonicalize_relations`, calling it on a graph containing arrow-named edges (`X → Y` pattern) does not delete those edges. Arrow-named edges are treated identically to any other edge: their existing `relation_type` (if populated) is used, and the edge survives.

**Why this priority**: This is a **blocking safety fix**. The existing delete path is actively dangerous: the 22,843 arrow-named edges carry real relationship data (coaching sessions, location membership, project participation). Deleting them would be irreversible data loss. This must be fixed before `canonicalize_relations` can be safely run on production.

**Independent Test**: On a test graph containing 10 arrow-named edges (`X → Y` pattern) with populated `relation_type` values, call `canonicalize_relations`. Assert: all 10 edges still exist after the call; no delete mutations appear in the WAL for those edges.

**Acceptance Scenarios**:

1. **Given** an edge with `name = "Brett Adam → PSET Architecture Council"` and `relation_type = "PARTICIPATED_IN"`, **When** `canonicalize_relations` runs, **Then** the edge is not deleted and `relation_type` remains `PARTICIPATED_IN`.
2. **Given** an edge with `name = "Brett → Seattle"` and a populated `relation_type`, **When** `canonicalize_relations` runs, **Then** the edge is not deleted.
3. **Given** an edge with `name = "Brett → Raji"` and an **empty** `relation_type`, **When** `canonicalize_relations` runs, **Then** the edge is still not deleted; it is treated like any other edge with empty `relation_type` (mapped, marked `UNCLASSIFIED`, or left for the backfill pass).
4. **Given** `canonicalize_relations` is run in `dry_run: true` mode, **When** the response is inspected, **Then** no arrow-named edges appear in the noise/deletion count.

---

### Edge Cases

- **Edge has empty `name` AND empty `fact`**: The backfill has no signal to derive `relation_type` from. MUST set `relation_type` to the sentinel value `UNCLASSIFIED` (not skip silently).
- **Edge `name` is an arrow pattern (`X → Y`) but `relation_type` is already populated**: Backfill leaves the edge untouched — populated `relation_type` is never overwritten.
- **Edge `name` is a SCREAMING_SNAKE_CASE predicate** (e.g. `ATTENDED`): Backfill normalizes the name and uses it as the `relation_type`.
- **Arrow pattern in `name` where both sides are common words, not entity names**: No special handling needed — arrow-named edges are NOT treated as noise regardless.
- **Backfill runs while concurrent ingestion is in progress**: The exact locking model is left to Research/Plan, but the pass MUST NOT corrupt edges being concurrently written.
- **Extractor returns an empty `relation_type` from the LLM**: The extractor MUST fall back to deriving a value from available content (prompt or post-processing), never storing empty.
- **`canonicalize_relations` called after backfill**: Arrow-named edges now have populated `relation_type` from backfill — they should flow through the canonicalization mapping logic unchanged (their `relation_type` is the valid predicate).
- **Large graphs (>100K edges)**: The backfill MUST support progress reporting via `_progress_token` streaming, consistent with existing admin methods.

## Requirements *(mandatory)*

### Functional Requirements

**Part 1 — Extractor fix (going-forward)**

- **FR-001**: The edge extractor (`crates/core/src/extractor.rs`) MUST always produce a non-empty `relation_type` on every extracted edge. If the LLM response does not include `relation_type` or returns it as empty, the extractor MUST derive a value from available content (e.g. from the `fact` or the predicate inferred from the episode text) rather than storing an empty value.
- **FR-002**: The `"source → target"` arrow-pattern string MUST NOT be used as the `relation_type` value on any extracted edge. If the extractor previously placed an arrow-pattern string in `relation_type`, it MUST be corrected.
- **FR-003**: The `name` field continues to hold whatever human-readable label the extractor produces (no constraint imposed); `relation_type` is the authoritative semantic field and is always separately populated.
- **FR-004**: A regression test MUST assert that after ingesting an episode, 100% of resulting edges have a non-empty `relation_type`, and that none of those values match the `X → Y` arrow pattern.

**Part 2 — Additive backfill IPC method**

- **FR-005**: A new IPC method (to be named by Research/Plan; suggested: `knowledge_backfill_relation_types`) MUST accept at minimum a `dry_run: bool` parameter.
- **FR-006**: In `dry_run: true` mode, the method MUST return a report including at minimum `{would_backfill: usize, total_edges: usize, dry_run: true}` and MUST make zero mutations to the graph or WAL.
- **FR-007**: In live mode (`dry_run: false`), the method MUST iterate over all edges with an empty or null `relation_type` and set a derived value on each. It MUST NOT touch edges where `relation_type` is already non-empty.
- **FR-008**: The derivation logic MUST use the `name` field when it is a recognizable predicate (e.g. a SCREAMING_SNAKE_CASE token, a normalized verb phrase), and fall back to the `fact` field when the `name` is uninformative (e.g. matches the `X → Y` arrow pattern or is otherwise empty/whitespace).
- **FR-009**: When neither `name` nor `fact` provides a usable signal, the method MUST set `relation_type` to the sentinel value `UNCLASSIFIED`. No edge is left with an empty `relation_type` after a live pass.
- **FR-010**: The backfill MUST NOT delete any edge, regardless of its `name` pattern.
- **FR-011**: The backfill MUST NOT modify the `name` or `fact` fields on any edge.
- **FR-012**: Every `relation_type` update performed by the live pass MUST be written as a mutation to the application WAL, so that `knowledge_rebuild_from_wal` reproduces the post-backfill state.
- **FR-013**: The backfill MUST be idempotent: a second run on an already-backfilled graph produces zero additional WAL mutations.
- **FR-014**: The method MUST support `_progress_token` streaming for large graphs, consistent with existing admin methods.
- **FR-015**: A parity test entry MUST be added to `crates/core/tests/ipc_parity.rs` for the new backfill IPC method.

**Part 3 — `canonicalize_relations` safety correction**

- **FR-016**: The `canonicalize_relations` implementation (`crates/core/src/canonicalize.rs`) MUST NOT classify edges as noise-for-deletion based on their `name` matching the `X → Y` arrow pattern.
- **FR-017**: Arrow-named edges MUST flow through `canonicalize_relations` the same as any other edge: their existing `relation_type` (if populated) is used as the predicate input for mapping, and the edge is never deleted by this logic.
- **FR-018**: If `canonicalize_relations` previously had a deletion path triggered by the `X → Y` pattern, that path MUST be removed or guarded such that it cannot trigger on arrow-named edges.
- **FR-019**: A regression test MUST assert that calling `canonicalize_relations` on a graph containing arrow-named edges (with or without populated `relation_type`) results in no deletions of those edges.

### Key Entities

- **`relation_type`**: A SCREAMING_SNAKE_CASE string representing the semantic predicate of a relationship edge (e.g. `LOCATED_IN`, `PARTICIPATED_IN`, `ATTENDED`). Added by #94; this issue ensures it is always populated. Distinct from `name` (human-readable label) and `fact` (natural-language paraphrase).
- **Arrow-named edge**: An edge whose `name` field matches the pattern `<TOKENS> → <TOKENS>` (entity-pair arrow notation produced by an older extractor path). These edges frequently carry rich, valid `relation_type` values and MUST NOT be treated as deletion candidates.
- **Backfill sentinel**: The string `UNCLASSIFIED`, used as the `relation_type` on edges where no informative signal exists in either `name` or `fact`. Consistent with #163's residual marking convention.
- **Additive pass**: A pass that only writes to edges with empty `relation_type`; never overwrites populated values; never deletes.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After the extractor fix (Part 1), ingesting any episode results in 100% of its extracted edges having a non-empty `relation_type` — measurable by querying edges immediately after ingestion and counting nulls.
- **SC-002**: After the live backfill pass (Part 2), the count of edges with empty `relation_type` in the workspace is ≈ 0 (within the expected `UNCLASSIFIED` sentinel population for edges with no usable signal).
- **SC-003**: All pre-existing `relation_type` values that were non-empty before the backfill are unchanged after the backfill — verifiable by comparing a pre/post snapshot.
- **SC-004**: No edges are deleted by either the backfill pass or the corrected `canonicalize_relations` — edge count is non-decreasing after these operations.
- **SC-005**: WAL round-trip: after a live backfill, wiping the DB and replaying the WAL via `knowledge_rebuild_from_wal` reproduces identical `relation_type` values on every edge (backfilled values survive replay).
- **SC-006**: `dry_run: true` call on a test graph with N edges missing `relation_type` reports `would_backfill = N`, makes no mutations, and writes no WAL entries.
- **SC-007**: Calling `canonicalize_relations` on a graph containing arrow-named edges results in zero deletions of arrow-named edges.
- **SC-008**: All existing tests pass without regression; new tests for the extractor fix (FR-004), backfill parity (FR-015), backfill idempotency (FR-013), and canonicalize_relations safety (FR-019) pass.

## Assumptions

- **A1**: The `relation_type` column was added by #94 and is present in the lbug schema. No schema migration is needed by this issue.
- **A2**: An edge with empty-string `relation_type` is treated equivalently to one with null `relation_type` throughout (both count as "missing").
- **A3**: The backfill is a one-time admin pass plus a going-forward extractor fix; it does not auto-run on every ingest.
- **A4**: The derivation algorithm for the backfill (how exactly to extract a predicate from `name` or `fact`) is left to Research/Plan. The spec constrains the observable behavior (never delete, never overwrite, sentinel for no-signal) but not the derivation mechanism.
- **A5**: `UNCLASSIFIED` is the correct sentinel for edges with no derivable `relation_type`, consistent with #163's residual-marking convention.
- **A6**: The backfill processes all edges in the workspace (no per-group or per-episode scoping in v1).
- **A7**: The corrected `canonicalize_relations` need only remove the arrow-pattern–triggers–deletion path; all other #163 behavior (lexical mapping, embedding fallback, `UNCLASSIFIED` marking for non-arrow residual) is orthogonal and may remain.
- **A8**: Concurrent safety (locking during the backfill pass) is addressed by Research/Plan; the exact locking model is not prescribed here.

## Out of Scope

- Collapsing `relation_type` values to a controlled canonical vocabulary (that is the separate, deprioritized #163 / `canonicalize_relations` feature; this issue is purely additive and does not restrict what `relation_type` values are valid).
- Deleting any edges (this issue explicitly prohibits deletion).
- Backfilling or modifying the `name` or `fact` fields.
- Filtering the backfill pass to a subset of edges (by group, episode, date range) — can be a follow-up.
- Auto-running backfill on every new ingest.
- Cleaning up previously-created duplicate edges or merging semantically equivalent edges.
- Changing the LLM prompt model or model parameters (only the extractor's post-processing behavior is in scope).

## Source References

- **Issue #94** (merged): Added `relation_type` column and established SCREAMING_SNAKE_CASE normalization. This issue extends #94's guarantee to the extractor and to existing edges.
- **Issue #163** (canonicalize_relations): Introduced the incorrect noise-deletion path that this issue corrects. The #163 spec FR-004/FR-005 (delete `X → Y` pattern edges as noise) is superseded by this issue's FR-016–FR-019.
- **Issue #164** (cross-episode entity resolution): The relation-side analog (entity-side was fixed there; relation-side is fixed here).
- `crates/core/src/extractor.rs` — edge extraction and `relation_type` assignment (Part 1 target)
- `crates/core/src/canonicalize.rs` — the delete-noise path (Part 3 target)
- `crates/core/src/db.rs` — edge insert/update (consulted for WAL write patterns)
- `crates/core/src/handlers.rs` — new backfill IPC method + existing method patterns for `dry_run` and `_progress_token` (Part 2 target)
- `crates/core/tests/ipc_parity.rs` — IPC parity tests (new backfill method entry required)
