# Feature Specification: `knowledge_delete_chunk_episode` and `knowledge_delete_by_source` must require an explicit group scope

**Feature Branch**: `fabrik/issue-406`
**Created**: 2026-08-15
**Status**: Specified
**Input**: User description: "`knowledge_delete_chunk_episode` deletes every `Episodic` row whose `name` matches the given `chunk_id`, across all groups, whenever the caller omits `group_ids`. The liminis app omits it on every call. Because the app enqueues an `unlink` for every chunk whose `chunk_id` disappears from a document, an ordinary heading rename issues one unscoped, name-matched `DETACH DELETE` per affected chunk."

## Background

`knowledge_delete_chunk_episode` deletes `Episodic` rows by matching on `ep.name`, which stores the caller-supplied `chunk_id`. `Conn::remove_episodes_by_chunk_id` (`crates/core/src/db.rs:910-937`) only adds a `group_id IN $gids` predicate when `group_ids` is `Some` and non-empty; otherwise the `MATCH` has no group predicate at all, and the subsequent `DETACH DELETE` removes every matching row in the database regardless of group. `handle_delete_chunk_episode` (`crates/core/src/handlers.rs:1276-1318`) passes `group_ids` straight through via `extract_optional_group_ids`, which returns `None` for a missing key, `null`, or an empty array.

The liminis app calls this method with no `group_ids` on every invocation (`liminis-app/src/main/indexing-queue.ts:1551`), and reaches it routinely rather than exceptionally: `chunk_id` is a structural address derived from a document's heading path (`buildChunkId(docId, headingPath, chunkIndex)`, `liminis-app/src/main/canonical-chunker.ts:456`), so renaming a heading changes the `chunk_id` for every chunk beneath it. The old ids drop out of the new chunk set and land in `ChunkStateStore.diffChunks()`'s `deleted` bucket (`chunk-state-store.ts`), each of which is enqueued as an `unlink` (`indexing-queue.ts:1178-1181`) that in turn issues one unscoped `deleteChunkEpisode` call (`indexing-queue.ts:1551`) — so a single heading rename fires a batch of unscoped, name-matched deletes, not just one. `Episodic.name` is not exclusive to chunk ingest — `knowledge_add_episode` lets any caller set an arbitrary `name` in any group — so a name collision across groups means an ordinary heading rename in one group can silently destroy another group's episode data.

This is the same failure class as #368 (an operation in one group destroying another group's data), which 0.13.0 treated as release-blocking, and it violates the principle established across ADR-0368, ADR-0371, and ADR-0385 that a write in group G touches only G's data. It is reachable today on `main` with no new feature required, and is scoped as a patch-level defensive fix for 0.13.2.

Issue #403 was filed independently, minutes before this issue, describing the same defect and mechanism; it is being closed as a duplicate of this one and its content folded in here.

**Distinguishing this from #292**: #292 examined `knowledge_delete_chunk_episode` and ruled it in-bounds-and-fine for **name** collisions — its stated rationale is that this is "an explicit, caller-invoked deletion whose contract has always been 'delete everything with this name.'" That reasoning covers name collisions within the caller's own intent; it does not address **group** scope, and it presumes a deliberate operator action. The liminis app's calls are not that — they are issued automatically by the indexing pipeline in response to a heading rename, with no operator reviewing or confirming each delete. This issue and #292 are addressing different axes of the same handler and are not in tension.

**Scope confirmed to include `knowledge_delete_by_source`**: `Conn::remove_episodes_by_source` (`crates/core/src/db.rs:865-895`, backing `handle_delete_by_source` at `crates/core/src/handlers.rs:1226-1268`) has the identical conditional-scope defect, forty lines above the chunk variant:

```rust
let group_clause = match group_ids {
    Some(ids) if !ids.is_empty() => " AND ep.group_id IN $gids",
    _ => "",
};
```

This method is folded into this issue's scope rather than deferred to a follow-up, for three reasons:

1. **It is strictly broader than the chunk-id bug.** `remove_episodes_by_source` matches `ep.source_description = $src OR ep.source_description STARTS WITH $prefix`, so an unscoped call prefix-matches across every group, where the chunk variant at least requires an exact `ep.name` match. Same unscoped `DETACH DELETE`, larger blast radius.
2. **The known caller invokes both, back to back, in the same code path.** In the liminis app's unlink handling, `indexing-queue.ts:1551` calls `deleteChunkEpisode({ chunk_id })` and `indexing-queue.ts:1555` calls `deleteBySource({ source_file: entry.filePath })` — both unscoped today. Fixing only one and shipping 0.13.2 would close half of this code path and leave the other half live, on the very same file deletion.
3. **The source variant is also exposed on the MCP agent-write surface with no scope**, at `knowledge-writer-provider.ts:104` (`deleteBySource({ source_file })`, no `group_ids`). That is a genuinely multi-group surface by construction — the tenancy case 0.13.0 shipped for, and one where cross-group deletion is not a theoretical risk.

Both functions get the same resolution: the group scope becomes mandatory, and neither defaults to `DEFAULT_GROUP_ID` or any other implicit scope — a silent default is how this defect reached `main` in the first place.

**Related, explicitly deferred**: #402 asks how a single delete call that spans multiple groups should attribute its WAL mutations. Making the group scope mandatory narrows #402 (an unscoped, all-groups call is no longer possible) but does not resolve it — an explicit multi-group `group_ids` is still permitted, and WAL attribution across that set is still an open question. #402 stays open and is out of scope for this issue; see Out of Scope.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A scoped chunk deletion touches only its own group (Priority: P1)

An operator (or the liminis app, once its own fix lands) deletes a chunk's episode(s) by `chunk_id`, naming the group the chunk belongs to. Only that group's matching `Episodic` rows are removed; rows in any other group that happen to share the same `name` are left untouched.

**Why this priority**: This is the core data-isolation guarantee the issue exists to restore — without it, a valid, correctly-scoped call could still be reasoned about incorrectly if an unscoped call were still possible elsewhere in the same code path.

**Independent Test**: Create two groups, each holding an `Episodic` row with the same `name` (chunk_id). Call `knowledge_delete_chunk_episode` for that `chunk_id` scoped to group A. Confirm group A's row is gone and group B's row is unchanged (e.g. via `knowledge_status` or `knowledge_get_episodes` scoped to B).

**Acceptance Scenarios**:

1. **Given** groups A and B each hold an `Episodic` row named `chunk_id "X"`, **When** `knowledge_delete_chunk_episode(chunk_id: "X", group_ids: ["A"])` is called, **Then** group A's row is deleted and group B's row still exists afterward.
2. **Given** a single chunk_id maps to several `Episodic` rows within one group (chunking splits one logical chunk into multiple episode rows), **When** the chunk is deleted scoped to that group, **Then** all of that group's matching rows are deleted in one call and `deleted_count` reflects the full count.

---

### User Story 2 - An unscoped chunk-deletion call is rejected outright, not widened to every group (Priority: P1)

A caller invokes `knowledge_delete_chunk_episode` without supplying any group scope — omitting `group_ids`, passing `null`, or passing an empty array. Today this silently deletes every matching row across every group in the database. After this fix, the call is rejected before any row is touched, with an error that names the missing parameter.

**Why this priority**: This is the actual defect — the caller supplying no scope must never be interpreted as "all groups," and closing it is the entire purpose of this issue.

**Independent Test**: Call `knowledge_delete_chunk_episode` with `group_ids` omitted (and separately with `group_ids: null` and `group_ids: []`) against a database holding matching rows in more than one group. Confirm the call returns an error and that every row in every group is still present afterward.

**Acceptance Scenarios**:

1. **Given** groups A and B each hold an `Episodic` row named `chunk_id "X"`, **When** `knowledge_delete_chunk_episode(chunk_id: "X")` is called with `group_ids` omitted, **Then** the call returns an error identifying that a group scope is required, and both A's and B's rows still exist afterward.
2. **Given** the same setup, **When** `group_ids` is explicitly `null` or `[]`, **Then** the call is rejected the same way as when it is omitted — an empty or null scope is not treated as "no filter."
3. **Given** the same setup, **When** the rejected call's error is inspected, **Then** it names the missing parameter (`group_ids`) rather than a generic failure, so a caller can distinguish "you forgot the scope" from other error classes.

---

### User Story 3 - A scoped source deletion touches only its own group (Priority: P1)

An operator (or the liminis app, once its own fix lands) deletes all episodes ingested from a source file by naming the group the source belongs to. Only that group's matching `Episodic` rows — whether matched by exact `source_description` or by the `source_file:` prefix — are removed; rows in any other group are left untouched even when they match or prefix-match the same source file.

**Why this priority**: `knowledge_delete_by_source` has the identical unscoped-delete defect as `knowledge_delete_chunk_episode`, in the same file, and is invoked on the same file-deletion path immediately after it. Its prefix match also gives it a larger blast radius than the chunk variant's exact-name match, making the isolation guarantee here at least as important.

**Independent Test**: Create two groups, each holding an `Episodic` row with the same `source_description` (or a value sharing a `source_file:` prefix). Call `knowledge_delete_by_source` for that source scoped to group A. Confirm group A's row(s) are gone and group B's row(s) are unchanged.

**Acceptance Scenarios**:

1. **Given** groups A and B each hold an `Episodic` row with `source_description = "doc.md"`, **When** `knowledge_delete_by_source(source_file: "doc.md", group_ids: ["A"])` is called, **Then** group A's row is deleted and group B's row still exists afterward.
2. **Given** groups A and B each hold an `Episodic` row whose `source_description` starts with `"doc.md:"`, **When** the same scoped call is made, **Then** only group A's prefix-matching rows are deleted; group B's rows are untouched.

---

### User Story 4 - An unscoped source-deletion call is rejected outright (Priority: P1)

A caller invokes `knowledge_delete_by_source` without supplying any group scope. Today this silently deletes every matching row — exact match or prefix match — across every group in the database, and is reachable both from the liminis app's indexing pipeline and from the MCP agent-write surface (`knowledge-writer-provider.ts:104` calls it with no scope). After this fix, the call is rejected before any row is touched, with an error that names the missing parameter.

**Why this priority**: Same defect class as User Story 2, on the sibling function, with agent-write exposure making the multi-group blast radius non-theoretical.

**Independent Test**: Call `knowledge_delete_by_source` with `group_ids` omitted (and separately `null` and `[]`) against a database holding matching rows in more than one group. Confirm the call returns an error and every row in every group is still present afterward.

**Acceptance Scenarios**:

1. **Given** groups A and B each hold a matching `Episodic` row, **When** `knowledge_delete_by_source(source_file: "doc.md")` is called with `group_ids` omitted, **Then** the call returns an error identifying that a group scope is required, and both groups' rows still exist afterward.
2. **Given** the same setup, **When** `group_ids` is explicitly `null` or `[]`, **Then** the call is rejected the same way.
3. **Given** the same setup, **When** the rejected call's error is inspected, **Then** it names the missing parameter (`group_ids`).

---

### Edge Cases

