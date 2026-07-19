# Feature Specification: `knowledge_rebuild_from_wal` Must Leave Entity/Relationship Search Immediately Queryable

**Feature Branch**: `fabrik/issue-192`
**Created**: 2026-07-16
**Status**: Draft
**Input**: User description: "After `knowledge_rebuild_from_wal` completes successfully, entity/relationship hybrid search (`knowledge_find_entities`, `knowledge_find_relationships`) returns zero results until `knowledge_build_indices` is called manually. A rebuild that reports `success: true` leaves the store in a state that looks empty to the primary search methods, which is a non-obvious and surprising trap for a downstream consumer."

## Background

`knowledge_rebuild_from_wal` is the primary recovery path for a workspace whose database was lost, corrupted, or is being rebuilt from an authoritative WAL. A consumer migrating onto lcg-service (in this case, an external Python knowledge-graph service, "orac", moving off an embedded graphiti engine) reasonably expects that once a rebuild reports success, the graph is ready to serve queries.

In practice, on lcg-service v0.9.0 (prebuilt release, `aarch64-apple-darwin`), replaying a 113-file / 5,565-event WAL completed cleanly — `knowledge_status` correctly showed the replayed counts (233 entities / 336 relationships / 130 episodes), and `mutations_replayed`, `failed_lines`, and `unparseable_lines` all indicated a fully faithful replay. Despite this, `knowledge_find_entities` and `knowledge_find_relationships` returned `[]` for entities known to exist in the graph, until `knowledge_build_indices` was called by hand — at which point the exact same queries returned the expected hits. `knowledge_search_passages` (episode search) worked correctly throughout, which made the entity/relationship search failure more confusing rather than less, since it was not obvious that different search methods depend on different index state.

This is not simply an unimplemented feature. The codebase already has a design intended to solve exactly this problem: **ADR-0025 ("Auto-Heal Index Build and Bulk-Load Reload Pattern")** documents that `handle_rebuild_from_wal` should own the index DDL lifecycle for WAL reload — dropping FTS indexes before replay, then calling `Conn::build_indices_and_constraints` once after replay completes, and states as a consequence: *"Post-reload searches are immediately available — no lazy-build stall on the first query."* A companion auto-heal path in the search handlers (`handle_find_entities`, `handle_find_relationships`) is meant to transparently repair a missing-index state on first search if the post-reload build failed or was interrupted.

The issue, therefore, is that this documented contract is being violated in practice: a rebuild reports `success: true` and correct entity/relationship counts, yet the store behaves as if unindexed, and neither the auto-heal path nor any other signal makes this visible to the caller. The existing regression test for this code path (`test_reload_builds_all_indexes` in `crates/core/tests/handlers_wal_admin.rs`) only asserts that the post-reload `knowledge_find_entities` call succeeds without an IPC error and that the internal `indices_built` flag is set — it does not assert that the returned result set is non-empty or contains the expected entity. This is a plausible gap in test coverage that would let exactly this failure mode through undetected, but confirming the underlying mechanism is Research-stage work, not something this spec prescribes.

Root-causing *why* the existing auto-build/auto-heal mechanism fails silently at production scale (113 files, thousands of mutations, HTTP embedder) is explicitly out of scope for this document — that diagnosis belongs to the Research stage. This spec defines the observable contract that must hold once the fix lands, and the acceptance criteria a fix must satisfy, regardless of where in the pipeline the defect actually lives.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Rebuild Leaves the Store Immediately Queryable (Priority: P1)

An operator or downstream service calls `knowledge_rebuild_from_wal` to recover or rebuild a workspace's graph from its WAL. As soon as the rebuild reports completion (terminal `success: true` for the synchronous/dry-run paths, or `JobStatus::Completed` for the polled background-job path) with a non-dry-run request, entity and relationship hybrid search work correctly without any further action — no separate call to `knowledge_build_indices` is required.

**Why this priority**: This is the core trap described in the issue. A caller who trusts the rebuild's own success signal and immediately queries the graph gets silently wrong (empty) results, with no indication that a required extra step exists. "Rebuild" implies a ready-to-serve store; this is the primary contract the fix must restore.

**Independent Test**: Populate `.lcg/wal/` with a WAL producing a known set of entities and relationships. Call `knowledge_rebuild_from_wal {"from_seq": 0}` (streaming or job-polled to completion), then immediately call `knowledge_find_entities` for the name of an entity known to be in the replayed data, and `knowledge_find_relationships` for a known relationship. Both must return at least one matching result, with no intervening call to `knowledge_build_indices`.

**Acceptance Scenarios**:

1. **Given** a fresh workspace with valid WAL files under `.lcg/wal/`, **When** `knowledge_rebuild_from_wal {"from_seq": 0}` is called (non-dry-run) and polled to a terminal success state, **Then** a subsequent `knowledge_find_entities` query for an entity name present in the replayed data returns that entity in its results.
2. **Given** the same setup, **When** the rebuild completes, **Then** a subsequent `knowledge_find_relationships` query for a relationship present in the replayed data returns that relationship in its results.
3. **Given** the rebuild is invoked via the streaming path (`_progress_token` present), **When** the terminal progress event / response reports success, **Then** the same immediate-queryability guarantee holds (both the streaming and background-job rebuild code paths must satisfy this contract).
4. **Given** the rebuild is invoked via the background-job path (`knowledge_rebuild_status` polling), **When** `knowledge_rebuild_status` reports `JobStatus::Completed`, **Then** the same immediate-queryability guarantee holds.

