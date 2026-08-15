# Feature Specification: `knowledge_delete_chunk_episode` must require an explicit group scope

**Feature Branch**: `fabrik/issue-406`
**Created**: 2026-08-15
**Status**: Draft
**Input**: User description: "`knowledge_delete_chunk_episode` deletes every `Episodic` row whose `name` matches the given `chunk_id`, across all groups, whenever the caller omits `group_ids`. The liminis app omits it on every call. Because the app enqueues an `unlink` for every chunk whose `chunk_id` disappears from a document, an ordinary heading rename issues one unscoped, name-matched `DETACH DELETE` per affected chunk."

## Background

`knowledge_delete_chunk_episode` deletes `Episodic` rows by matching on `ep.name`, which stores the caller-supplied `chunk_id`. `Conn::remove_episodes_by_chunk_id` (`crates/core/src/db.rs:910-937`) only adds a `group_id IN $gids` predicate when `group_ids` is `Some` and non-empty; otherwise the `MATCH` has no group predicate at all, and the subsequent `DETACH DELETE` removes every matching row in the database regardless of group. `handle_delete_chunk_episode` (`crates/core/src/handlers.rs:1276-1318`) passes `group_ids` straight through via `extract_optional_group_ids`, which returns `None` for a missing key, `null`, or an empty array.

The liminis app calls this method with no `group_ids` on every invocation (`liminis-app/src/main/indexing-queue.ts:1551`), and reaches it routinely rather than exceptionally: `chunk_id` is a structural address derived from a document's heading path (`buildChunkId(docId, headingPath, chunkIndex)`, `liminis-app/src/main/canonical-chunker.ts:456`), so renaming a heading changes the `chunk_id` for every chunk beneath it, and `ChunkStateStore.diffChunks()` enqueues an `unlink`/delete for each one. `Episodic.name` is not exclusive to chunk ingest — `knowledge_add_episode` lets any caller set an arbitrary `name` in any group — so a name collision across groups means an ordinary heading rename in one group can silently destroy another group's episode data.

This is the same failure class as #368 (an operation in one group destroying another group's data), which 0.13.0 treated as release-blocking, and it violates the principle established by ADR-0371 that a write in group G touches only G's data. It is reachable today on `main` with no new feature required, and is scoped as a patch-level defensive fix for 0.13.2.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A scoped chunk deletion touches only its own group (Priority: P1)

An operator (or the liminis app, once its own fix lands) deletes a chunk's episode(s) by `chunk_id`, naming the group the chunk belongs to. Only that group's matching `Episodic` rows are removed; rows in any other group that happen to share the same `name` are left untouched.

**Why this priority**: This is the core data-isolation guarantee the issue exists to restore — without it, a valid, correctly-scoped call could still be reasoned about incorrectly if an unscoped call were still possible elsewhere in the same code path.

**Independent Test**: Create two groups, each holding an `Episodic` row with the same `name` (chunk_id). Call `knowledge_delete_chunk_episode` for that `chunk_id` scoped to group A. Confirm group A's row is gone and group B's row is unchanged (e.g. via `knowledge_status` or `knowledge_get_episodes` scoped to B).

**Acceptance Scenarios**:

1. **Given** groups A and B each hold an `Episodic` row named `chunk_id "X"`, **When** `knowledge_delete_chunk_episode(chunk_id: "X", group_ids: ["A"])` is called, **Then** group A's row is deleted and group B's row still exists afterward.
2. **Given** a single chunk_id maps to several `Episodic` rows within one group (chunking splits one logical chunk into multiple episode rows), **When** the chunk is deleted scoped to that group, **Then** all of that group's matching rows are deleted in one call and `deleted_count` reflects the full count.

---

### User Story 2 - An unscoped call is rejected outright, not widened to every group (Priority: P1)

A caller invokes `knowledge_delete_chunk_episode` without supplying any group scope — omitting `group_ids`, passing `null`, or passing an empty array. Today this silently deletes every matching row across every group in the database. After this fix, the call is rejected before any row is touched, with an error that names the missing parameter.

**Why this priority**: This is the actual defect — the caller supplying no scope must never be interpreted as "all groups," and closing it is the entire purpose of this issue.

**Independent Test**: Call `knowledge_delete_chunk_episode` with `group_ids` omitted (and separately with `group_ids: null` and `group_ids: []`) against a database holding matching rows in more than one group. Confirm the call returns an error and that every row in every group is still present afterward.

**Acceptance Scenarios**:

1. **Given** groups A and B each hold an `Episodic` row named `chunk_id "X"`, **When** `knowledge_delete_chunk_episode(chunk_id: "X")` is called with `group_ids` omitted, **Then** the call returns an error identifying that a group scope is required, and both A's and B's rows still exist afterward.
2. **Given** the same setup, **When** `group_ids` is explicitly `null` or `[]`, **Then** the call is rejected the same way as when it is omitted — an empty or null scope is not treated as "no filter."
3. **Given** the same setup, **When** the rejected call's error is inspected, **Then** it names the missing parameter (`group_ids`) rather than a generic failure, so a caller can distinguish "you forgot the scope" from other error classes.

---

### Edge Cases