- `group_ids` supplied and non-empty, but none of the named groups contain a row matching the delete criteria (chunk_id name, or source exact/prefix match): this is a normal "nothing to delete" outcome (`deleted_count: 0`, `success: true`), not an error — the error path is specifically for a missing/empty scope, not for a scope that matches nothing.
- `group_ids` supplied with more than one group, for either method: the deletion is scoped to the union of the named groups (existing multi-group behavior via `group_id IN $gids`), not rejected — "no scope" and "multiple groups" are different cases.
- A `chunk_id` or `source_file` that has never existed in any group, called with a valid scope: returns success with `deleted_count: 0`, consistent with current behavior for a non-matching name/source.
- A `source_file` value that exactly equals another source's prefix (e.g. `"doc"` vs. `"doc.md"`): prefix-matching behavior is unchanged by this fix; only the group predicate becomes mandatory, so this remains whatever it already is today, just group-scoped.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Both `knowledge_delete_chunk_episode` and `knowledge_delete_by_source` MUST require a non-empty, resolvable group scope and MUST reject the request — deleting nothing — when no such scope is supplied, whether `group_ids` is absent, `null`, or an empty array.
- **FR-002**: The rejection MUST surface, for both methods, as an actionable error that names the missing parameter (`group_ids`), and MUST NOT be satisfied by silently substituting a default group (e.g. `DEFAULT_GROUP_ID`) or any other implicit scope.
- **FR-003**: When a valid group scope is supplied, both methods' deletions MUST match and delete only `Episodic` rows whose `group_id` is among the named groups; rows in any other group MUST remain untouched, regardless of whether they match on `name` (chunk variant) or on `source_description`/prefix (source variant).
- **FR-004**: Both methods MUST continue to remove every `Episodic` row matching their respective criteria within the requested scope in a single call, including the case where one `chunk_id` (or one source file) maps to multiple episode rows.
- **FR-005**: `Conn::remove_episodes_by_chunk_id`'s and `Conn::remove_episodes_by_source`'s group-scope parameters MUST both become mandatory (non-`Option`), so that an unscoped, all-groups query is not representable at the data-access layer for either function, not merely blocked at the request-handling layer above them.
- **FR-006**: A group scope that is syntactically valid but matches no rows (in the named groups) MUST continue to return a successful, zero-count result rather than an error, for both methods — the new rejection is specific to a missing/empty scope, not to a scope that matches nothing.
- **FR-007**: `knowledge_delete_by_source`'s prefix match (`source_description STARTS WITH source_file + ":"`) MUST remain confined to the named group scope — a `source_description` in another group that happens to share the prefix MUST NOT be matched or deleted.

### Key Entities

- **Episodic row**: a graph node representing one ingested episode/chunk, carrying `name` (the caller-supplied `chunk_id`, matched by `knowledge_delete_chunk_episode`), `source_description` (the source file, matched exactly or by prefix by `knowledge_delete_by_source`), and `group_id` (its owning group).
- **Group scope**: the set of `group_id` values a request is confined to; for both methods it must be supplied explicitly by the caller rather than defaulted.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A call to `knowledge_delete_chunk_episode` with no group scope (omitted, `null`, or `[]`) returns an error and results in zero `Episodic` rows deleted anywhere in the database, verified against a fixture with matching rows in multiple groups.
- **SC-002**: Given two groups each holding an `Episodic` row with the same `name`, a scoped delete for group A removes only group A's row; group B's row is confirmed present immediately afterward.
- **SC-003**: A `chunk_id` mapping to multiple `Episodic` rows within one scoped group is fully deleted in a single call, with `deleted_count` equal to the number of matching rows in that group.
- **SC-004**: A call to `knowledge_delete_by_source` with no group scope (omitted, `null`, or `[]`) returns an error and results in zero `Episodic` rows deleted anywhere in the database, verified against a fixture with exact-match and prefix-matching rows in multiple groups.
- **SC-005**: Given two groups each holding an `Episodic` row whose `source_description` matches or prefix-matches the same source file, a scoped delete for group A removes only group A's row(s); group B's rows are confirmed present immediately afterward.

## Assumptions

