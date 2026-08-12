# Feature Specification: Merge must never write another group's data

**Feature Branch**: `fabrik/issue-371`
**Created**: 2026-08-11
**Status**: Draft
**Input**: User description: "Mutations never cross a stream boundary — every mutation in group G's WAL belongs to G. This is currently violated in exactly one place: entity merge. `corrections::merge_entities_inner` rewrites, deduplicates, or self-loop-invalidates an alias entity's edges regardless of which group owns each edge, so a merge in group A can write mutations that belong to a foreign group L's WAL stream. Under the settled multi-stream WAL model (one WAL directory per group_id), that is not free — it requires mutation-level group attribution that does not exist anywhere in the write path today. The fix is to skip foreign edges entirely rather than engineer around the gap: a merge in group G touches only edges whose group_id == G, and the foreign group re-resolves its own binding at its own re-bind time (ADR-0369), becoming unbound by derivation rather than by anyone writing to it. This also surfaces an edge case ADR-0369 deliberately deferred: re-resolution by name recovers a merge silently when canonical and alias share a name, but not when the canonical has a different name (alias 'IBM' merged into 'International Business Machines') — re-resolving 'IBM' just finds the tombstoned alias again. The fix is `merged_into` forwarding on tombstoned aliases, with tombstone detection as the mandatory floor when forwarding can't complete."

## Background

This issue was originally filed for the self-loop branch of `merge_entities_inner` alone — the
reachable failure mode where group `A` merges two entities that a layer group `L` has drawn an
edge between, and `L`'s edge is silently invalidated as an incidental self-loop, with no
replacement, no error, and (since #368's Review stage removed the unused
`MergeEdgeCounts.self_loops` counter) not even a discarded count. That scenario is preserved
below under *Original case (self-loop)* because it is the concrete, reachable bug that motivated
the issue and remains the primary regression target.

