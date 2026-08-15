# Feature Specification: Multi-Stream / Layer-Graph E2E Test

**Feature Branch**: `fabrik/issue-394`
**Created**: 2026-08-13
**Status**: Specified
**Input**: User description: "Port the multi-stream / layer-graph end-to-end harness into the repo as a first-class integration test, so the composition it exercises is protected by CI rather than by an ad-hoc script."

## Background

A Python harness (`multistream_test.py`, repo-external) has been driving a real `liminis-context-graph --mcp-stdio` process by hand and found three defects that the existing unit and integration tests missed:

- **#383** — `applied_seq` never advancing for `wal_flush_ungrouped` writes, visible only by writing a graph through the assertion API and then reading `knowledge_status`. It also surfaced a downstream consequence nobody had predicted: `knowledge_wal_mark_create` fails outright on a null position, making #365 checkpoints unusable for assert-built graphs.
- **#385** — `delete_by_group` and `rebind_pointers` writing other groups' mutations into the default stream, visible only by inspecting which WAL directory received which mutation after a purge.
- **#392** — the `rebind_pointers` staleness gate skipping pointers that are already `unbound`, visible only by running purge → checkpoint restore → rebind in sequence.

Each defect lives in the *composition* of several independently-shipped features — #361 (purge), #365 (checkpoints), #369 (pointers), #378 (per-group streams), #383 (positions), #385 (mutation attribution), #387 (generations) — rather than in any single one of them. No existing single-feature test spans all of these together, so a regression touching their interaction can pass the full suite and land on `main` undetected. This issue ports the ad-hoc harness into the repo as a first-class Rust integration test so that composition is protected by CI going forward.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - CI catches composition-level regressions across purge/checkpoint/pointer/stream features (Priority: P1)

