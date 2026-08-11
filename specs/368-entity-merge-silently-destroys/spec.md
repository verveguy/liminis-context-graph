# Feature Specification: Group-Scope Duplicate-Edge Detection During Entity Merge

**Feature Branch**: `fabrik/issue-368`
**Created**: 2026-08-11
**Status**: Draft
**Input**: User description: "Entity merge silently destroys cross-group edges: `has_directed_edge` and `get_full_edges_for_entity` are not group-scoped"

## Background

Entity merge (`corrections::merge_entities_inner`, `crates/core/src/corrections.rs:805`, invoked via the
`knowledge_apply_corrections` / `merge_entities` IPC method) reaches across `group_id` boundaries
in two places, and in one of them it **silently destroys edges belonging to a group it is not
authoritative for**.

For each of an alias entity's edges (collected via `Db::get_full_edges_for_entity`,
`crates/core/src/db.rs:1586`, which returns edges regardless of their `group_id`), the merge either
rewrites the edge onto the canonical entity, or drops it as a duplicate when `Db::has_directed_edge`
(`crates/core/src/db.rs:1638`) reports the canonical already has a directed edge with the same
`name` between the same endpoints. `has_directed_edge`'s query does not filter on `group_id` either
— it matches on entity UUIDs and edge name only.

The **rewrite** half is desirable and must be preserved: it copies the alias edge's own `group_id`
onto the newly created edge (`corrections.rs:865`), so a foreign group's edge is re-pointed at the
canonical and stays recorded under its own group. The **drop** half is not desirable: if *any*
group's edge happens to share the same `name` between the same (post-merge) endpoints, the alias
edge is invalidated (`corrections.rs:849-853`) and never recreated — regardless of which group that
matching edge belongs to. A merge performed in the context of one group can permanently delete
another group's assertion, with no record left behind of why.

Both an entity (`EntityRow`) and an edge (`RelatesToEdge`, backed by the `RelatesToNode_` shadow
node) carry their own, independently-set `group_id` field — an edge's `group_id` need not match the
`group_id` of the entities it connects. This is exactly what the reproduction below relies on: two
entities that both live in group `A` can be connected by an edge belonging to group `L`.

This defect is latent today only because nothing yet writes cross-group edges. It becomes live as
soon as more than one `group_id` shares a database — the multi-source replica topology in #360, and
the layered-graph work in the companion issue to this one (referenced in the original issue body but
not further specified here).

A second call site with the identical pattern exists at `corrections.rs:543`, inside
`apply_same_as` — the older, YAML-corrections-file-driven `same_as` merge path. It calls
`has_directed_edge` the same unscoped way, for the same reason (skip re-creating an edge that
already exists on the canonical), and is subject to the same failure mode. See FR-004 and Source
References.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A foreign group's edge survives a merge it didn't ask for (Priority: P1)

An operator merges two alias entities that live in group `A`. Unbeknownst to that operator, a
different group `L` sharing the same database has independently asserted an edge, with the same
relation name, between one of the aliases and a third entity that both groups can see. Today, that
merge silently deletes group `L`'s edge as a "duplicate" of an unrelated edge in group `A` — group
`L` loses data it never asked to have touched, and there is no error, log line, or count that
reveals it happened.

**Why this priority**: this is the exact reported failure. It is silent, destructive, and crosses a
tenant/group boundary that the rest of the system treats as an isolation guarantee (see
`specs/162-knowledge-merge-entities-collapse/spec.md`'s own FR-003/FR-004/FR-005, all of which scope
entity resolution to a single `group_id`). A merge operation performed for one group must not be
able to destroy another group's data as a side effect.

**Independent Test**: Build the reproduction from the issue directly against `merge_entities`/
`merge_entities_inner`: two groups `A` and `L` in one database; in `A`, entities `X1`, `X2` (aliases
of the same thing) and `Y`; edge `X2 --[rel]--> Y` in group `A`; edge `X1 --[rel]--> Y` in group `L`
(same relation `name`, different `group_id`). Merge `X1` into `X2` (group `A`). Assert group `L`'s
edge still exists and is queryable afterward — either untouched (still pointing at `X1`) or
re-pointed at `X2` while retaining `group_id: L` — but never invalidated with no replacement.

**Acceptance Scenarios**:

1. **Given** entities `X1`, `X2`, `Y` in group `A` with `X2 --[rel]--> Y`, and a separate edge
   `X1 --[rel]--> Y` in group `L`, **When** `X1` is merged into `X2` within group `A`, **Then**
   group `L`'s `rel` edge still exists afterward (either re-pointed to `X2 --[rel]--> Y` retaining
   `group_id: L`, or left as-is) — it is never invalidated without a replacement.
2. **Given** the same setup, **When** the merge completes, **Then** no new edge is silently created
   in group `A` that merges or conflates group `L`'s assertion with group `A`'s.

