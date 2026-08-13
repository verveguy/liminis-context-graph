# Feature Specification: Group-scoped complete purge: remove entities and edges, not just episodes

**Feature Branch**: `fabrik/issue-361`
**Created**: 2026-08-11
**Status**: Draft
**Input**: User description: "Group-scoped complete purge: remove entities and edges, not just episodes"

## Background

There is no way to remove one `group_id`'s knowledge from a database. The existing deletion
methods remove episodes but leave the graph they produced behind.

From `crates/core/src/db.rs`, quoted in #353's backfill guard:

> `remove_episode` and `remove_episodes_by_source`/`_by_chunk_id` only `DETACH DELETE` the
> `Episodic` node, never the `Entity`/edge data it created

So `knowledge_delete_by_source` scoped to a group removes the episodes and **orphans every entity
and relationship they created**. Those nodes stay in the graph: still returned by FTS and vector
search, still traversable, and now *unattributable*, because the episodes that linked them to a
source are gone. That is worse than not purging at all — the data survives with its provenance
stripped. The only complete removal today is `knowledge_clear_all`, which clears everything.

This is a known shape, not a new discovery: it is exactly why #353's backfill cannot treat
"zero episodes" as "empty graph" and has to check `count_nodes("Entity") == 0 &&
count_relates_to_edges() == 0` before collapsing to a `0` position. It has simply never been
surfaced as a purge limitation.

**Why it matters now**: A multi-source read replica (see the companion multi-source hydration
issue) needs **per-source refresh**: drop one source's contribution and replay it, without
touching the others. Without a complete group-scoped purge, the only correct refresh is
`clear_all` plus a full rehydrate of every source. ADR-0026 documents a production WAL at 43,821
files and ~7h estimated full replay. Paying that to refresh one source out of N is what makes
multi-source replicas impractical, and it defeats the point of the per-source positions being
added alongside.

