# ADR-0371: Merge Skips Foreign-Group Edges Entirely; `merged_into` Forwarding Closes the Rename Gap

**Status**: Accepted
**Date**: 2026-08-12
**Issue**: #371

## Context

Three decisions about the multi-stream WAL model are settled by prior work:

1. **A logical graph, a `group_id`, and a WAL stream are one thing.** One WAL directory per
   group; an instance holds N writers (#360).
2. **Mutations never cross a stream boundary.** Every mutation in group G's WAL belongs to G —
   this is what makes each stream independently replayable.
3. **Cross-group references are resolvable semantic pointers owned by the referring graph**
   (ADR-0369). The referenced graph holds nothing about its referrers and never learns it has any.

Decision 2 is currently violated in exactly one place: entity merge. `corrections::merge_entities_inner`
(and its second call site, `apply_same_as`) collects an alias's edges via the deliberately
un-scoped `get_full_edges_for_entity`, then does one of three things to each edge, regardless of
which group owns it:

| branch | pre-#371 behavior |
|---|---|
| self-loop (`new_src == new_dst`) | `invalidate_edge`, no replacement, no count |
| duplicate (`has_directed_edge`) | scoped to the edge's own group by ADR-0368, invalidated |
| otherwise (rewrite) | rewritten onto the canonical, retaining its own `group_id` |

ADR-0368 fixed the *duplicate* branch's false-positive dedup (comparing a foreign edge against
the wrong group's canonical edge) and explicitly preserved the *rewrite* branch as "already
correct" — a merge repairing a foreign group's binding on that group's behalf looked like free
correctness at the time.

Under the settled multi-stream model it is not free. One connection, one `drain_mutations`,
mutations belonging to two groups — and therefore two WAL streams. Routing them into the right
streams needs mutation-level group attribution in a path (`Conn::executed_mutations` →
`drain_mutations` → `wal_exec::wal_flush_*`) that carries no group information at all today.
Engineering that attribution through for exactly one code path, ahead of #360's general
write-routing work, is more machinery than the problem needs.

The reachable, concrete failure mode that originally motivated this issue: source group `A` holds
entities `X1` and `Y`; layer group `L` asserts a cross-group edge `X1 --[rel]--> Y`
(ADR-0369-style pointer edge). Group `A` merges `X1` into `Y` — a legitimate consolidation once
`A` decides they're the same entity — which, from `L`'s perspective, collapses its edge into a
self-loop. Under the pre-#371 rewrite/self-loop branches, `L`'s edge was silently invalidated (or
rewritten) by `A`'s merge, with no error and, since #368's Review stage dropped the unused
`MergeEdgeCounts.self_loops` counter, not even a discarded count.

Fixing this surfaces an edge case ADR-0369 deliberately deferred. ADR-0369's re-resolution
recovers a merge silently and for free *only* when canonical and alias share a name — name
resolution deliberately resolves *through* `Merged` tombstones, so re-resolving the shared name
finds the still-active canonical. It does not recover a merge where the canonical has a
*different* name from the alias (e.g. alias `"IBM"` merged into canonical `"International
Business Machines"`): re-resolving `"IBM"` finds the tombstoned `"IBM"` row again, every time. A
foreign pointer that lands there is silently stale — `Bound` to a dead end — and indistinguishable
from a healthy binding unless something detects that the landing row is itself a tombstone.

Once merge stops eagerly repairing a foreign binding (this issue's core change), that
name-changing case becomes reachable in exactly the scenario the issue is about: a foreign group's
stale pointer, re-binding entirely on its own, with no help from the merge that invalidated it.

## Decision

### A merge in group G touches only edges whose `group_id == G`

All three branches collapse to one rule. `merge_entities_inner` and `apply_same_as` each gain an
early `continue` for any edge whose own `group_id` differs from the merging group's — before the
self-loop check, before the duplicate check, before the rewrite. A foreign edge is left completely
untouched: not rewritten, not invalidated as a duplicate, not invalidated as a self-loop. It is
counted (`foreign_edges_skipped`) so the merge's result is observable, but nothing about it is
written.

This is a simplification, not an addition — the loop gains one `continue`, nothing else.
ADR-0368's per-edge-group `has_directed_edge` scoping stays correct (every edge a merge still
processes belongs to the merging group by construction) but stops being load-bearing for foreign
edges, since they never reach that check anymore. **This supersedes the *rewrite* half of
ADR-0368's decision** — ADR-0368 states the rewrite branch "was already correct and preserved a
foreign edge's group ownership"; under the multi-stream model that write is exactly what must not
happen. ADR-0368 is amended with a note pointing here.