---

### User Story 2 - Same-group duplicate detection is unaffected (Priority: P1)

Duplicate-edge collapsing within a single group is the documented, intentional behavior of merge
(FR-009 in `specs/162-knowledge-merge-entities-collapse/spec.md`: "Directed edges that would be
duplicated on the canonical after merging ... MUST be deduplicated; one copy retained"). Fixing the
cross-group leak must not change this single-group behavior.

**Why this priority**: the fix touches the exact query path that same-group dedup already relies on
for every merge performed today (even in a single-group database). Any regression here breaks
existing, relied-upon behavior for the common case, not just the new multi-group case.

**Independent Test**: Within a single group, create entities `X1`, `X2` (aliases) and `Y`, with
`X2 --[rel]--> Y` and `X1 --[rel]--> Y` both in that same group. Merge `X1` into `X2`. Assert only
one `rel` edge from `X2` to `Y` remains, and `X1`'s copy is invalidated (not duplicated) — matching
today's behavior exactly.

**Acceptance Scenarios**:

1. **Given** two edges with the same `name` and (post-merge) endpoints in the *same* `group_id`,
   **When** a merge runs, **Then** exactly one is retained and the other is invalidated as a
   duplicate — unchanged from current behavior.

---

### User Story 3 - Merge result counts describe the merging group's own graph (Priority: P2)

An operator merging entities in group `A` reads `MergeEntitiesResult.edges_rewritten` and
`edges_deduplicated` to understand what the merge did. Today, if a foreign group's edge happens to
be touched, its rewrite or (pre-fix) deduplication is folded into the same counts as group `A`'s own
edges, so the numbers don't describe group `A`'s graph specifically.

**Why this priority**: this is a reporting/observability gap, not a data-loss bug — it matters less
than User Stories 1–2, but an operator cannot trust the merge result to reason about their own
group's graph state while it silently includes another group's activity.

**Independent Test**: Reuse the User Story 1 setup. Merge `X1` into `X2` within group `A`. Inspect
`MergeEntitiesResult`: group `A`'s own edge activity (if any) is reported separately from whatever
happened to group `L`'s edge as a side effect of the rewrite.

**Acceptance Scenarios**:

1. **Given** a merge that rewrites both a same-group edge and a foreign-group edge, **When** the
   result is returned, **Then** the counts make it unambiguous which edges belonged to the merging
   group and which did not — either by counting only the merging group's edges, or by reporting
   foreign-group edges in a separate, clearly-labeled count.

---

### Edge Cases

- **A foreign-group edge whose relation `name` and post-merge endpoints happen to already exist in
  that *same* foreign group on the canonical.** This is a legitimate same-group duplicate from that
  foreign group's own point of view and should still collapse — the fix scopes duplicate detection
  to "same group as the edge being evaluated," not "never dedup a foreign-group edge at all."
- **Self-loop edges arising from merging two ends of an existing edge.** Unaffected by this fix —
  this drop is already correct regardless of group and must continue to happen exactly as it does
  today (see `corrections.rs:838`, called out explicitly in the issue as out of scope for this fix).
- **Merged/tombstoned entities and name resolution.** `db.rs:1219`'s existing, deliberate decision
  that name resolution does not filter `Merged` tombstones is unrelated to this issue and must not
  change.
- **The `apply_same_as` correction path (`corrections.rs:489`, `same_as` YAML corrections)** has the
  identical unscoped `has_directed_edge` call at `corrections.rs:543`, for the identical reason. See
  FR-004.
- **A merge with no foreign-group edges involved.** Behavior and reported counts are unchanged from
  today — this fix only changes behavior when a cross-group edge is actually encountered.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Duplicate-edge detection during entity merge MUST only treat an alias's edge as a
  duplicate of an existing canonical edge when both edges share the same `group_id` — the alias
  edge's own `group_id`, not the `group_id` the merge operation is being run under. A cross-group
  edge (one whose `group_id` differs from any matching edge already on the canonical) MUST NOT be
  invalidated as a duplicate.
- **FR-002**: The existing rewrite behavior MUST be preserved: an alias edge whose `group_id`
  differs from the merging group is re-pointed onto the canonical entity and retains its own
  original `group_id` (not the merging group's `group_id`, and not a null/blank value).
- **FR-003**: `MergeEntitiesResult`'s `edges_rewritten` and `edges_deduplicated` counts MUST be
  unambiguous about which group's edges they describe. Either they MUST count only edges whose
  `group_id` matches the merge's own `group_id` parameter, or edges belonging to a different
  `group_id` MUST be reported through a separate, distinctly-labeled count — they MUST NOT be
  silently folded into the merging group's counts.
- **FR-004**: The same group-scoping fix MUST be applied to the `apply_same_as` correction path's
  duplicate check (`corrections.rs:543`), which has the identical defect for the identical reason —
  fixing only `merge_entities_inner` and leaving `apply_same_as` on unscoped duplicate detection
  would leave the same class of data loss reachable through the older correction-file path.
- **FR-005**: The self-loop drop behavior (an edge invalidated because merging its two endpoints
  would make it point to itself) MUST be unaffected by this fix — it is correct regardless of group
  and is out of scope for any behavior change.

### Key Entities

- **Entity**: A node in the graph, scoped to its own `group_id`, identified by UUID and name.
- **Edge (`RelatesToEdge` / `RelatesToNode_`)**: A directed, named relationship between two
  entities, carrying its own `group_id` independent of either endpoint entity's `group_id`.
- **Merge operation**: Collapses one or more alias entities into a canonical entity within a single
  `group_id` (the "merging group"), rewriting or deduplicating each alias's edges in the process.
- **Merging group**: The `group_id` under which a merge operation is invoked — used to resolve the
  canonical and alias entities by name, and used to classify each edge as same-group or foreign for
  result-count reporting under this fix. It is NOT the scope used for duplicate-edge detection —
  that scope is always the candidate edge's own `group_id` (see FR-001 and the Assumptions
  section).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The reproduction from the issue (two groups, cross-group edge on a shared endpoint,
  merge in one group) results in the foreign group's edge surviving the merge — either re-pointed at
  the canonical with its original `group_id` intact, or left untouched — in 100% of runs.
- **SC-002**: Existing same-group merge/dedup behavior (FR-009/FR-010/FR-011 from
  `specs/162-knowledge-merge-entities-collapse/spec.md`) is unchanged: duplicate same-group edges
  still collapse to one, self-loops still drop, invalidated edges are still excluded from rewrite.
- **SC-003**: A new regression test reproduces the issue's cross-group scenario, fails against the
  current (unfixed) code, and passes after the fix.
- **SC-004**: A new regression test confirms same-group duplicate detection is unchanged (User Story
  2), guarding against a fix that over-corrects into never deduplicating.
- **SC-005**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and
  `cargo test --release` all pass.

## Assumptions

- "The group being merged" (per the issue's acceptance criteria) means: duplicate detection for a
  given alias edge is scoped to that edge's own `group_id`, compared only against other edges
  sharing that same `group_id` on the canonical — not scoped to the `group_id` parameter the merge
  operation itself was invoked with. This is what makes the reproduction's outcome correct: group
  `L`'s edge is only ever compared against other group `L` edges, regardless of which group
  initiated the merge.
- Fixing `apply_same_as`'s identical call site (FR-004) is in scope for this issue, not deferred as
  follow-up, because it is the same defect reachable through a second, already-existing code path —
  not a new feature or a design question for the companion issue.
- Whether cross-group edges *should* exist at all, and what the long-term semantics of a
  cross-group rewrite ought to be (e.g. whether it should instead be left untouched for the foreign
  group to re-resolve), is explicitly the companion issue's question per the original issue body.
  This spec only requires that the destructive (silent-drop) half stops happening; it does not
  mandate "rewrite" over "leave untouched" as the resolution — either is acceptable as long as the
  foreign group's edge is not lost.
- No cross-group edges exist in any production or test database today (per the issue: "latent today
  only because nothing yet writes cross-group edges"), so this fix carries no data-migration
  concern — it only changes behavior for edges written after the fix, and for the specific
  regression test scenario.

## Out of Scope

- Cross-group *entity* merges (merging a canonical and alias that themselves live in different
  `group_id`s) — already explicitly out of scope per `specs/162-knowledge-merge-entities-collapse/spec.md`,
  and unaffected by this issue.
- Designing the long-term semantics of cross-group edges in general (e.g. whether a group should be
  notified when its edge is rewritten by another group's merge, or whether rewrite vs. leave-in-place
  is the right default) — deferred to the companion issue referenced in the original issue body.
- Any change to `db.rs:1219`'s `Merged`-tombstone name-resolution behavior.
- Any change to self-loop drop behavior.
- The multi-source replica topology work tracked in #360.

## Source References

- **liminis-context-graph#368**: this issue.
- **liminis-context-graph#360**: multi-source replica topology — the scenario that makes this defect
  reachable in practice.
- `crates/core/src/db.rs:1586` (`get_full_edges_for_entity`), `crates/core/src/db.rs:1638`
  (`has_directed_edge`).
- `crates/core/src/corrections.rs:805` (`merge_entities_inner`), `crates/core/src/corrections.rs:489`
  (`apply_same_as`, the older `same_as`-correction duplicate path with the identical defect at
  `corrections.rs:543`).
- `specs/162-knowledge-merge-entities-collapse/spec.md` — the original `merge_entities` spec;
  FR-009/FR-010/FR-011 define the same-group dedup/self-loop/invalidated-edge behavior this fix must
  preserve unchanged.