The scope has since widened, because the settled multi-stream WAL model (design discussion
across #360, #368, and #369) generalizes it. Three decisions are now settled:

1. **A logical graph, a `group_id`, and a WAL stream are one thing.** One WAL directory per
   group; an instance holds N writers (#360).
2. **Mutations never cross a stream boundary.** Every mutation in group G's WAL belongs to G.
   This is what makes each stream independently replayable. It is currently violated in exactly
   one place: entity merge.
3. **Cross-group references are resolvable semantic pointers owned by the referring graph**
   (#369 / ADR-0369, merged as PR #372). The referenced graph holds nothing about its referrers
   and never learns it has any.

Together these make unbinding **derived rather than recorded**: nobody notifies anybody. Graph
`A` discovers that a binding into `B` is stale when `A` next compares `B`'s applied position
against its own `bound_at_seq` and re-resolves — and that re-bind is `A`'s own write, into `A`'s
own stream.

`corrections::merge_entities_inner` (`crates/core/src/corrections.rs`) is the only remaining code
path that writes to another group's data. For each of an alias entity's edges — collected via the
deliberately un-scoped `get_full_edges_for_entity` — it currently does one of three things,
regardless of which group owns the edge:

| branch | today | required |
|---|---|---|
| self-loop (`new_src == new_dst`) | `invalidate_edge`, no replacement, no count | leave the foreign edge untouched |
| duplicate (`has_directed_edge`) | scoped to the edge's own group by #368 | leave the foreign edge untouched |
| otherwise (rewrite) | rewritten onto the canonical, retaining its own `group_id` | leave the foreign edge untouched |

**All three collapse to one rule: a merge in group G touches only edges whose `group_id == G`.**
Foreign edges are skipped entirely and left for their owning group to re-resolve, becoming
`unbound` by derivation rather than by anyone writing to them.

This is a simplification, not an addition — the loop gains an early `continue` for foreign edges.
Issue #368's per-edge-group `has_directed_edge` scoping stays correct (every edge a merge still
processes belongs to the merging group by construction) but stops being load-bearing for foreign
edges, since they never reach that check.

**Why this, rather than making cross-stream writes work.** The rewrite branch looked like free
correctness when #368 preserved it — a merge repairs the layer's bindings on the layer's behalf.
Under the multi-stream WAL model it is not free: one connection, one `drain_mutations`, mutations
belonging to two groups, and therefore two WAL streams. Getting them into the right streams needs
mutation-level group attribution in a path (`Conn::executed_mutations` → `drain_mutations` →
`wal_exec::wal_flush_*`) that carries no group information at all today. Skipping foreign edges
removes that requirement rather than engineering around it: every WAL then contains only its own
group's mutations, and #360's write-routing work needs only per-operation attribution, which every
write handler already has. The cost is that a merge no longer repairs the foreign layer's bindings
eagerly — the layer is briefly stale until its own re-bind runs. ADR-0369 already establishes that
an unbound window is routine rather than exceptional, so this is consistent with the model rather
than a new concession.

**Edge case: a merge that changes the name.** Re-resolution recovers the common merge silently and
for free, because alias and canonical usually share a name and re-resolving that name finds the
canonical (name resolution deliberately resolves *through* `Merged` tombstones). It does not
recover a merge where the canonical has a *different* name — alias `"IBM"` merged into canonical
`"International Business Machines"`. Re-resolving `"IBM"` finds the tombstoned alias again, so a
foreign pointer re-binds to a `Merged` tombstone with no active edges: silently stale, and
indistinguishable from a healthy binding, unless something detects that the landing row is a
tombstone.

ADR-0369 considered a `merged_into` forwarding pointer on merged aliases and deliberately left it
out of scope, on the grounds that name-based re-resolution already covers the common case. That
deferral is lifted here, because this issue's scope — merge no longer eagerly repairing a foreign
binding — makes the uncovered case (a merge that also changes the name) reachable in exactly the
scenario this issue is about: a foreign group's stale pointer, re-binding entirely on its own,
with no help from the merge that invalidated it. The decision: do both — `merged_into` forwarding
is the answer, with tombstone detection as its mandatory floor, since forwarding alone silently
regresses to "bound and wrong" on any pre-existing tombstone that predates this feature (every
alias tombstoned before it ships has no `merged_into` recorded).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A merge never writes to a foreign group's edges (Priority: P1)

When group `G` merges an alias entity into a canonical, every edge belonging to a *different*
group is left completely untouched: not rewritten onto the canonical, not invalidated as a
duplicate, and not invalidated as a self-loop. Same-group edges keep today's behavior exactly:
rewrite onto the canonical, duplicate collapse, self-loop invalidation.

**Why this priority**: This is the core invariant the issue exists to establish — the one that
makes each group's WAL stream self-contained and independently replayable under the multi-stream
model. Without it, #360's write-routing work has no clean mutation-level group attribution to rely
on.

**Independent Test**: Seed a foreign-group edge referencing entities that are about to be merged
in a different group, in each of the three shapes (would-become-self-loop, would-become-duplicate,
would-be-rewritten); run the merge; confirm the foreign edge's `group_id`, endpoints, and
`invalid_at` are byte-for-byte unchanged, while an equivalent same-group edge in each shape still
gets today's treatment.

**Acceptance Scenarios**:

1. **Given** a foreign-group edge whose merge would produce a self-loop (`new_src == new_dst`),
   **When** the merge runs, **Then** the foreign edge is left untouched — not invalidated, no
   replacement written.
2. **Given** a foreign-group edge that duplicates an edge the canonical already has (in the
   foreign edge's own group), **When** the merge runs, **Then** the foreign edge is left untouched
   — not invalidated as a duplicate.
3. **Given** a foreign-group edge that would otherwise be rewritten onto the canonical, **When**
   the merge runs, **Then** the foreign edge is left untouched — no replacement edge is written
   into the foreign group, and the original edge's `source_node_uuid`/`target_node_uuid` still
   reference the alias.
4. **Given** a same-group edge in each of the three shapes above, **When** the merge runs,
   **Then** it receives exactly today's treatment (self-loop invalidation, duplicate collapse, or
   rewrite onto the canonical, respectively) — same-group behavior is unchanged.
5. **Given** a merge that skips one or more foreign edges, **When** the merge result is inspected,
   **Then** the number of skipped foreign edges is counted and reported through
   `MergeEntitiesResult`, `MergePlan`, `AliasInfo`, and the `handle_merge_entities` IPC response,
   replacing the prior `foreign_edges_rewritten`/`foreign_edges_deduplicated` (and the
   corresponding `MergePlan`/`AliasInfo` fields) with a `foreign_edges_skipped` equivalent.
6. **Given** a merge that touches both same-group and foreign-group edges, **When** the resulting
   mutations are inspected directly against `Conn::drain_mutations()` output (not only against
   post-merge graph state), **Then** every mutation belongs to the merging group's `group_id` —
   none belongs to any other group.

---

### User Story 2 - A merge records what an alias became (Priority: P1)

`corrections::merge_entities` records the canonical's UUID on every alias entity it tombstones, so
a `Merged` row is no longer a dead end — it resolves forward to what it became. This makes a merge
auditable ("what did this entity become") for the first time, and is the mechanism the foreign
group's own re-bind (User Story 3) depends on to recover from a merge that also changed the name.

**Why this priority**: Without this, User Story 3's fixpoint resolution has nothing to follow —
`merged_into` is the data this issue's re-bind fix is built on.

**Independent Test**: Merge alias `X` into canonical `Y`; confirm `X`'s tombstoned row now records
`Y`'s UUID as its `merged_into` target, retrievable by a reader that doesn't already know `Y`.

**Acceptance Scenarios**:

1. **Given** alias `X` is merged into canonical `Y`, **When** the merge completes, **Then** `X`'s
   `Merged`-labelled row records `Y`'s UUID as the entity `X` became.
2. **Given** `apply_same_as` (the second merge path, `corrections.rs`) tombstones an alias,
   **When** it completes, **Then** the same `merged_into` recording applies — this is not
   exclusive to `merge_entities`.

---

### User Story 3 - Re-binding follows a merge chain to its end, and never claims "bound" on a dead end (Priority: P1)

A foreign group's own re-bind pass (ADR-0369), when its pointer resolution lands on a
`Merged`-labelled row, follows that row's `merged_into` forward — repeating if the target is
itself `Merged` — until it reaches a non-`Merged` row (the fixpoint), and binds there. A guard
against cycles is always in place, regardless of whether a cycle is currently believed reachable.
If forwarding cannot complete — no `merged_into` was recorded (every alias tombstoned before this
feature ships has none), or the cycle guard trips — re-binding records `binding_state: unbound`
rather than reporting a binding that is actually wrong.

**Why this priority**: This is the floor that makes User Story 2's forwarding safe to ship
alongside pre-existing data. Forwarding without this floor silently regresses "landed on a
tombstone" from an already-known failure mode (a stale-but-detectable binding) into "reported
bound and wrong" — worse than doing nothing.

**Independent Test**: Build a merge chain `A → B → C` (in `merged_into` terms: `A`'s tombstone
points at `B`, `B`'s at `C`); re-bind a pointer that had resolved to `A`; confirm it lands on `C`.
Separately, build a tombstoned row with no `merged_into` recorded (simulating pre-feature data);
re-bind a pointer that resolves to it; confirm it is recorded `unbound`, never `bound`.

**Acceptance Scenarios**:

1. **Given** entity `A` was merged into `B`, and `B` was later merged into `C`, **When** a foreign
   pointer that had previously resolved to `A` is re-bound, **Then** it binds to `C`.
2. **Given** a `Merged`-labelled row with no `merged_into` recorded, **When** re-binding resolves
   to that row, **Then** the pointer is recorded `unbound`, not `bound`.
3. **Given** a `merged_into` chain that cycles, **When** re-binding follows it, **Then** the cycle
   guard stops the traversal and the pointer is recorded `unbound`, not `bound`, and the pass does
   not hang or error out of the whole re-bind operation.
4. **Given** a merge changed the canonical's name relative to the alias (e.g. `"IBM"` →
   `"International Business Machines"`), **When** a foreign pointer previously bound to the alias
   is re-bound, **Then** it follows `merged_into` to the canonical rather than re-resolving the old
   name and landing on the (still-matching) tombstone.

---

### Edge Cases

- **The layer scenario (original case).** Source group `A` holds entities `X1` and `Y`. Layer
  group `L` asserts a cross-group edge `X1 --[rel]--> Y`. Group `A` merges `X1` into `Y` — a
  legitimate consolidation once `A` decides they are the same entity, which makes the merge
  produce a self-loop from `L`'s perspective (`new_src == new_dst == Y`). Under today's behavior,
  `L`'s edge is invalidated with no replacement, no error, no count. Under the required behavior,
  `L`'s edge is left completely untouched by `A`'s merge, and recovers correctly (bound or
  unbound, as appropriate) the next time `L` runs its own re-bind pass.
- **Pre-existing tombstones carry no `merged_into`.** Every alias merged before this feature ships
  has a `Merged` label but no forwarding data. Re-binding onto one of these must not be
  indistinguishable from a healthy binding — see User Story 3, Acceptance Scenario 2.
- **A merge chain that changes the name partway through.** `A` (name "IBM") merged into `B`
  (also named "IBM" — a same-name consolidation, the common case), `B` later merged into `C`
  (name "International Business Machines" — the name-changing case). A pointer that had resolved
  to `A` must still land on `C` via the forwarding chain, not fall back to a name re-resolution
  that would only work for the first hop.
- **Duplicate/self-loop detection scoped to the edge's own group.** #368 scoped
  `has_directed_edge`'s dedup check to the edge's own `group_id`. That parameter stays correct
  post-#371 for same-group edges; it simply becomes structurally unreachable for foreign edges,
  since they're skipped before the check runs.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `merge_entities_inner` MUST leave every edge whose `group_id` differs from the
  merging group's `group_id` completely untouched — not rewritten onto the canonical, not
  invalidated as a duplicate, not invalidated as a self-loop.
- **FR-002**: Same-group edge handling (rewrite onto canonical, duplicate collapse via
  `has_directed_edge`, self-loop invalidation) MUST remain exactly as it is today, unchanged by
  this issue.
- **FR-003**: A merge MUST NOT produce any mutation belonging to a `group_id` other than the
  merging group's. This MUST be verified directly against `Conn::drain_mutations()` output, not
  only against post-merge graph state, since a mutation could in principle be authored correctly
  in graph-state terms while still being attributed to the wrong stream.
- **FR-004**: Foreign edges skipped by a merge MUST be counted and reported as
  `foreign_edges_skipped` (or a directly equivalent name) across `MergeEntitiesResult`,
  `MergePlan`, `AliasInfo`, and the `handle_merge_entities` IPC response, replacing the existing
  `foreign_edges_rewritten`/`foreign_edges_deduplicated` fields (and their `MergePlan`/`AliasInfo`
  counterparts, `total_foreign_edges_rewritten`/`total_foreign_edges_collapsed` and
  `foreign_active_edges`/`foreign_duplicate_edges`).
- **FR-005**: `merge_entities` MUST record the canonical's UUID as a `merged_into` forwarding
  reference on every alias entity it tombstones with the `Merged` label.
- **FR-006**: `apply_same_as` MUST receive the same treatment as `merge_entities_inner`: foreign
  edges left untouched (FR-001), and `merged_into` recorded on every alias it tombstones (FR-005).
  It already carries #368's group-scoped `has_directed_edge` fix.
- **FR-007**: A resolution process that lands on a `Merged`-labelled row (e.g. a foreign group's
  own re-bind pass, per ADR-0369) MUST follow that row's `merged_into` forward, repeating while
  the target is itself `Merged`, until it reaches a non-`Merged` row (the fixpoint), and bind
  there.
- **FR-008**: Forwarding in FR-007 MUST be guarded against cycles, regardless of whether a cycle
  is currently reachable through normal merge operation.
- **FR-009**: When forwarding cannot complete — no `merged_into` recorded, or the cycle guard
  trips — the resolution process MUST record `binding_state: unbound`, and MUST NOT report
  `bound`.

### Key Entities *(if the feature involves data)*

- **`merged_into` forwarding reference**: data recorded on a tombstoned (`Merged`-labelled) alias
  entity, pointing at the canonical entity's UUID it was merged into. Makes a merge auditable
  ("what did this entity become") and gives a foreign group's re-bind pass a fixpoint to follow
  when name-based re-resolution would otherwise land on the tombstone itself. Storage mechanism
  (e.g. a key in the entity's existing `attributes` JSON column, or a dedicated relationship
  table) is a Research/Plan-stage decision — see Assumptions.
- **`foreign_edges_skipped`**: a per-alias and per-merge count of edges left untouched because
  they belong to a group other than the merging group. Replaces the prior
  `foreign_edges_rewritten`/`foreign_edges_deduplicated` counters, which described mutations this
  issue removes.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A merge in group `G`, given a foreign-group `L` edge referencing entities being
  merged, leaves that edge's `group_id`, endpoints, and validity completely unchanged — verified
  both in post-merge graph state and directly against the merge's own `drain_mutations()` output.
- **SC-002**: Same-group merge behavior (rewrite, dedup, self-loop) is unchanged: existing
  same-group regression coverage continues to pass without modification.
- **SC-003**: A pointer that had resolved to an entity later merged through a two-hop chain
  (`A → B → C`) re-binds to `C`, not to `A` or `B`.
- **SC-004**: A pointer that resolves to a `Merged` row carrying no `merged_into` (representing
  every alias tombstoned before this feature ships) is recorded `unbound`, never `bound`.
- **SC-005**: A merge that changes the canonical's name relative to the alias no longer leaves a
  foreign pointer silently bound to a dead tombstone — it either forwards correctly to the
  canonical or is explicitly `unbound`.
- **SC-006**: The original self-loop scenario (group `A` merges `X1` into `Y`; layer group `L`'s
  edge `X1 --[rel]--> Y` survives the merge untouched and recovers correctly on `L`'s own re-bind)
  passes as a regression test.

## Assumptions

- **Storage mechanism for `merged_into` is a Research/Plan-stage decision, not fixed here.** The
  issue expresses a preference for a dedicated relationship table (queryable/traversable in
  Cypher, supporting fixpoint resolution as a graph traversal) over a JSON `attributes` key
  (no schema migration, consistent with how ADR-0369 carries its own pointer fields) *if the
  migration cost is acceptable* — but this spec requires only that forwarding data be recorded and
  followable to a fixpoint (FR-005, FR-007), not which mechanism carries it.
- **`foreign_edges_skipped` replaces rather than supplements the removed counters.** No consumer
  outside this crate (in particular, no Python-side code under `graphiti_service.py` /
  `service_protocol.py`) references `foreign_edges_rewritten`, `foreign_edges_deduplicated`, or
  their `MergePlan`/`AliasInfo` equivalents — confirmed by search of the codebase at spec time —
  so removing them in favor of `foreign_edges_skipped` is a Rust/IPC-response schema change with
  no cross-language compatibility impact.
- **A detected cycle degrades to the same outcome as a missing `merged_into`** — `unbound` for the
  affected pointer — and does not abort or error the surrounding re-bind pass as a whole.
- **#368's `has_directed_edge` group-scoping stays as-is.** This issue does not revert or modify
  it; it simply stops being reachable for foreign edges, since those are now skipped before the
  duplicate check runs.
- **The removed `MergeEdgeCounts.self_loops` counter (dropped in #368's Review stage) is not being
  reinstated.** Self-loop invalidation still applies only to same-group edges under this issue; no
  new counter is required by this spec beyond `foreign_edges_skipped`.
- **This issue does not implement #360's mutation-level group attribution or write-routing.**
  Skipping foreign edges is what makes that later work require only per-operation attribution
  (which every write handler already has) instead of mutation-level tracing through
  `Conn::executed_mutations` → `drain_mutations` → `wal_exec::wal_flush_*`.

## Out of Scope

- **#360's mutation-level group attribution and multi-writer WAL routing.** This issue removes the
  one code path that would have required it for entity merge; implementing the general mechanism
  is #360's own scope.
- **Eager repair of a foreign group's binding by another group's merge.** Explicitly rejected by
  the settled model: repair happens only in the owning group's own re-bind pass (ADR-0369), at its
  own re-bind time, never eagerly by another group's write.
- **Changes to ADR-0369's pointer resolution algorithm** beyond adding the `merged_into`-forwarding
  fixpoint step when resolution lands on a `Merged` row. The rest of the resolution/re-bind design
  (staleness check via `bound_at_seq`, `ambiguous` handling, etc.) is unchanged.
- **Choosing between JSON-attribute and relationship-table storage for `merged_into`** — left to
  Research/Plan (see Assumptions).

## Source References

- `crates/core/src/corrections.rs` — `merge_entities_inner` (self-loop branch, `has_directed_edge`
  duplicate branch, rewrite branch), `merge_entities` (canonical resolution, already refuses a
  `Merged` entity as canonical), `apply_same_as` (the second merge path, already carrying #368's
  group-scoped `has_directed_edge` fix), `MergeEdgeCounts`, `MergeEntitiesResult`, `MergePlan`,
  `AliasInfo`.
- `crates/core/src/handlers.rs` — `handle_merge_entities`'s IPC response construction (the
  `foreign_edges_rewritten`/`foreign_edges_deduplicated` fields to be replaced).
- `crates/core/src/db.rs` — the `Merged`-labelled-tombstone resolution comment (name-index scan
  fallback resolving *through* `Merged` rows, consistent with the winner-selection order).
- `crates/core/src/pointer.rs` — `BindingState` (`Bound`/`Unbound`/`Ambiguous`), `CrossGroupPointer`.
- `crates/core/src/cross_group.rs` — `resolve_endpoint`, `rebind_pointers` (the foreign group's own
  re-bind pass that FR-007–FR-009 extend with `merged_into` forwarding).
- [ADR-0368](../../docs/adr/0368-group-scoped-edge-dedup-in-merge.md) — the precedent this issue
  supersedes the *rewrite* half of; ADR-0368 must gain a note pointing at ADR-0371 in the same PR.
- [ADR-0369](../../docs/adr/0369-resolvable-cross-group-pointers.md) — the pointer model and
  `unbound` state this issue depends on and extends; its *Alternatives Considered* section
  deliberately deferred `merged_into` forwarding, a deferral this issue lifts.
- #360 — multi-stream WAL model; the settled decision record this issue's core rule follows from.
- #368 / PR #370 — scoped the dedup-drop; preserved the rewrite behavior this issue supersedes.
- #369 / PR #372 — the pointer model and the `unbound` state this issue depends on.

## ADR Requirements

- A new **ADR-0371** MUST record the settled model above (the three decisions, the one-rule
  collapse of the self-loop/duplicate/rewrite table, and the `merged_into`-forwarding-plus-
  tombstone-detection resolution for the name-changing edge case).
- It MUST supersede the *rewrite* half of ADR-0368's decision. ADR-0368 currently states that the
  rewrite behavior "was already correct and preserved a foreign edge's group ownership" — that is
  no longer the decision, and ADR-0368 MUST carry a note pointing at ADR-0371, landed in the same
  PR as the ADR-0371 addition.
- It MUST extend, not contradict, ADR-0369: the pointer model itself is unchanged; what changes is
  who is responsible for repairing a binding (the owning group, always) and when (at its own
  re-bind, never eagerly by another group's merge).
