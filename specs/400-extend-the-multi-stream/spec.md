# Feature Specification: Extend the Multi-Stream E2E Test to Cover Generation Reset, Cross-Group Merge and Ambiguous Resolution

**Feature Branch**: `fabrik/issue-400`
**Created**: 2026-08-15
**Status**: Specified
**Input**: User description: "Extend `crates/service/tests/mcp_multistream_e2e.rs` with three phases covering compositions the layered-graph model depends on but which are only tested in-process today: stream generation reset (#387), cross-group merge (#368/#371), and ambiguous endpoint resolution (#369)."

## Background

`crates/service/tests/mcp_multistream_e2e.rs` (added by #394, merged as PR #395) is the repo's only test that drives a live `--mcp-stdio` process across #361 + #365 + #369 + #378 + #383 + #385 in a single composition, asserting on the **on-disk WAL layout** rather than only on API responses. It runs in ~4s with a stub embedder and no LLM, and is not `#[ignore]`d.

Its track record is the reason this issue exists: **every one of #383, #385 and #392 passed its own unit tests and failed only here.** Each lived in the composition of features rather than in any one feature.

Three more behaviors that the layered-graph model depends on are **not untested** — each has substantial coverage at the `crates/core` integration level against a real database — but none of them has ever been exercised in the composition this test protects: a live layer graph, per-group WAL streams, and a real process boundary, all at once.

| behavior | existing `crates/core` coverage |
|---|---|
| generation reset / self-heal (#387) | `crates/core/tests/wal_generation_reset.rs` (~35KB; 11 pointer/rebind references) |
| cross-group merge (#368/#371) | `crates/core/tests/merge_entities.rs` (~42KB; 143 group/foreign references), `cross_group_pointers.rs` (52 merge references) |
| ambiguous binding (#369) | `crates/core/tests/cross_group_pointers.rs` (16 ambiguous references) |

The composition is precisely the seam where the three known defects (#383, #385, #392) were found, so this gap is a real one even though no defect is currently suspected of hiding in it — closing it is preventative, not a bug fix. Milestone **0.14.0** deliberately: nothing here is known to be broken and no consumer is blocked.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Upstream stream generation reset self-heals a layer graph's pointers (Priority: P1)

A contributor changes code in the WAL generation-identity or self-heal path (#387). Today, no test at the process boundary republishes a source group's stream under a new generation while a live layer graph holds pointers into it, so a regression in that self-heal path — pointers left stranded on a stale generation, or a foreign group's stream disturbed by a reset that should be scoped to one group — can pass the full suite and reach `main` undetected.

**Why this priority**: This is one of three named compositions this issue exists to protect (SC-002), each guarding a defect class the existing nine-phase test cannot reach because it never republishes a stream under a new generation.

**Independent Test**: Revert #387's generation-mismatch detection or self-heal handling, run `cargo test --release`, and confirm the named assertion for this phase fails.

**Acceptance Scenarios**:

1. **Given** group A has published content and group C holds a cross-group pointer that is `bound` into one of A's entities by `endpoint_name`, **When** A's WAL stream is republished under a new `.wal-generation.json` identity with republished content (new entity identities, not merely additional lines appended to the existing generation), **Then** the reset self-heal path runs and C's pointer into A ends `bound` to A's newly republished entity (resolved by name, not by the stale UUID).
2. **Given** the same reset, **When** it completes, **Then** group B's WAL stream, its WAL position, and its generation are all byte-identical/unchanged — the reset is scoped to group A alone.

---

### User Story 2 - A merge in one group never rewrites another group's edges, and a layer's pointer follows the merge forward (Priority: P1)

A contributor changes code in entity merge or in `merged_into` forwarding (#368/#371). Today, no test at the process boundary merges two entities in a source group while a live layer graph holds a cross-group edge into the entity that loses the merge, so a regression that lets a merge in one group write into another group's WAL stream — the exact defect class ADR-0371 closed — can pass the full suite and reach `main` undetected.

**Why this priority**: Guards the rule ADR-0371 states as the governing invariant of the whole multi-stream model: a write in group G touches only G's data. A regression here is a cross-stream data-integrity break, not a cosmetic one.

**Independent Test**: Revert #371's foreign-group merge skip (restore the old rewrite-foreign-edges-eagerly behavior), run `cargo test --release`, and confirm the named assertion for this phase fails.

**Acceptance Scenarios**:

1. **Given** group A holds two entities and group C holds a cross-group edge whose endpoint targets the one that is about to lose a merge (become the tombstoned alias), and C's pointer is currently `bound`, **When** A merges the two entities, **Then** no edge owned by C, and no edge owned by B, is rewritten or deleted as a result — the merge's mutations land only in A's own WAL stream, never C's or B's.
2. **Given** the same merge, **When** C's pointer is subsequently re-examined (via `knowledge_rebind_pointers` or equivalent re-resolution), **Then** it follows the `merged_into` forwarding chain recorded on the tombstoned alias and ends `bound` to the surviving canonical entity, not left dangling on the tombstone.

---

### User Story 3 - An ambiguous endpoint name is reported as ambiguous, never silently bound to a winner (Priority: P1)

A contributor changes code in cross-group endpoint resolution or ambiguity detection (#369). Today, no pointer in the existing nine-phase test is ever ambiguous — every `endpoint_name` it uses resolves to exactly zero or one entity — so a regression that makes ambiguous resolution silently pick an arbitrary winner (the exact failure ADR-0369 was written to prevent) can pass the full suite and reach `main` undetected. This is also the half of #392's FR-001 the current suite structurally cannot reach.

**Why this priority**: `ambiguous` is a third state, distinct from `bound`/`unbound`, that the current end-to-end suite never produces — so any handler path that implicitly assumes a pointer is one of only two states is presently unexercised at the process level.

**Independent Test**: Revert #369's ambiguity detection (restore the silent-winner behavior), run `cargo test --release`, and confirm the named assertion for this phase fails.

**Acceptance Scenarios**:

1. **Given** group A contains two entities that share one `endpoint_name`, **When** group C asserts (or already holds) a cross-group edge whose endpoint targets that shared name, **Then** the pointer's `binding_state` is `ambiguous`, not silently bound to either candidate.
2. **Given** that ambiguous pointer, **When** `knowledge_rebind_pointers` is subsequently called, **Then** it re-examines the pointer (attempts resolution again) rather than skipping it as already resolved.

---

### Edge Cases

- **Merge-phase ordering**: the merge phase (User Story 2) MUST run while C's pointers into A are still `bound` — i.e., before the existing purge/restore/rebind sequence (the current test's phases 6-9). As of this issue's base commit, the existing phase 9 documents C's pointers into A as remaining `unbound` indefinitely after that sequence, via a `TODO(#392)` marker asserting the (at that time) known-broken `rebind_pointers` staleness-gate behavior. #392's fix has been authored (closed on GitHub, milestone 0.13.1) but had not yet merged to `main` as of this spec; whether it lands before or after this issue is implemented, running the merge phase *before* the purge/restore/rebind sequence is the correct choice either way — it does not depend on #392's fix having merged, and it is the only placement for which "C's pointers into A start out `bound`" is guaranteed rather than incidental. This resolves the ordering question the issue notes explicitly ("Decide and document which, since a merge against an already-purged A tests nothing.").
- What happens to a WAL stream for a group that is party to none of the three new phases? Same standard this test already applies to existing phases (e.g. B during A's purge/restore): it must be asserted unchanged, on disk, not only via `knowledge_status`.
- What happens if the test process is interrupted or panics mid-run? It must not leak persistent state outside its own temp directory (SC-003) regardless of how it exits — the standing convention this test already follows.
- What happens to the two other groups' checkpoints, positions and generations during each new phase? Each new phase's assertions must independently confirm the groups not involved in that specific phase are untouched, following the same "named, on-disk" convention as the existing purge/restore assertions (f)-(h).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The test MUST gain a phase exercising an upstream stream reset (#387): group A republishes under a new generation — a new `.wal-generation.json` identity with republished content, not merely an advance — while C holds pointers into A. It MUST assert that C's pointers end `bound` to A's republished entities via the reset self-heal path, and that group B's stream, position and generation are all unchanged by it.
- **FR-002**: The test MUST gain a phase exercising a cross-group merge (#368/#371): merge two entities within group A while C holds a cross-group edge into the entity that loses. It MUST assert (i) C's pointer follows the `merged_into` chain and remains/ends `bound` to the survivor, (ii) no edge owned by C or by B is rewritten or deleted by a merge performed in A, and (iii) the merge's mutations land only in A's on-disk WAL stream — verified on disk, per the governing rule of ADR-0371 that a write in group G touches only G's data. Per the Edge Cases section, this phase MUST run before the existing purge/restore/rebind sequence, while C's pointers into A are still `bound`.
- **FR-003**: The test MUST gain a phase exercising ambiguous resolution (#369): create two entities in A sharing the `endpoint_name` a pointer from C targets. It MUST assert the pointer's `binding_state` becomes `ambiguous` rather than silently binding to an arbitrary winner, and that a subsequent `knowledge_rebind_pointers` call re-examines it rather than skipping it as already resolved — the `ambiguous` half of #392's FR-001, which the current suite does not reach because no pointer in it is ever ambiguous.
- **FR-004**: The three new phases MUST reuse the existing three-group fixture (groups A, B and C as already established by the current nine phases) rather than standing up a second harness or a fourth group, and MUST follow the established convention that each assertion's failure message names both the property it checks and the motivating issue (e.g. "a merge in A must not rewrite C's edge (#371)").
- **FR-005**: The test MUST remain part of the default `cargo test --release` pass, not `#[ignore]`d, and MUST require no network, API key, corpus fixture or LLM — consistent with the existing test's own constraints.

### Key Entities

- **Group**: An isolated namespace (`group_id`) for entities, relationships, and its own WAL stream. This test continues to use three: `A` and `B` as independent source graphs, `C` as a layer graph holding only cross-group edges into them.
- **WAL stream generation**: A per-group identity (`.wal-generation.json`) distinguishing one publication of a group's content from a later, unrelated republish of the same group under the same `group_id` — the subject of User Story 1.
- **Cross-group pointer / `binding_state`**: A resolvable reference from one group's edge to another group's entity, carrying a tri-state `binding_state`: `bound` (resolves to exactly one entity), `unbound` (resolves to none), or `ambiguous` (resolves to more than one) — the third state is the subject of User Story 3.
- **`merged_into` forwarding**: A record left on a tombstoned (merged-away) alias entity pointing at its surviving canonical, letting a stale cross-group pointer resolve forward to the correct entity after a merge — the subject of User Story 2.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The extended test completes well within its existing 60-second CI budget. (It currently runs in ~4s, leaving substantial headroom; the PR that implements this issue reports the new figure.)
- **SC-002**: Reverting #387's generation handling, #371's foreign-group merge skip, or #369's ambiguity detection each independently causes at least one named assertion (from the corresponding new phase) to fail.
- **SC-003**: The test leaves no state outside its own temp directory.

## Assumptions

- `crates/service/tests/mcp_multistream_e2e.rs`, as merged by PR #395, already exists with the nine phases and ten assertions described in this issue's Background, and already provides the reusable fixture (three-group setup, `McpClient`, `wal_snapshot`, `c_bindings`/`binding_states_into` helpers) this issue's three new phases build on (confirmed present in the codebase as of this spec).
- `crates/core/tests/wal_generation_reset.rs`, `merge_entities.rs`, and `cross_group_pointers.rs` already provide first coverage of generation reset, cross-group merge, and ambiguous binding respectively at the `crates/core` integration level against a real database (confirmed present as of this spec). This issue adds composition-level coverage at the live-process/MCP boundary; it is not the first test of any of these three behaviors.
- ADR-0369 (resolvable cross-group pointers, the `bound`/`unbound`/`ambiguous` tri-state), ADR-0371 (a merge in group G touches only G's data; `merged_into` forwarding), and ADR-0387 (WAL stream generation identity and the `knowledge_rebuild_from_wal` self-heal path) are all Accepted and already implemented — the mechanisms the three new phases exercise already exist in the codebase. This issue adds test coverage; it does not implement new product behavior.
- The MCP methods the new phases depend on (`knowledge_merge_entities` or equivalent, `knowledge_rebind_pointers`, `knowledge_rebuild_from_wal`, `knowledge_status`, `knowledge_add_cross_group_edge`, `knowledge_assert_entity`) already exist in the current codebase (confirmed present as of this spec, several already used by the existing nine phases).
- #392 (the `rebind_pointers` staleness-gate bug the existing phase 9 documents with a `TODO(#392)` and asserts as still-broken) is closed on GitHub with its fix authored on a separate branch, but that fix had not yet merged into `main` as of this spec's base commit — the test file in this issue's starting state still asserts the pre-fix behavior. The Edge Cases ordering constraint for User Story 2 (merge phase before purge/restore/rebind) is written to hold regardless of whether #392's fix has merged by the time this issue is implemented.

## Out of Scope

- Fixing #392 (the `rebind_pointers` staleness-gate bug) — unrelated to and unaffected by this issue's three new phases, which are ordered specifically to avoid depending on it (see Edge Cases).
- Any product-facing behavior change, new MCP method, or schema change — this issue is purely test coverage for behavior that already exists and is already covered at the `crates/core` level.
- Adding compositions beyond the three named (generation reset, cross-group merge, ambiguous resolution) — a fourth or later composition gap, if one is found, is a follow-up issue.
- Deciding the exact insertion point/phase numbering for the three new phases within the test file (beyond the merge-phase-before-purge ordering constraint this spec fixes), the internal mechanism used to simulate an externally-republished WAL generation, and any refactoring of the existing nine-phase test's structure — implementation detail left to the Research/Plan/Implement stages.
- Modifying or extending the existing `crates/core` integration coverage (`wal_generation_reset.rs`, `merge_entities.rs`, `cross_group_pointers.rs`) — this issue only adds coverage at the process boundary, on top of what those already cover.

## Source References

- `crates/service/tests/mcp_multistream_e2e.rs` — the test this issue extends (added by #394, merged as PR #395).
- `crates/core/tests/wal_generation_reset.rs`, `crates/core/tests/merge_entities.rs`, `crates/core/tests/cross_group_pointers.rs` — existing `crates/core`-level coverage of the three behaviors this issue composes at the process boundary.
- [ADR-0369](/docs/adr/0369-resolvable-cross-group-pointers.md) — the pointer model and `bound`/`unbound`/`ambiguous` tri-state User Story 3 exercises.
- [ADR-0371](/docs/adr/0371-merge-never-writes-foreign-group-data.md) — "a merge in group G touches only edges whose `group_id == G`" and `merged_into` forwarding, the rule User Story 2 verifies end to end.
- [ADR-0387](/docs/adr/0387-wal-stream-generation-identity.md) — WAL stream generation identity and the `knowledge_rebuild_from_wal` self-heal path User Story 1 verifies end to end.
- Motivating issues: #394/PR #395 (the original port), #392 (the third defect this test surfaced; closed, milestone 0.13.1; its fix had not yet merged to `main` as of this spec's base commit — see Assumptions), #368/#371 (cross-group merge), #369 (resolvable pointers), #387 (generation identity).