---

### User Story 2 - Index Build State Is Never Silently Wrong (Priority: P1)

If, for any reason, the post-rebuild index (re)build does not fully succeed (partial failure, a cancelled/interrupted rebuild, or any other cause), this state is surfaced explicitly to the caller — via the rebuild result and via `knowledge_status` — rather than being swallowed. A caller must be able to determine, either from the rebuild response or by checking status, whether entity/relationship search is currently backed by valid indices, without having to attempt a search and interpret an empty result as ambiguous (empty-because-no-data vs. empty-because-unindexed).

**Why this priority**: The issue explicitly calls out that "silently succeeding with unqueryable entity/relationship search is the worst of the three" possible outcomes. Even if User Story 1's guarantee holds in the common case, defense in depth requires that any residual failure mode be observable rather than silent — this was true before this fix (the current non-fatal `eprintln!`-only failure handling is invisible to a downstream consumer of a service binary) and must not remain true after it. This prevents regression to the exact silent-failure trap described in the original report, even in edge cases the primary fix doesn't fully close.

**Independent Test**: Force an index-build failure during a non-dry-run rebuild (e.g., via a fault-injection hook or a scenario that triggers `Conn::build_indices_and_constraints` to error). Assert that the rebuild's result payload and/or a subsequent `knowledge_status` call surfaces an explicit indication that indices are not (fully) built — distinguishable from a normal successful rebuild.

**Acceptance Scenarios**:

1. **Given** a non-dry-run rebuild where the post-replay index build succeeds, **When** the rebuild result is inspected, **Then** it includes an explicit field indicating indices are built and current (e.g., a boolean flag), set to true.
2. **Given** a non-dry-run rebuild where the post-replay index build fails, **When** the rebuild result is inspected, **Then** the same field indicates indices are not built/current, and the rebuild result does not claim an unqualified success that a caller would reasonably read as "fully ready to serve."
3. **Given** any rebuild outcome (success or index-build failure), **When** `knowledge_status` is called afterward, **Then** it reflects the current index-build state without requiring a search attempt to discover it.
4. **Given** the index state is not built/current, **When** `knowledge_find_entities` or `knowledge_find_relationships` is called, **Then** the existing auto-heal behavior still applies (search transparently triggers a build-and-retry) — this scenario documents that the auto-heal path remains the safety net, not the primary contract.

---

### Edge Cases

- **Dry-run rebuild**: `knowledge_rebuild_from_wal {"dry_run": true}` must continue to leave the database and its indices untouched — dry-run is a simulation and must not build or alter indices, matching current behavior.
- **Cancelled/interrupted rebuild** (client disconnect or service shutdown mid-replay, per R9): per existing design (ADR-0025), the index build still runs once over whatever data was loaded before cancellation. The index-build-state signal from User Story 2 must accurately reflect the outcome of that partial build, not silently report full success.
- **Rebuild at production scale**: the reported failure occurred at 113 WAL files / 5,565 mutations with an HTTP embedder (bge-base, dim 768) — a scale well beyond the existing unit-test coverage (which uses 3-mutation WALs). Whatever fix is implemented must be verified to hold at a scale representative of real workloads, not only at trivial scale.
- **Concurrent search during rebuild**: a search request racing an in-flight rebuild is out of scope for this issue's acceptance criteria beyond existing locking behavior (the rebuild path already holds `write_lock` during the mutating phase) — no new concurrency contract is introduced here.
- **`knowledge_search_passages` (episode search)**: already works correctly after a bare rebuild per the original report, and is not affected by this fix — it uses a different code path that does not depend on the FTS/HNSW indices in question. No change to episode search is in scope.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: After a non-dry-run `knowledge_rebuild_from_wal` reaches a terminal success state (synchronous response, terminal streaming progress event, or `JobStatus::Completed` for the background-job path), `knowledge_find_entities` MUST return results for entities present in the replayed data, without requiring any prior call to `knowledge_build_indices`.
- **FR-002**: Under the same condition as FR-001, `knowledge_find_relationships` MUST return results for relationships present in the replayed data, without requiring any prior call to `knowledge_build_indices`.
- **FR-003**: FR-001 and FR-002 MUST hold for all three ways `knowledge_rebuild_from_wal` can be invoked: the non-streaming synchronous path, the streaming (`_progress_token`) path, and the background-job (`knowledge_rebuild_status`-polled) path.
- **FR-004**: The rebuild result payload (returned from the synchronous/streaming call, and from `knowledge_rebuild_status` for the job path) MUST include an explicit field reporting whether entity/relationship search indices are built and current as of that rebuild — distinguishing "indices ready" from "indices not (fully) built" in all cases, including partial/cancelled rebuilds.
- **FR-005**: `knowledge_status` MUST expose the current index-build state, so a caller can determine index readiness without needing to inspect a rebuild result or attempt a search.
- **FR-006**: A rebuild that fails to (re)build indices after replay MUST NOT report an outcome that a caller would reasonably interpret as "fully ready to serve" — replay-success and index-build-success MUST be independently and explicitly observable (this may keep the existing replay `success: true` semantics for the mutation-replay outcome, but must not let index-build failure hide behind it unqualified).
- **FR-007**: Dry-run rebuilds MUST NOT build or alter indices, and MUST NOT report an index-built state that implies otherwise.
- **FR-008**: The existing auto-heal search path (transparent rebuild-and-retry on a missing-index error, per ADR-0025) MUST remain functional as a fallback safety net, regardless of how FR-001/FR-002 are achieved.
- **FR-009**: Documentation (at minimum, the IPC/protocol reference covering `knowledge_rebuild_from_wal`, `knowledge_build_indices`, and `knowledge_status`) MUST state the resulting contract clearly: whether `knowledge_build_indices` is ever required after a successful rebuild in the normal case, and what the index-build-state field(s) mean.