A contributor changes code in one of the multi-group WAL features (purge, checkpoints, pointers, per-group streams, positions, mutation attribution, generations). Today, no single test exercises the composition of these features together, so a regression that only manifests when several of them interact (as #383, #385 and #392 did) can pass the full test suite and reach `main` undetected. With this test in the default suite, `cargo test --release` fails immediately, naming the property that broke and the issue that motivated it.

**Why this priority**: This is the entire purpose of the issue — three real defects (#383, #385, #392) escaped existing unit/integration tests because they live in the composition, not in any single feature.

**Independent Test**: Run the new test as part of `cargo test --release` (default suite, no special flags) against a workspace with #383, #385, or #387 reverted, and confirm at least one named assertion fails.

**Acceptance Scenarios**:

1. **Given** a clean workspace on the current `main`, **When** `cargo test --release` runs, **Then** the new test executes as part of the default suite (no `#[ignore]`), starts a stub embedder and a dead extractor endpoint, and completes in well under 60 seconds.
2. **Given** the nine phases described below are driven against a live `--mcp-stdio` process using only the assertion API, **When** each phase's checks run, **Then** each of the ten assertions evaluates against both the MCP response and the on-disk WAL layout as specified, and a failing assertion's message names the property and the motivating issue number.
3. **Given** the fix for #392 has not yet landed, **When** the phase-9 rebind assertion runs, **Then** it is either `#[ignore]`d with a comment naming #392, or it asserts today's actual (broken) behavior with a `TODO(#392)` comment — the assertion is present and visible either way, never silently omitted.
4. **Given** the fix for #392 lands later and flips the phase-9 assertion, **When** the suite next runs, **Then** the change is a deliberate, visible diff to the test (removing the `#[ignore]`/`TODO` and asserting the corrected behavior), not a silent pass.

---

### Edge Cases

- What happens when the test process is interrupted or panics mid-run? The test must not leak persistent state outside its own temp directory (SC-004) regardless of how it exits.
- What happens if the stub embedder's port collides with another concurrently running test? The harness must bind to an ephemeral (OS-assigned) port for the stub embedder, matching the reference implementation, and the extractor endpoint must be one to which extraction is never actually attempted in this test — no live code path calls it — so a "dead port" is safe regardless of whether anything is listening there.
- What happens to WAL directories for groups not touched by a given operation (e.g. group `B`'s stream during `A`'s purge and restore)? They must be asserted unchanged, per FR-003.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The harness MUST be ported to a Rust integration test under `crates/service/tests/`, reusing the existing `common::McpClient`, `common::spawn_stub_embedder` and `common::binary_path` helpers rather than introducing a second harness or a Python dependency in the test pipeline.
- **FR-002**: It MUST construct all graph content through `knowledge_assert_entity`, `knowledge_assert_relationship` and `knowledge_add_cross_group_edge` — no episode ingest, no extraction, no corpus fixture — so it is deterministic and needs no LLM.
- **FR-003**: It MUST assert on the on-disk WAL layout, not only on API responses: which group subdirectory received which mutations, and that groups not party to an operation are unchanged. This is the property that caught #385 and it is invisible from `knowledge_status` alone.
- **FR-004**: It MUST run in the default `cargo test --release` suite, not `#[ignore]`d. It must complete in seconds and require no fixture, unlike the `mcp_real_corpus_*_e2e` siblings whose `#[ignore]` exists because they seed a 1,506-entity corpus and rebuild indexes.
- **FR-005**: Each assertion MUST fail with a message naming the property and the issue that motivated it (e.g. "cross-group edge mutations must land in the owning group's stream (#385)"), so a future regression is self-describing.
- **FR-006**: The phase-9 rebind assertion (pointers into a purged-and-restored group return to `bound` after `knowledge_rebind_pointers`) is currently expected to fail, per #392. It MUST be included and marked to reflect that — either `#[ignore]` with a comment naming #392, or asserting today's actual behavior with a `TODO(#392)` comment — so that fixing #392 flips it deliberately rather than leaving the gap untested. It MUST NOT be silently dropped.
- **FR-007**: The test MUST drive nine phases against a real `liminis-context-graph --mcp-stdio` process: (1) entities plus an intra-group relationship in groups A and B, (2) the same in group C, created lazily on first write, (3) four cross-group edges from C into A and B (one foreign endpoint each), (4) one C-owned edge with both endpoints foreign (A→B) — the pure layer case, (5) per-group checkpoints via `knowledge_wal_mark_create`/`knowledge_wal_mark_list`, (6) a `knowledge_delete_by_group` dry-run on A, (7) a real purge of A via `knowledge_delete_by_group`, (8) restore of A by replaying its own WAL stream to its pre-purge checkpoint via `knowledge_rebuild_from_wal`, (9) `knowledge_rebind_pointers` for A.
- **FR-008**: All ten assertions from the reference implementation MUST be ported: (a) `applied_seq` advances per group after writes, (b) per-group checkpoint lists do not aggregate across groups, (c) the purge dry-run names the owning layer group in `unbound_impacts`, (d) a purged group's cross-group edges survive (are not deleted), (e) the purged group's pointers into it go `unbound`, (f) the purge does not delete the purged group's own WAL stream, (g) and (h) replaying one group's WAL leaves each of the other two groups' WAL positions byte-identical, (i) the purged group's entities are restored after replay, (j) pointers re-bind after `knowledge_rebind_pointers` (subject to FR-006's #392 caveat).

### Key Entities

- **Group**: An isolated namespace (`group_id`) for entities, relationships, and its own WAL stream. This test uses three: `A` and `B` as source graphs, `C` as a layer graph holding only cross-group edges.
- **Cross-group edge**: A relationship whose source and/or target endpoint resolves into a different group than the edge's own `group_id`, tracked via a resolvable pointer with a `binding_state` (`bound`/`unbound`).
- **WAL stream**: The on-disk, per-group directory of `.jsonl` mutation-log files that this test inspects directly, not only through API responses.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The suite runs in the default `cargo test --release` pass and completes in under 60 seconds on CI.
- **SC-002**: Reverting any one of #383, #385 or #387 causes at least one named assertion to fail.
- **SC-003**: It requires no network, no API key, no corpus fixture, and no LLM.
- **SC-004**: It leaves no state outside its temp directory.

## Assumptions

- The `common::McpClient`, `common::spawn_stub_embedder`, and `common::binary_path` test helpers referenced in FR-001 already exist in `crates/service/tests/common/mod.rs` and expose functionality equivalent to the reference Python harness's `Mcp` class and `spawn_stub_embedder()` (confirmed present in the codebase as of this spec).
- The MCP methods this test depends on (`knowledge_assert_entity`, `knowledge_assert_relationship`, `knowledge_add_cross_group_edge`, `knowledge_status`, `knowledge_wal_mark_create`, `knowledge_wal_mark_list`, `knowledge_delete_by_group`, `knowledge_rebuild_from_wal`, `knowledge_rebuild_status`, `knowledge_rebind_pointers`, `knowledge_get_nodes_by_group`, `knowledge_query_cypher`) already exist in the current codebase (confirmed present as of this spec).
- The reference Python implementation at `~/dev/liminis-project/multistream_test.py` (repo-external, not part of this repository) is authoritative for the exact sequence of calls and assertions; where the Rust port's idiomatic structure differs from the Python original (e.g. helper function shapes, error handling), it is the *behavior and assertions* the harness encodes that must be preserved, not its Python-specific structure.
- The test's expected runtime ("in seconds" per FR-004, "well under 60 seconds" per SC-001) is consistent with the reference implementation's phase count and its lack of corpus seeding or index-build work.

## Out of Scope

- Fixing #392 (the `rebind_pointers` staleness gate bug) itself — this issue only ports the test that documents and exercises the gap. FR-006 explicitly requires the test to reflect current (broken) behavior until #392 is fixed separately.
- Deciding where the WAL-layout inspection helper (walking the WAL root, parsing each line's `params` for `group_id`) ultimately lives — the issue notes it may belong in `crates/service/tests/common/` for reuse by other WAL tests, but that placement decision is left to the Research/Plan/Implement stages as an implementation detail.
- Any new MCP methods, schema changes, or product-facing behavior — this issue is purely test infrastructure.

## Source References

- Reference implementation: `~/dev/liminis-project/multistream_test.py` (repo-external Python harness, 432 lines)
- Sibling tests: `crates/service/tests/mcp_real_corpus_e2e.rs`, `mcp_real_corpus_admin_data_e2e.rs`, `mcp_real_corpus_admin_lifecycle_e2e.rs`, `mcp_real_corpus_mutation_e2e.rs`
- Test helpers: `crates/service/tests/common/mod.rs`
- Motivating issues: #361 (purge), #365 (checkpoints), #369 (pointers), #378 (per-group streams), #379 (assertion API), #383 (positions), #385 (mutation attribution), #387 (generations), #392 (rebind staleness gate)