- `group_ids` supplied and non-empty, but none of the named groups contain a row matching `chunk_id`: this is a normal "nothing to delete" outcome (`deleted_count: 0`, `success: true`), not an error — the error path is specifically for a missing/empty scope, not for a scope that matches nothing.
- `group_ids` supplied with more than one group: the deletion is scoped to the union of the named groups (existing multi-group behavior via `group_id IN $gids`), not rejected — "no scope" and "multiple groups" are different cases.
- A `chunk_id` that has never existed in any group, called with a valid scope: returns success with `deleted_count: 0`, consistent with current behavior for a non-matching name.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_delete_chunk_episode` MUST require a non-empty, resolvable group scope and MUST reject the request — deleting nothing — when no such scope is supplied, whether `group_ids` is absent, `null`, or an empty array.
- **FR-002**: The rejection MUST surface as an actionable error that names the missing parameter (`group_ids`), and MUST NOT be satisfied by silently substituting a default group (e.g. `DEFAULT_GROUP_ID`) or any other implicit scope.
- **FR-003**: When a valid group scope is supplied, the deletion MUST match and delete only `Episodic` rows whose `group_id` is among the named groups; rows with a matching `name` in any other group MUST remain untouched.
- **FR-004**: The deletion MUST continue to remove every `Episodic` row matching the given `chunk_id` within the requested scope in a single call, including the case where one `chunk_id` maps to multiple episode rows.
- **FR-005**: `Conn::remove_episodes_by_chunk_id`'s group-scope parameter MUST become mandatory (non-`Option`) so that an unscoped, all-groups query is not representable at the data-access layer, not merely blocked at the request-handling layer above it.
- **FR-006**: A group scope that is syntactically valid but matches no rows (in the named groups) MUST continue to return a successful, zero-count result rather than an error — the new rejection is specific to a missing/empty scope, not to a scope that matches nothing.

### Key Entities

- **Episodic row**: a graph node representing one ingested episode/chunk, carrying `name` (the caller-supplied `chunk_id`) and `group_id` (its owning group).
- **Group scope**: the set of `group_id` values a request is confined to; for this method it must be supplied explicitly by the caller rather than defaulted.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A call to `knowledge_delete_chunk_episode` with no group scope (omitted, `null`, or `[]`) returns an error and results in zero `Episodic` rows deleted anywhere in the database, verified against a fixture with matching rows in multiple groups.
- **SC-002**: Given two groups each holding an `Episodic` row with the same `name`, a scoped delete for group A removes only group A's row; group B's row is confirmed present immediately afterward.
- **SC-003**: A `chunk_id` mapping to multiple `Episodic` rows within one scoped group is fully deleted in a single call, with `deleted_count` equal to the number of matching rows in that group.

## Assumptions

- The client-side fix — making the liminis app pass an explicit `group_id` on every `deleteChunkEpisode` call — is filed as `liminis#998`. Failing closed on an unscoped request is still correct behavior and is not weakened by this note; what changes is that the two fixes must be **coordinated at rollout**, not merely tracked as independent work: `liminis-app/scripts/build-liminis-context-graph.sh` builds the server binary from an unpinned sibling checkout (no tag/commit/version pin) and bundles it straight into the app, so the next app build after this server fix lands on `main` would bundle a server that rejects unscoped deletes while `indexing-queue.ts:1551` still sends them, breaking every unlink and every heading rename until the client fix also lands. This is a deployment-sequencing concern for whoever merges this issue's PR, not a change to this issue's own requirements or acceptance criteria.
- `liminis#998` scoping also confirmed that the app's indexing path never sends a `group_id` either (`processChunk` params are `{ source_file, chunk_text, chunk_id, reference_time }`), so all app-indexed episodes currently live in `DEFAULT_GROUP_ID`. The client fix is to state that group explicitly, not to introduce a new one — informational context for Research/Plan, not a change to this issue's scope.
- The narrower hazard in #292 (a `knowledge_add_episode` row colliding with a chunk lineage *within* one group) is explicitly out of scope, per the issue, and is being handled separately.
- This fix does not depend on, and should not wait for, the chunked-ingest or temporal-model work referenced for later milestones.

## Out of Scope

- The liminis app's client-side call site (`indexing-queue.ts`) — filed as `liminis#998`; see Assumptions for the rollout-coordination note.
- #292 (within-group `knowledge_add_episode`/chunk-lineage collision).
- Chunked-ingest and temporal-model work for later milestones.

## Source References

- `crates/core/src/db.rs:910-937` — `remove_episodes_by_chunk_id`
- `crates/core/src/handlers.rs:1276-1318` — `handle_delete_chunk_episode`
- `crates/core/src/handlers.rs:4356-4372` — `extract_optional_group_ids` (missing/null/empty all resolve to `None`)
- `crates/core/src/db.rs:865-895` — `remove_episodes_by_source` (identical conditional-scope pattern; whether this issue's fix also covers it is an unresolved scope question — see the issue thread)
- `crates/core/src/handlers.rs:1226-1268` — `handle_delete_by_source`
- `crates/core/src/handlers.rs:1334-1363` — `handle_delete_by_group`'s existing mandatory, actionable `group_ids` validation (precedent for the error style this issue asks for)
- `liminis-app/src/main/indexing-queue.ts:1551` — the unscoped client call
- `liminis-app/src/main/canonical-chunker.ts:456` — structural `chunk_id` derivation
- `docs/adr/0371-merge-never-writes-foreign-group-data.md` — "a write in group G touches only G's data"
- #368 — the same failure class, fixed in 0.13.0
