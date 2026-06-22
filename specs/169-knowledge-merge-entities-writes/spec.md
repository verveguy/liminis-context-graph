# Feature Specification: Fix knowledge_merge_entities TIMESTAMP Coercion Bug

**Feature Branch**: `fabrik/issue-169`
**Created**: 2026-06-22
**Status**: Draft
**Input**: User description: "knowledge_merge_entities writes STRING into TIMESTAMP columns (no-op merges); coerce like WAL replay"

## Background

`knowledge_merge_entities` (introduced in #162) is non-functional on real TIMESTAMP-schema graphs. When it re-creates rewritten edges on the canonical node, it passes `valid_at`, `created_at`, and `invalid_at` as **STRING** params into `TIMESTAMP` columns. lbug rejects every insert with:

```
Binder exception: Expression $valid_at has data type STRING but expected TIMESTAMP.
Implicit cast is not supported.
```

This was confirmed on a 13,400-node production graph: a dry-run-validated merge of 64 "Brett"/"Brett Adam" nodes with 7,401 edges to rewrite applied as a complete **no-op** — `merged_count: 0`, 0 nodes merged, service log spammed with the above error.

The `(non-fatal, Python-schema DB?)` diagnostic logged by `insert_relates_to_edge` is a **misdiagnosis**. The schema is canonical TIMESTAMP (matches `schema.rs`); the bug is a missing coercion in the merge edge-recreation path, not a schema variant.

The WAL-replay path already handles this correctly: `crates/core/src/replay.rs` `json_to_cypher_literal` emits RFC-3339 `TIMESTAMP` literals (fixed in #130). The merge path (`corrections.rs` `merge_entities` → `db.rs` `insert_relates_to_edge`) does not reuse that coercion and therefore diverges and fails. The existing tests for `knowledge_merge_entities` passed because they exercised a path or schema that bypassed the typed timestamp columns.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Merge Entities on a TIMESTAMP-Schema Graph Succeeds (Priority: P1)

A user with a production TIMESTAMP-schema graph calls `knowledge_merge_entities` to collapse duplicate entity nodes. The merge applies correctly: nodes collapse, edges are rewritten to the canonical, and `merged_count` is greater than zero. No timestamp type errors appear in the service log.

**Why this priority**: The feature is completely non-functional on all real production graphs (all of which use the canonical TIMESTAMP schema). This is a P1 regression from #162.

**Independent Test**: Seed a test graph with 3 entities sharing a name, each connected by edges that carry `valid_at` and `created_at` TIMESTAMP values. Call `knowledge_merge_entities { canonical_name: "<name>", merge_all_by_name: true }`. Assert: `merged_count: 2`, exactly 1 entity with that name remains, the rewritten edges are readable, and no `STRING but expected TIMESTAMP` errors appear in the logs.

**Acceptance Scenarios**:

1. **Given** a TIMESTAMP-schema graph with N entities sharing a name and edges carrying `valid_at`/`created_at` timestamps, **When** `knowledge_merge_entities { canonical_name, merge_all_by_name: true }` is called, **Then** `success: true`, `merged_count: N-1`, `edges_rewritten > 0`, and 0 `STRING but expected TIMESTAMP` binder errors occur.
2. **Given** the same call on a graph where only 1 entity has the specified name, **When** called, **Then** `success: true`, `merged_count: 0`, no mutations, no errors.
3. **Given** edges carrying `invalid_at` (retracted/invalidated edges), **When** a merge is applied, **Then** those edges are NOT rewritten to the canonical (only active edges are moved), with no TIMESTAMP errors on the invalidated-edge handling path.

---

### User Story 2 — Misleading "Python-schema DB?" Diagnostic Removed (Priority: P2)

A user running `knowledge_merge_entities` on a canonical TIMESTAMP-schema DB no longer sees `(non-fatal, Python-schema DB?)` in the service log. The diagnostic either correctly identifies the actual error type, or the dead fallback branch is removed entirely.

**Why this priority**: The misleading log led to a wrong hypothesis about the schema type, masking the real coercion bug. It should not mislead future debugging. Lower priority than the functional fix because it does not affect correctness.

**Independent Test**: Run `knowledge_merge_entities` on a canonical TIMESTAMP-schema DB and inspect logs. Assert that no log line containing `Python-schema DB?` is emitted.

**Acceptance Scenarios**:

1. **Given** a canonical TIMESTAMP-schema DB, **When** `knowledge_merge_entities` is called (successfully or with an edge-level error), **Then** no `Python-schema DB?` text appears in any log line.
2. **Given** the `insert_relates_to_edge` fallback branch is dead code (single-schema deployment), **When** the fix is applied, **Then** the fallback branch is removed rather than merely silenced.

---

### User Story 3 — Regression Test Covers TIMESTAMP-Schema Edge Recreation (Priority: P1)

A regression test that exercises edge recreation with TIMESTAMP columns is added, failing before the fix and passing after. The existing test suite gap (tests passed despite the bug) is closed.

**Why this priority**: The existing tests passed through the bug undetected. Without a targeted regression test, the same class of bug can recur unnoticed.

**Independent Test**: Add an integration test that creates entities with edges carrying explicit `valid_at`/`created_at` TIMESTAMP values in the canonical schema, merges them via `knowledge_merge_entities`, and asserts zero timestamp-type errors and `merged_count > 0`. The test MUST fail against the unfixed code and pass after the fix.

**Acceptance Scenarios**:

1. **Given** the regression test fixture with TIMESTAMP-carrying edges, **When** run against the unfixed code, **Then** the test fails (detects the coercion bug).
2. **Given** the same fixture run against the fixed code, **Then** the test passes: `merged_count > 0`, rewritten edges have correct TIMESTAMP values, no binder errors.
3. **Given** the fixed code, **When** `cargo test` runs in CI, **Then** the new regression test is included and passes.

---

### Edge Cases

- Edges with `invalid_at` set (invalidated/retracted) MUST NOT be rewritten to the canonical — only the timestamp coercion path matters for active edges.
- `null` / absent timestamp fields: if `valid_at`, `created_at`, or `invalid_at` is `null` or absent in the edge data, the coercion path MUST handle this gracefully (pass `null`, not a malformed TIMESTAMP literal).
- `dry_run: true` calls MUST also not trigger TIMESTAMP errors — if the dry-run path calls any insert code that touches timestamp columns, those paths must be coerced correctly or bypassed cleanly.
- The fix MUST NOT break the WAL replay path in `replay.rs` — both paths should converge on the same coercion helper, not diverge again.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The edge-recreation path in `knowledge_merge_entities` (`corrections.rs` `merge_entities` → `db.rs` `insert_relates_to_edge`) MUST coerce `valid_at`, `created_at`, and `invalid_at` values to TIMESTAMP literals before passing them to lbug, using the same coercion logic as `json_to_cypher_literal` in `replay.rs`.
- **FR-002**: The timestamp coercion logic MUST be extracted into a **shared helper** accessible from both `replay.rs` and the merge edge-recreation path, so the two paths cannot diverge again. Duplicating the coercion logic in a second location is not acceptable.
- **FR-003**: The shared coercion helper MUST handle `null` / absent timestamp values gracefully — it MUST NOT emit an invalid TIMESTAMP literal for absent fields.
- **FR-004**: The misleading `(non-fatal, Python-schema DB?)` diagnostic in `insert_relates_to_edge` MUST be removed. If the "Python-schema" fallback branch is dead code (no longer reachable in a single-schema TIMESTAMP deployment), the entire branch MUST be removed rather than merely re-labelled.
- **FR-005**: A regression integration test MUST be added that: (a) constructs entities with edges carrying explicit TIMESTAMP-valued `valid_at` and `created_at` fields, (b) calls `knowledge_merge_entities` against the canonical TIMESTAMP schema, (c) asserts `merged_count > 0` and zero `STRING but expected TIMESTAMP` binder errors, (d) verifies rewritten edges are readable with correct TIMESTAMP values.
- **FR-006**: The fix MUST NOT change any IPC protocol shape — `knowledge_merge_entities` request/response fields are unchanged.
- **FR-007**: All existing `knowledge_merge_entities` tests MUST continue to pass after the fix.
- **FR-008**: `cargo fmt --all`, `cargo test`, and `cargo clippy --release --all-targets -- -D warnings` MUST all pass on the PR branch.

### Key Entities

- **`json_to_cypher_literal`** (`replay.rs`): The existing timestamp coercion function — the source of truth for RFC-3339 → TIMESTAMP literal conversion. Must be refactored into a shared helper.
- **`insert_relates_to_edge`** (`db.rs`): The failing direct-insert function. Currently lacks TIMESTAMP coercion and contains the misleading fallback branch.
- **`merge_entities`** (`corrections.rs`): The entry point for the merge edge-recreation path. Calls `insert_relates_to_edge` with raw string-typed params.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On a TIMESTAMP-schema graph, `knowledge_merge_entities` applied to N identical-name entities returns `merged_count: N-1 > 0`, the canonical retains all rewritten edges, and the service log contains **zero** `STRING but expected TIMESTAMP` errors.
- **SC-002**: The `(non-fatal, Python-schema DB?)` log message does NOT appear in any run against a canonical TIMESTAMP-schema DB.
- **SC-003**: The regression test (FR-005) fails against the unfixed code and passes after the fix.
- **SC-004**: The production scenario — collapsing 64 "Brett"/"Brett Adam" nodes with 7,401 edges — completes with `merged_count > 0` and no timestamp binder errors (validated against the production graph or equivalent fixture).
- **SC-005**: `cargo test` (including the new regression test), `cargo clippy --release --all-targets -- -D warnings`, and `cargo fmt --all` all pass on the PR branch.

## Assumptions

- All production deployments use the canonical TIMESTAMP schema (as confirmed by schema introspection in the bug report). A legacy "Python-schema" variant with STRING timestamp columns is no longer in use.
- The `json_to_cypher_literal` function in `replay.rs` is the correct and proven coercion for RFC-3339 → lbug TIMESTAMP literals. Extracting it as a shared helper requires no behavioral change to its output.
- `null` / absent timestamp fields are a valid state for some edges (e.g., edges without an `invalid_at` before they're retracted). The coercion helper must pass `null` through unchanged, not fabricate a timestamp value.
- The `dry_run` path in `merge_entities` does not call `insert_relates_to_edge` — if it does, that path must also be covered.

## Out of Scope

- Fixing other TIMESTAMP coercion gaps outside of the `knowledge_merge_entities` edge-recreation path (e.g., in other direct-write methods). Those would be separate issues if discovered.
- WAL format changes or WAL writer type hints — this fix is purely at the read/insert path, not the WAL write path.
- Undo / reverse-merge capability.
- Performance improvements to the merge operation.

## Source References

- `crates/core/src/handlers.rs` — `handle_merge_entities` (IPC dispatch)
- `crates/core/src/corrections.rs` — `merge_entities` (merge orchestration, edge recreation entry point)
- `crates/core/src/db.rs` — `insert_relates_to_edge` (failing direct insert + misleading fallback branch)
- `crates/core/src/replay.rs` — `json_to_cypher_literal` (correct coercion to extract as shared helper)
- Issue #162 — `knowledge_merge_entities` feature (introduced the regression)
- Issue #130 — WAL replay TIMESTAMP fix (established `json_to_cypher_literal` as the coercion source of truth)