### Key Entities

- **Rebuild result / job result**: The JSON payload returned by `knowledge_rebuild_from_wal` (or polled via `knowledge_rebuild_status`) reporting replay statistics; gains an explicit index-build-state field per FR-004.
- **`knowledge_status` response**: The service's health/state snapshot; gains an explicit index-build-state field per FR-005.
- **Index-build state**: A boolean-or-richer signal distinguishing "entity/relationship search indices are built and reflect the current graph contents" from "not built / stale," surfaced consistently across the rebuild result and `knowledge_status`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Given a WAL producing a non-trivial graph (at minimum, hundreds of mutations across multiple WAL files — representative of the scale in the original report, not just the 3-mutation scale of existing unit tests), a `knowledge_find_entities` query for a known entity name returns that entity immediately after a non-dry-run rebuild reports success, with zero intervening calls to `knowledge_build_indices`.
- **SC-002**: Same as SC-001 for `knowledge_find_relationships`.
- **SC-003**: A test that forces the post-replay index build to fail demonstrates that both the rebuild result and `knowledge_status` reflect the non-ready index state, distinguishable from the all-succeeded case.
- **SC-004**: Existing WAL-replay fidelity guarantees (`mutations_replayed`, `failed_lines`, `unparseable_lines`, `fidelity_warning`) are unchanged by this fix — this issue does not alter replay correctness, only index-build observability and reliability.
- **SC-005**: All pre-commit gates pass: `cargo fmt --all`, `cargo test`, `cargo clippy --release --all-targets -- -D warnings`.

## Assumptions

- **A1**: The intended design already exists (ADR-0025) and is meant to make post-rebuild search immediately available; this issue is treated as a defect in realizing that design (or in its observability), not as a request for a wholly new feature. Determining the precise defect is Research-stage work.
- **A2**: The existing regression test `test_reload_builds_all_indexes` (`crates/core/tests/handlers_wal_admin.rs`), which only checks that a post-reload search call succeeds without error rather than asserting non-empty/expected results, is a plausible coverage gap that let this defect ship undetected. Confirming and closing this gap is expected to be part of the fix's test plan, but the spec does not mandate a specific test implementation.
- **A3**: Keeping the current non-fatal treatment of index-build failure during rebuild (i.e., not failing the whole rebuild op just because index build failed, per ADR-0025's rationale that a successful multi-thousand-mutation replay should not be reported as failed) remains correct; this issue adds an explicit, separate signal for index-build state rather than overturning that design decision.
- **A4**: `knowledge_search_passages` (episode/passage search) is unaffected — it does not depend on the same indices and continues to work as it does today.

## Out of Scope

- Diagnosing and fixing the precise root cause of why the existing auto-build/auto-heal mechanism fails at production scale — that investigation belongs to the Research stage.
- Any change to WAL replay correctness, fidelity accounting, or the WAL file format itself.
- Performance optimization of index building (e.g., making `build_indices_and_constraints` faster) beyond what is needed to make it reliable and observable.
- Changes to `knowledge_search_passages` / episode search behavior.
- General-purpose index staleness tracking unrelated to the rebuild path (e.g., staleness from live incremental writes outside of a rebuild).

## Source References

- `crates/core/src/handlers.rs` — `handle_rebuild_from_wal` (streaming path ~L1327-1508, background-job path ~L1569-1720), `build_indices_once` (~L40-68), `handle_find_entities` / `handle_find_relationships` auto-heal logic (~L462-545), `handle_knowledge_status` (~L247-364)
- `docs/adr/0025-auto-heal-index-build.md` — the existing design this issue's contract must actually deliver on
- `crates/core/tests/handlers_wal_admin.rs` — `test_reload_builds_all_indexes`, `test_interrupted_reload_auto_heals`: existing coverage that does not assert post-reload search returns non-empty/expected results
- `crates/core/tests/auto_heal_index_integration.rs` — existing auto-heal integration tests
- Issue #146 (bulk-load reload pattern) and #58 (original auto-heal) — referenced by ADR-0025 as prior work this issue's fix must not regress
