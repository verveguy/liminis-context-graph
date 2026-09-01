# Feature Specification: Determine and resolve cross-group WAL attribution for multi-group episode deletes

**Feature Branch**: `fabrik/issue-402`
**Created**: 2026-08-15
**Status**: Specified
**Input**: User description: "`knowledge_delete_chunk_episode` can delete `Episodic` nodes across several groups in one call, but flushes the resulting mutations to exactly one group's WAL writer. Determine whether a group-scoped rebuild of a non-default group restores episodes that a multi-group delete removed from it, and fix it or document why it is safe."

## Background

`knowledge_delete_chunk_episode` (`crates/core/src/handlers.rs`, backed by `Db::remove_episodes_by_chunk_id` in `crates/core/src/db.rs`) can delete `Episodic` nodes belonging to more than one group in a single call. Regardless of how many groups are affected, the resulting mutations are flushed through exactly one group's WAL writer — the caller's single named group when `group_ids` names exactly one group, or the default group's writer otherwise (`crates/core/src/handlers.rs:1574-1579`). `handle_delete_by_source` (`crates/core/src/handlers.rs:1524-1529`, backed by `Db::remove_episodes_by_source`) has the identical fallback pattern, forty lines above it, and its own code comment cites "Same FR-004 rationale."

This collapsing of multi-group mutations into one WAL stream is in tension with the per-group WAL streams introduced by #378 and the per-group mutation attribution principle established in ADR-0385, ADR-0368, and ADR-0371: a write in group G is expected to be attributed to G, so that a rebuild or recovery scoped to G alone sees everything that happened to G. If a delete that removes episodes from group `b` is recorded only in the default group's WAL, a rebuild or recovery scoped to `b` alone would replay `b`'s stream without seeing that delete — and the open question is whether that causes `b`'s deleted episodes to reappear.

**Scope narrowed by #406 (PR #412, merged to `main`).** At the time this issue was filed, `group_ids` was optional for both handlers, and omitting it meant "every group" — the fully unscoped, all-groups form of the defect. Issue #406 (duplicate-closed from #403) made `group_ids` mandatory and non-empty for both `knowledge_delete_chunk_episode` and `knowledge_delete_by_source`; an omitted, `null`, or empty `group_ids` is now rejected outright before any row is touched. This closes the fully-unscoped trigger described in the original issue text. It does not close the defect this issue is about: an explicit multi-group `group_ids` (e.g. `["a", "b", "c"]`) is still permitted and still falls back to the default group's WAL writer per the code cited above. #406's own issue body says so explicitly: *"Making the group scope mandatory narrows #402 ... but does not resolve it — an explicit multi-group `group_ids` is still permitted, and WAL attribution across that set is still an open question."* This spec is scoped to that narrowed, still-live case.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Group-scoped rebuild after a multi-group delete (Priority: P1)

An operator calls `knowledge_delete_chunk_episode` (or `knowledge_delete_by_source`) with an explicit `group_ids` array naming more than one group, at least one of which is not the default group. Later, a group-scoped rebuild or recovery is run for one of those non-default groups. The investigation determines whether that rebuild restores the episodes the delete removed from that group, and the outcome (fix, or documented rationale) is applied to both affected handlers.

**Why this priority**: This is the entire premise of the issue. Everything else — the fix-or-document decision, the parity check between the two handlers — depends on this determination.

**Independent Test**: Create episodes in two non-default groups `a` and `b`. Call `knowledge_delete_chunk_episode` (or `knowledge_delete_by_source`) with `group_ids: ["a", "b"]`. Run a group-scoped rebuild/recovery for group `a` alone. Assert on whether `a`'s deleted episodes are present or absent afterward.

**Acceptance Scenarios**:

1. **Given** episodes exist in non-default groups `a` and `b`, **When** `knowledge_delete_chunk_episode(chunk_id, group_ids: ["a", "b"])` is called and group `a` subsequently undergoes a group-scoped rebuild, **Then** the test records whether `a`'s deleted episodes reappear after the rebuild.
2. **Given** the determination from scenario 1 is "yes, deleted episodes reappear," **When** the fix is implemented (fanning the delete's mutations out per owning group, or another approach that preserves correctness), **Then** the same test sequence shows the episodes remain deleted after the group-scoped rebuild.
3. **Given** the determination from scenario 1 is "no, deleted episodes do not reappear," **When** the investigation concludes, **Then** a code comment or ADR update records why the shared-default-group WAL attribution is safe for the multi-group case, and no functional change is made.
4. **Given** the determination and outcome for `knowledge_delete_chunk_episode`, **When** the identical multi-group scenario is run against `handle_delete_by_source`, **Then** the same determination and outcome (fix, or documented rationale) is recorded for it too, since both handlers share the identical fallback-to-default-group pattern.

### Edge Cases

- `group_ids` naming exactly one group is already routed directly to that group's writer and is unaffected by this issue — only two-or-more-group calls exercise the fallback path.
- `group_ids` naming the default group alongside one or more non-default groups.
- A group-scoped rebuild of a non-default group racing a multi-group delete that is still in flight.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The investigation MUST determine whether a group-scoped rebuild or recovery of a non-default group named in an explicit multi-group `group_ids` delete restores episodes that the delete removed from that group.
- **FR-002**: If restoration occurs, a fix MUST be implemented so that a group-scoped rebuild does not resurrect episodes deleted by a multi-group `knowledge_delete_chunk_episode` or `knowledge_delete_by_source` call naming that group.
- **FR-003**: If restoration does not occur, the codebase MUST record — via code comment and/or ADR — why the shared-default-group WAL attribution is safe for the multi-group case, so a future reader does not have to re-derive it.
- **FR-004**: The determination and its outcome (fix or documented rationale) MUST be produced for both `handle_delete_chunk_episode` and `handle_delete_by_source`, since both share the identical fallback-to-default-group behavior whenever more than one group is named.
- **FR-005**: This investigation and any resulting fix apply only to the explicit multi-group form of `group_ids` (two or more named groups). The previously-possible fully unscoped ("no `group_ids`") form was closed by #406 (PR #412) and is out of scope here.

### Key Entities

- **Episodic node**: A graph node representing an ingested chunk or episode, owned by a `group_id`.
- **WAL stream**: A per-group append-only mutation log (#378) used for group-scoped rebuild and recovery.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A test exists that exercises the sequence — multi-group delete naming a non-default group, followed by a group-scoped rebuild of that group — and asserts on episode presence in a way that matches the recorded determination.
- **SC-002**: For both `knowledge_delete_chunk_episode` and `handle_delete_by_source`, either a code change ships that prevents cross-group resurrection, or the repository carries an explicit, current record (ADR or code comment) of why none is needed.

## Assumptions

- #406 (PR #412) is merged to `main` and enforces mandatory, non-empty `group_ids` on both `knowledge_delete_chunk_episode` and `handle_delete_by_source`. The fully-unscoped ("all groups") trigger no longer exists; the explicit multi-group trigger does, per `crates/core/src/handlers.rs:1524-1529` (`handle_delete_by_source`) and `:1574-1579` (`handle_delete_chunk_episode`).
- Per-group WAL streams (#378) and per-group mutation attribution (ADR-0385) remain the relevant correctness model against which this issue is judged.

## Out of Scope

- The fully unscoped ("no `group_ids`") delete form — already closed by #406 / PR #412.
- Group scoping of the delete's match/read semantics generally — that was #406's concern, not this issue's.

## Source References

- `crates/core/src/handlers.rs:1524-1529` (`handle_delete_by_source` fallback), `:1574-1579` (`handle_delete_chunk_episode` fallback)
- `crates/core/src/db.rs` — `remove_episodes_by_source`, `remove_episodes_by_chunk_id`
- #378 (per-group WAL streams), ADR-0385 (per-group mutation attribution), ADR-0368 / ADR-0371 (group-scoped ownership)
- #292 — different concern (foreign-row identification on the ingest path), same handler family
- #406 (PR #412, merged) — made `group_ids` mandatory for both handlers; its own issue text explicitly defers this issue's cross-group WAL attribution question as still open for the multi-group case
- #403 — closed as duplicate of #406