The foreign group re-resolves its own binding at its own re-bind time (ADR-0369's
`rebind_pointers`), becoming unbound (or re-bound) by derivation rather than by anyone writing to
it. This costs eagerness — a layer's binding is briefly stale until its own re-bind runs — but
ADR-0369 already establishes that an unbound window is routine rather than exceptional, so this is
consistent with the model, not a new concession.

`MergeEdgeCounts`'s two `foreign_*` fields (`foreign_rewritten`, `foreign_deduped`) collapse into
one `foreign_skipped` — the three shapes (would-be self-loop, would-be duplicate, would-be
rewrite) are no longer distinguishable outcomes, since none of them happen. The same collapse
propagates through `MergeEntitiesResult`, `MergePlan`, `AliasInfo`, and the
`handle_merge_entities` IPC response: `foreign_edges_rewritten`/`foreign_edges_deduplicated` (and
their plan/alias-level counterparts) are replaced with `foreign_edges_skipped`. No consumer outside
this crate references the removed field names (confirmed at spec time), so this is a
Rust/IPC-response schema change with no cross-language compatibility impact.

### `merged_into` forwarding, with tombstone detection as the mandatory floor

`corrections::merge_entities` (and `apply_same_as`) now records the canonical's UUID as a
`merged_into` key on every alias it tombstones, additively nested in the alias's `attributes` JSON
— the same zero-migration, additive-attributes pattern ADR-0369 established for
`cross_group_pointers` (`pointer::read_merged_into`/`write_merged_into`, mirroring
`read_pointers`/`write_pointers`). A `Merged` row is no longer a dead end — it resolves forward to
what it became, auditable for the first time.

`cross_group::resolve_endpoint` — the single function both `create_cross_group_edge` and
`rebind_pointers` call to turn `(source_group_id, endpoint_name)` into a binding — is the follower.
After its existing name-resolution-plus-ambiguity-check produces a winner, if that winner carries
the `Merged` label, `resolve_endpoint` follows `merged_into` forward, repeating while the target is
itself `Merged`, until it reaches a non-`Merged` fixpoint (`Bound` there) or forwarding fails
(`Unbound`). Forwarding fails, and only fails, when:

- no `merged_into` is recorded on a `Merged` row — the permanent, expected shape of every alias
  tombstoned before this feature shipped, not a transitional concern; or
- the target UUID is itself already visited this walk — a `HashSet<String>` of visited UUIDs
  guards against a cycle unconditionally, regardless of whether one is currently reachable through
  normal merge operation; or
- a `merged_into` target UUID doesn't resolve to any entity (a dangling reference).

Putting this in `resolve_endpoint` itself, rather than scoping it to `rebind_pointers` alone,
covers both of `resolve_endpoint`'s callers uniformly with one fix rather than two.

The ordering is the correctness-critical part: the "is this a dead end" check runs *before*
`resolve_endpoint` can report `Bound`, never as an afterthought. Without it, forwarding alone would
silently regress "landed on a tombstone" from an already-known, detectable failure mode (a stale
binding, indistinguishable in isolation but at least representing a real prior state) into "reported
bound and wrong" — worse than doing nothing, and exactly what the name-changing merge case above
produces if left unguarded.

## Consequences

### Positive

- A merge in any group can no longer write into another group's WAL stream, closing the one
  remaining gap in "mutations never cross a stream boundary" — #360's write-routing work now needs
  only per-operation group attribution (which every write handler already has), not mutation-level
  tracing through `Conn::executed_mutations` → `drain_mutations` → `wal_exec::wal_flush_*`.
- The original self-loop bug (a merge silently destroying a foreign layer's edge with no count) is
  fixed as a direct consequence of the more general rule, not as a special case.
- A merge that also renames the canonical no longer leaves a foreign pointer permanently,
  silently `Bound` to a tombstone — it forwards to the real canonical, or is honestly `Unbound`.
- `Merged` rows become forward-auditable ("what did this become") for the first time, independent
  of whether any foreign pointer ever needed to resolve through one.

### Negative / Residual risks

- A merge no longer eagerly repairs a foreign group's binding. Between the merge and the foreign
  group's next `rebind_pointers` pass, that group's pointer is stale (still resolving to the
  pre-merge alias). This is the explicit cost of the decision, mitigated by ADR-0369 already
  treating `Unbound`/staleness as a routine, expected window rather than an error state.
  `rebind_pointers` is not run automatically on a timer; an operator or scheduled job must trigger
  it for staleness to actually clear.
- Every alias tombstoned before this feature ships has no `merged_into` recorded. `resolve_endpoint`
  correctly reports `Unbound` for a foreign pointer that lands on one of these — an improvement
  over the pre-#371 alternative (nothing detected the tombstone at all under the old
  rewrite-eagerly model) — but a real-world database will carry this permanent population of
  orphan tombstones indefinitely; it is not a transitional data-migration gap to be "fixed" later.