With the settled multi-stream WAL model (one logical graph = one `group_id` = one WAL directory —
see #360), a group and a stream are the same thing. So purging group B followed by replaying B's
own WAL directory from B's own cursor **is** isolated per-stream reset, leaving co-resident
streams untouched. That replaces a full-rebuild-on-any-change workaround downstream (orac
`--wal-merged`) — that benefit falls out of the model rather than needing separate engineering.

> **Implementation note (Review stage, PR #381)**: #360 was closed as superseded (`NOT_PLANNED`)
> and replaced by #378, which is still open and unimplemented. There is today exactly one WAL
> directory and one DB-wide `applied_seq` per instance, not one per group, so the per-stream
> reset described above is not yet buildable. See FR-005/SC-004/SC-006 below and
> [ADR-0361](../../docs/adr/0361-group-scoped-purge.md) for how the implementation handles this
> gap.

**Why the existing workaround is not acceptable**: The only way to clear a group today is
`knowledge_query_cypher` with `DELETE … WHERE group_id = $g`, which **bypasses the WAL and
embedding invariants** the structured write tools maintain. A control plane cannot depend on that.

**Why it is tractable, and why that is now conditional**: Under the replica model these issues
assume — sources are bounded contexts, no cross-source edges, no shared entities — a group's
subgraph is disjoint from every other group's, *as long as no cross-group edges exist*. Under
that condition there is nothing to dangle: no edge crosses a group boundary, so removing a
group's nodes cannot orphan another group's data.

That condition is **deliberately retired by #369** (resolvable semantic pointers for cross-graph
references), which introduces a layer graph — its own `group_id`, with edges connecting entities
across source groups — by construction. Once cross-group edges exist, a group-scoped purge *can*
orphan another group's data unless it is handled explicitly. #369 supplies the answer this issue
depends on: a cross-group edge affected by a purge is left **unbound** rather than deleted or
left dangling. This issue is therefore **blocked by #369**: the purge implementation here must
produce the state #369 defines. The purge itself stays exactly as tractable — the disjointness it
needs is over *entities and same-group edges*, which still holds; only edges that cross a group
boundary are the exception, and #369 defines how those are handled.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Purge a group's data completely (Priority: P1)

As an operator refreshing a per-source read replica, I want to purge all data belonging to one
or more `group_id`s so that a source's contribution can be dropped and replayed without touching
other sources, without leaving orphaned data, and without silently destroying another group's
cross-group references.

**Why this priority**: This is the core capability the issue exists to deliver; without it, no
per-source refresh workflow is possible, and the only alternative is a full `clear_all` plus
rehydrate of every source.

**Independent Test**: Purge group A from a database holding groups A and B; verify by direct
count that group A's entities, episodes, and edges are gone and group B's are untouched.

**Acceptance Scenarios**:

1. **Given** a DB with groups A and B, **When** group A is purged, **Then** no `Entity`,
   `Episodic`, or relationship carrying group A remains, and group B's entity/episode/edge counts
   are identical before and after.
2. **Given** a DB with a cross-group edge whose hop node (`RelatesToNode_`) belongs to group L
   and whose hop relationship touches an `Entity` in group A, **When** group A is purged, **Then**
   the `RelatesToNode_` belonging to group L is preserved (not deleted) and the affected edge is
   left in the `unbound` state defined by #369.
3. **Given** a purged group A, **When** its own WAL directory is replayed from its own cursor,
   **Then** the pre-purge state is restored exactly (same counts; UUIDs are stable because they
   come from the WAL). _(Deferred to #378 — see the implementation note under Background. Tested
   today via the closest available proxy: purge group A, then replay the one existing DB-wide WAL
   from before the purge and confirm A is restored while B is unaffected.)_
4. **Given** a `group_id` that does not exist in the DB, **When** purge is called with it,
   **Then** the call succeeds as a no-op and nothing changes.

---

### User Story 2 - Preview a purge before committing to it (Priority: P2)

As an operator about to purge a group, I want to see what the purge would do — counts of
entities, edges, and episodes that would be removed, and which other groups' cross-group pointers
would be left `unbound` — without actually mutating the database, because deletion is irreversible
on a master and the cross-group blast radius is not otherwise computable from the call's own
parameters.

**Why this priority**: Secondary to the purge capability itself, but necessary because — after
#369 — a group purge has a second-order effect (other groups' pointers going `unbound`) that is
invisible to the caller ahead of time and not discoverable through the group-scoped read tools
(`knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group`), since those are scoped to the
group being purged, not the other groups whose pointers are affected.

**Independent Test**: Call `knowledge_delete_by_group` with `dry_run: true` against a DB with a
cross-group edge into the target group; verify no data changed and the returned counts match what
a subsequent real (non-dry-run) purge actually removes/unbinds.

**Acceptance Scenarios**:

1. **Given** a DB with groups A and B and a cross-group pointer from B into A, **When**
   `knowledge_delete_by_group(["A"], dry_run: true)` is called, **Then** no `Entity`, `Episodic`,
   or relationship is deleted, no pointer becomes `unbound`, and the call returns the counts of
   entities/edges/episodes that a real purge of A would remove, plus the count of pointers (owned
   by B) that would become `unbound`.
2. **Given** the same DB, **When** the dry-run result is compared against the result of an
   immediately following real purge of A, **Then** the counts match exactly.

---

### Edge Cases

- Purging a `group_id` that doesn't exist (must be a no-op success, not an error).
- Purging a group that has inbound cross-group edges from another group: the foreign
  `RelatesToNode_` must survive, left `unbound`, not deleted and not left dangling.
- A search that previously returned group A results must return none after purging A, while
  still returning group B's results.
- Read paths encountering a half-connected `RelatesToNode_` (one hop rel destroyed, node itself
  intact) during the routine `unbound` window that occurs on every refresh.
- `dry_run: true` combined with `confirm: true` (or omitted): `dry_run` takes precedence — no
  mutation occurs regardless of `confirm`.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A method that removes **all** data for one or more `group_id`s — `Episodic` nodes,
  `Entity` nodes, and the relationships among them — leaving no orphans.
- **FR-002**: Accept a **list** of `group_id`s, not a single value. A logical source may span
  several groups (e.g. an orac/zen channel carrying an alias chain across a repo rename), and
  purging it must be one atomic operation rather than N calls with partial-failure states.
- **FR-003**: Entities or edges belonging to a group NOT named in the call MUST be untouched.
  Verify by count, not by inspection.
- **FR-004**: Indexes (FTS, HNSW, name index) MUST be consistent afterwards — either maintained
  during the purge or explicitly rebuilt, with the choice documented. A purge that leaves a stale
  HNSW index pointing at deleted nodes is the failure mode to avoid; `mark_name_index_untrusted`
  is the existing precedent for signalling this.
- **FR-005**: The applied position for the purged source MUST be reset, so the next hydration
  replays it from the beginning rather than believing it is current. This is the coupling to the
  multi-source issue — a purge that leaves `applied_seq` intact produces a source that reports
  up-to-date while holding no data. _(Deferred to #378 — `applied_seq` is a DB-wide singleton
  today, not per-group, so resetting it after purging one group would misrepresent every
  co-resident group's position; a worse bug than the one FR-005 exists to prevent. The purge
  leaves it untouched and returns `applied_seq_reset: false` explicitly. See ADR-0361.)_
- **FR-006**: Purging a `group_id` that does not exist MUST be a no-op success, not an error —
  it is the natural idempotent shape for a refresh loop.
- **FR-007**: Available under the `admin` MCP scope, not `write`. A read-only replica needs to
  purge as part of refresh, and it is launched without `write` (see the multi-source issue's
  assumptions).
- **FR-008**: A group-scoped purge MUST NOT delete `RelatesToNode_` nodes belonging to a
  `group_id` other than the one(s) being purged, even when it deletes an `Entity` that node is
  attached to via a hop relationship. The two-hop model stores a cross-group edge as
  `Entity(A) -[:RELATES_TO]-> RelatesToNode_(L) -[:RELATES_TO]-> Entity(B)`, where the
  `RelatesToNode_` carrying the fact and pointer fields belongs to group L. Purging A should
  `DETACH DELETE` `Entity(A)` — destroying the hop rel, which is correct — but the foreign edge
  node is L's data, and deleting it is unrecoverable loss in a group the purge is not
  authoritative for.
- **FR-009**: Cross-group edges whose hop relationship into a purged group was destroyed MUST be
  left in the `unbound` state that #369 defines as a first-class state distinct from
  `invalid_at`, so a later re-hydration can re-bind them (re-resolve the pointer by name,
  re-create the missing hop rel) without new schema or resurrecting deleted rows.
- **FR-010**: `knowledge_status` MUST be able to report how many pointers are currently unbound,
  since the unbound window is routine (occurs on every refresh), not exceptional.
- **FR-011**: Available as an MCP tool `knowledge_delete_by_group(group_ids: [string], confirm?:
  bool, dry_run?: bool)`, deleting all entity nodes, relationship edges, and episodes in the given
  group(s) while maintaining the same WAL/index invariants as the other structured delete tools.
  This supersedes the ad hoc `knowledge_query_cypher` `DELETE … WHERE group_id = $g` workaround,
  which bypasses the WAL and embedding invariants and MUST NOT be treated as a sanctioned deletion
  path.
- **FR-012**: `knowledge_delete_by_group` MUST support a `dry_run?: bool` parameter. When `true`,
  the tool computes and returns a structured plan without mutating any data: per-`group_id` counts
  of entities, edges, and episodes that would be removed; the count of cross-group pointers that
  would be left `unbound` as a result (per FR-009), broken out by the `group_id` that owns each
  affected pointer; and confirmation that no `RelatesToNode_` owned by another group would be
  deleted. This follows the precedent already set by `knowledge_merge_entities`'s `MergePlan`:
  dry-run applies where the blast radius is not computable from the call's own parameters, which
  is the case here — a group purge has a second-order effect (other groups' pointers going
  `unbound`) that is invisible to the caller ahead of time and not discoverable via the
  group-scoped read tools (`knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group`), since
  those are scoped to the group being purged, not the other groups whose pointers are affected.
- **FR-013**: `dry_run` and `confirm` are independent parameters serving different purposes and
  MAY be used together or separately: `dry_run` answers "what would this do?" without mutating;
  `confirm` guards against the real (mutating) call being made by accident. When `dry_run: true`
  is set, it takes precedence — no mutation occurs regardless of the value of `confirm`.

### Key Entities

- **`RelatesToNode_`**: The intermediate node in the two-hop cross-group edge model
  (`Entity(A) -[:RELATES_TO]-> RelatesToNode_(L) -[:RELATES_TO]-> Entity(B)`), carrying the fact
  and pointer fields, owned by the `group_id` of the layer graph that asserted the cross-group
  reference (group L).
- **Unbound state**: The state #369 defines for a cross-group edge whose hop relationship into a
  purged group has been destroyed but whose `RelatesToNode_` survives, pending re-binding on
  rehydration.
- **Purge plan**: The structured, non-mutating result returned by `knowledge_delete_by_group`
  when called with `dry_run: true` — per-group counts of entities, edges, and episodes that would
  be removed, plus counts of cross-group pointers that would become `unbound`, broken out by the
  owning `group_id`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After purging group A from a DB holding groups A and B, no `Entity`, `Episodic`, or
  relationship carrying group A remains — verified by direct count, not by search results.
- **SC-002**: Group B's entity, episode, and edge counts are identical before and after.
- **SC-003**: A search that previously returned group A results returns none afterwards, and still
  returns group B's.
- **SC-004**: Purge-then-replay of group A restores it to exactly the pre-purge state (same counts;
  uuids come from the WAL so they are stable). _(Deferred to #378 — see the implementation note
  under Background; tested today via the DB-wide-WAL proxy described there.)_
- **SC-005**: Purging an absent group_id succeeds and changes nothing.
- **SC-006**: The purged source's applied position is reset (FR-005). _(Deferred to #378 — see
  FR-005.)_