- The client-side fix — making the liminis app pass an explicit `group_id` on every unscoped delete call — is filed as `liminis#998`. Failing closed on an unscoped request is still correct behavior and is not weakened by this note; what changes is that the two fixes must be **coordinated at rollout**, not merely tracked as independent work: `liminis-app/scripts/build-liminis-context-graph.sh` builds the server binary from an unpinned sibling checkout (no tag/commit/version pin) and bundles it straight into the app, so the next app build after this server fix lands on `main` would bundle a server that rejects unscoped deletes while the app's call sites still send them, breaking every unlink and every heading rename until the client fix also lands. This is a deployment-sequencing concern for whoever merges this issue's PR, not a change to this issue's own requirements or acceptance criteria.
- `liminis#998`'s scope was widened, alongside this issue, to cover all three now-known unscoped call sites: `indexing-queue.ts:1551` (`deleteChunkEpisode`), `indexing-queue.ts:1555` (`deleteBySource`), and the MCP agent-write surface at `knowledge-writer-provider.ts:104` (`deleteBySource`). It was originally scoped to `deleteChunkEpisode` only.
- `liminis#998` scoping also confirmed that the app's indexing path never sends a `group_id` on ingest either (`processChunk` params are `{ source_file, chunk_text, chunk_id, reference_time }`), so all app-indexed episodes currently live in `DEFAULT_GROUP_ID`. The client fix is to state that group explicitly, not to introduce a new one — informational context for Research/Plan, not a change to this issue's scope.
- #292 (a `knowledge_add_episode` row colliding with a chunk lineage *within* one group, on the **name** axis) is explicitly out of scope and is being handled separately — see Background for why it does not overlap with this issue's **group**-scope defect.
- This fix does not depend on, and should not wait for, the chunked-ingest or temporal-model work referenced for later milestones.

## Out of Scope

- The liminis app's client-side call sites (`indexing-queue.ts`, `knowledge-writer-provider.ts`) — filed as `liminis#998`; see Assumptions for the rollout-coordination note.
- #292 (within-group `knowledge_add_episode`/chunk-lineage collision, on the name axis rather than the group axis).
- #402 (WAL attribution when a single, explicitly-scoped multi-group delete call spans several groups). Making the group scope mandatory narrows this — the unscoped, all-groups case that made it worse is gone — but does not resolve it, since an explicit multi-group `group_ids` remains permitted. Deferred to 0.14.0.
- #404 and #405: unverified investigations into edge provenance and upsert-key semantics respectively. Both are open questions whose resolution would change what gets written, which is not patch-level work for a 0.13.2 defensive fix.
- Chunked-ingest and temporal-model work for later milestones.

## Source References

- `crates/core/src/db.rs:910-937` — `remove_episodes_by_chunk_id`
- `crates/core/src/handlers.rs:1276-1318` — `handle_delete_chunk_episode`
- `crates/core/src/handlers.rs:4356-4372` — `extract_optional_group_ids` (missing/null/empty all resolve to `None`)
- `crates/core/src/db.rs:865-895` — `remove_episodes_by_source` (identical conditional-scope pattern, confirmed in scope for this issue)
- `crates/core/src/handlers.rs:1226-1268` — `handle_delete_by_source`
- `crates/core/src/handlers.rs:1334-1363` — `handle_delete_by_group`'s existing mandatory, actionable `group_ids` validation (precedent for the error style this issue asks for)
- `liminis-app/src/main/indexing-queue.ts:1551` — the unscoped `deleteChunkEpisode` client call
- `liminis-app/src/main/indexing-queue.ts:1555` — the unscoped `deleteBySource` client call, immediately following the chunk-episode delete on the same unlink path
- `liminis-app/src/main/indexing-queue.ts:1178-1181` — where a disappeared chunk is enqueued as `unlink`
- `liminis-app/src/main/canonical-chunker.ts:456` — structural `chunk_id` derivation
- `liminis-app/src/main/chunk-state-store.ts` — `ChunkStateStore.diffChunks()`'s `deleted` bucket
- `knowledge-writer-provider.ts:104` — the MCP agent-write surface's unscoped `deleteBySource` call
- `docs/adr/0368-group-scoped-edge-dedup-in-merge.md`, `docs/adr/0371-merge-never-writes-foreign-group-data.md`, `docs/adr/0385-per-group-mutation-attribution-for-multi-group-writers.md` — "a write in group G touches only G's data"
- #368 — the same failure class, fixed in 0.13.0
- #403 — duplicate of this issue, folded in
- #292 — the name-collision question this issue's group-scope defect is distinct from
- #402 — WAL attribution for multi-group delete calls; narrowed but not resolved by this issue, deferred to 0.14.0
- #404, #405 — unverified investigations, explicitly out of scope for this patch-level fix