- `apply_same_as` gained its own, independently-implemented foreign-skip and `merged_into`-write
  logic (it had zero foreign-edge awareness before this issue) rather than sharing code with
  `merge_entities_inner` — the two paths must be kept in parity by hand if either changes again.

## Alternatives Considered

### Make cross-stream writes work via mutation-level group attribution

Rejected as solving a bigger problem than this issue has. Threading group attribution through
`Conn::executed_mutations`/`drain_mutations`/`wal_exec::wal_flush_*` for exactly one code path,
ahead of #360's general write-routing design, is speculative machinery — #360 needs only
per-operation attribution once merge stops being the one path that needs mutation-level tracing at
all.

### Keep rewriting foreign edges, but attribute the mutation to the foreign group's own stream

Rejected for the same reason as above, plus a correctness question ADR-0369 already answered the
other way: decision 3 makes the *referring* graph (the foreign group) responsible for repairing
its own binding, never the referenced graph's merge. Writing into the foreign stream from inside
`A`'s merge would violate that ownership boundary even if the plumbing existed.

### Split `foreign_edges_skipped` by would-have-been shape (self-loop / duplicate / rewrite)

Rejected: the spec's User Story 1 AC5 asks only for a count of skipped foreign edges, and none of
the three shapes produce distinguishable behavior anymore — they're all "left untouched." A
finer-grained count would describe what *would* have happened under the old model, which is not
information this feature needs to expose.

### Store `merged_into` as a dedicated relationship table instead of a JSON attribute

Considered, since a rel table would be directly Cypher-traversable and the spec expressed a
preference for it "if the migration cost is acceptable." Rejected in favor of the JSON-attributes
route: `EntityRow.attributes` already exists, needs zero schema migration, and reuses the exact
`cross_group_pointers` precedent ADR-0369 established one issue earlier — a second, structurally
different mechanism for a functionally identical "forwarding reference" concept would cost more
than the Cypher-traversability was worth for what FR-005/FR-007 actually require (recorded and
followable to a fixpoint, not a particular storage shape).

### Bound hop-count cap instead of a visited-`HashSet` for the cycle guard

Rejected: a hop cap is an arbitrary constant that either rejects a legitimately long
non-cyclic chain or fails to catch a short cycle reliably depending on where the cap is set. A
visited-UUID set handles both correctly with no tuning parameter, at the cost of one small
allocation per resolution that lands on a `Merged` row (the uncommon path, not the hot one).

## Related

- `crates/core/src/corrections.rs` — `merge_entities_inner`, `merge_entities`, `apply_same_as`,
  `MergeEdgeCounts`, `MergeEntitiesResult`, `MergePlan`, `AliasInfo`.
- `crates/core/src/cross_group.rs` — `resolve_endpoint`, `rebind_pointers`.
- `crates/core/src/pointer.rs` — `read_merged_into`/`write_merged_into`, mirroring
  `read_pointers`/`write_pointers`.
- `crates/core/src/handlers.rs` — `handle_merge_entities`'s IPC response construction.
- `crates/core/tests/merge_entities.rs` — foreign-edge-untouched coverage for all three shapes,
  `merged_into` recording, and a `drain_mutations()`-level check that a merge produces no mutation
  for a foreign edge.
- `crates/core/tests/cross_group_pointers.rs` — `merged_into` forwarding (two-hop chain, orphan
  tombstone, cycle guard, name-changing merge) and the original self-loop layer-scenario
  regression.
- [ADR-0368](0368-group-scoped-edge-dedup-in-merge.md) — the duplicate-check group-scoping fix this
  issue builds on; its rewrite-branch decision is superseded here.
- [ADR-0369](0369-resolvable-cross-group-pointers.md) — the pointer model, `BindingState`, and
  `bound_at_seq` staleness gate this issue extends with `merged_into` forwarding; its own
  *Alternatives Considered* section deferred that forwarding to this issue.
- #360 — the multi-stream WAL model this issue's core rule follows from.