- **SC-007**: After purging group A, every `RelatesToNode_` belonging to any group other than A
  remains present in the database, even where a hop relationship into group A was destroyed.
- **SC-008**: `knowledge_status` reports a non-zero unbound-pointer count immediately after a
  purge that affected cross-group edges, and that count returns to zero once the purged group is
  rehydrated and its pointers are re-bound.
- **SC-009**: A `dry_run: true` call to `knowledge_delete_by_group` mutates nothing (verified by
  direct count, before and after) and returns counts — entities, edges, and episodes per group,
  and cross-group pointers that would become `unbound` broken out by owning group — that exactly
  match the counts actually produced by an immediately following real (non-dry-run) call with the
  same `group_ids`.

## Assumptions

- One logical WAL stream corresponds to one `group_id` and one WAL directory (per #360). Purging
  a group and then replaying that group's own WAL directory from its own cursor is equivalent to
  isolated per-stream reset, leaving co-resident streams untouched. _(Implementation note, Review
  stage: this assumption does not hold yet — #360 was closed as superseded by #378, which is
  still open. See the Background implementation note and ADR-0361.)_
- Corrections (#327/#329) and `same_as` assertions do not exist today. Cascading purge to them is
  out of scope until they exist; when they do, their handling should follow the same principle
  established here for cross-group edges (survive in a defined non-deleted state rather than
  being deleted or left dangling), to be specified when those features land.
- The disjointness invariant this issue originally relied on ("no cross-group edges, so nothing
  can dangle") holds only in the absence of cross-group edges. #369 deliberately introduces
  cross-group edges and is the design that defines survivable behavior for them; this issue's ADR
  must state the invariant as conditional on #369's absence and name #369 as what retires it.

## Out of Scope

- Cascading purge to corrections or `same_as` assertions — they do not exist yet (see
  Assumptions).
- Defining or implementing #369's `unbound` state mechanism itself — this issue depends on and
  produces that state but does not define it.

## Source References

- #353 — the `DETACH DELETE` behaviour documented in its backfill guard.
- ADR-0026 — replay cost (43,821 files, ~7h) that makes selective purge necessary.
- #327 / #329 — corrections, the likely future cascade target.
- #369 — resolvable semantic pointers; defines the `unbound` state and retires the disjointness
  invariant. This issue is blocked by #369.
- #374 — closed as a duplicate of this issue; contributed the `knowledge_delete_by_group` tool
  surface, the WAL-bypass argument against raw cypher delete, and the per-stream-reset framing.
- #360 — multi-stream WAL model; one `group_id` equals one WAL stream. Superseded
  (`NOT_PLANNED`) by #378, which is still open — see the Background implementation note.
- `knowledge_merge_entities` / `MergePlan` — the existing precedent for a `dry_run` mode returning
  a structured plan when the blast radius is not computable from the call's own parameters.

## Notes

- Milestone **0.13.0**.
- Pairs with the multi-source hydration issue — purge-and-replay is the per-source refresh path,
  and FR-005 couples them directly.
