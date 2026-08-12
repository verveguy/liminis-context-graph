# ADR-0368: Duplicate-Edge Detection During Merge Scopes by the Edge's Own `group_id`, Not the Merge's

**Status**: Accepted (rewrite branch superseded by [ADR-0371](0371-merge-never-writes-foreign-group-data.md))
**Date**: 2026-08-11
**Issue**: #368

> **Update (ADR-0371, #371)**: The Context/Decision text below states the rewrite branch "was
> already correct and preserved a foreign edge's group ownership." That is no longer the decision.
> Under the settled multi-stream WAL model, rewriting a foreign edge writes a mutation into another
> group's WAL stream — not free, and not permitted. ADR-0371 supersedes the rewrite half of this
> decision: a merge now skips every foreign-group edge entirely (self-loop, duplicate, and rewrite
> alike) rather than rewriting it. This ADR's duplicate-check group-scoping fix (`has_directed_edge`'s
> `group_id` parameter) is unaffected and stays correct — it simply becomes structurally
> unreachable for foreign edges, since they're skipped before that check runs.

## Context

Entity merge (`corrections::merge_entities_inner`, invoked via `knowledge_apply_corrections` /
`merge_entities`) rewrites each alias entity's edges onto the canonical entity, or drops an edge
as a duplicate when `Db::has_directed_edge` reports the canonical already has a directed edge
with the same `name` between the same endpoints. `has_directed_edge`'s query matched purely on
`(source_uuid, target_uuid, name)` — it did not filter on `group_id` at all.

Both an entity (`EntityRow`) and an edge (`RelatesToEdge`) carry their own, independently-set
`group_id`. An edge's `group_id` need not match either endpoint entity's `group_id`. This meant
that if any group's edge happened to share the same relation `name` between the same (post-merge)
endpoints as an alias's edge, the alias's edge was silently invalidated and never recreated —
regardless of which group the matching canonical edge belonged to. A merge performed in one group
could permanently destroy another group's assertion, with no error, log line, or count revealing
it happened. The rewrite half of the same code path (copying `old_edge.group_id` onto the newly
created edge) was already correct and preserved a foreign edge's group ownership when *no*
matching canonical edge existed — only the duplicate-detection half was destructive.

The identical pattern, for the identical reason, existed at a second call site:
`apply_same_as`, the older YAML-corrections-file-driven `same_as` merge path.

This defect was latent because nothing wrote cross-group edges yet. It becomes reachable once
more than one `group_id` shares a database (the multi-source replica topology tracked in #360,
and a layered-graph feature referenced by this issue but specified separately).

## Decision

Scope `has_directed_edge`'s duplicate check to the **alias edge's own `group_id`** — not the
`group_id` the merge operation itself was invoked under (the "merging group"). `has_directed_edge`
gained a `group_id: &str` parameter and an `AND rn.group_id = $group_id` predicate; both call
sites (`merge_entities_inner` and `apply_same_as`) pass `&old_edge.group_id`.

This distinction matters concretely: in the reporting scenario, entities `X1`/`X2` live in group
`A`, `X2`'s edge to `Y` is in group `A`, and it's `X1` — the entity being merged away — whose edge
to `Y` is in group `L`. Scoping the check by the *merge's own* `group_id` parameter (`A`) would
still compare `X1`'s group-`L` edge against `X2`'s group-`A` edge and reproduce the bug. Only
scoping by the edge's own `group_id` makes the check compare group `L` against group `L`
regardless of which group initiated the merge.

`get_full_edges_for_entity` (the query that collects an alias's edges to process) stays
unscoped — a merge must still see every one of an alias's edges, including foreign-group ones, so
it can rewrite them onto the canonical. Only the *duplicate-check* query needed the `group_id`
filter; scoping collection instead would have caused foreign-group edges to be silently skipped
rather than rewritten, a different but equally silent loss.

`merge_entities_inner`'s return type changed from a positional `(usize, usize, usize)` tuple to a
named `MergeEdgeCounts` struct, both because a fourth/fifth count needed to be added and because a
wider positional tuple is an easy transcription error at its two accumulation sites in
`merge_entities`. `MergeEntitiesResult`, `MergePlan`, and `AliasInfo` each gained `foreign_*`
counterpart fields (`foreign_edges_rewritten`, `foreign_edges_deduplicated`, etc.) reported
alongside the existing same-group counts, rather than folding foreign-group activity silently into
them — an operator reading a merge result needs to know whether a count describes only their own
group's graph or includes another group's activity as a side effect.

`apply_same_as` received only the group-scoping fix. It has no per-call counts today (it returns
a single action-label string), so the counts-reporting half of this decision doesn't apply there.

## Consequences

### Positive

- A merge performed for one group can no longer destroy another group's edge as an unintended
  side effect — the exact defect reported in #368.
- Same-group deduplication (the documented, intentional behavior merge already relied on) is
  unchanged: two edges sharing a `group_id`, `name`, and post-merge endpoints still collapse to
  one.
- `MergeEntitiesResult`/`MergePlan`/`AliasInfo` are now unambiguous about which group's edges
  their counts describe, without discarding visibility into cross-group activity.
- Both call sites with the same defect (`merge_entities_inner` and `apply_same_as`) received the
  fix; the older YAML-corrections-file path isn't left as a second, unfixed way to reach the same
  data loss.

### Negative / Residual risks

- The long-term semantics of cross-group edges in general — whether a group should be notified
  when its edge is rewritten by another group's merge, or whether "rewrite" is even the right
  default versus "leave in place" — is explicitly deferred to the companion issue referenced by
  #368. This fix only guarantees the foreign edge is not lost; it does not change which of the two
  acceptable outcomes (re-pointed vs. untouched) occurs.
- `apply_same_as`'s dedup-and-invalidate step still has no self-loop branch or per-call counts the
  way `merge_entities_inner` does — this ADR's counts-reporting half is `merge_entities`-specific
  by design, not a gap introduced here.

## Alternatives Considered

### Scope the duplicate check by the merging group's own `group_id` parameter instead

Rejected: does not fix the reported repro. The alias entity being merged away can itself carry a
foreign-group edge; scoping by the merge's own group would still compare that foreign edge against
whatever the canonical has in the *merge's* group, reproducing the same false-positive dedup.

### Filter `MergeEntitiesResult`'s existing counts to the merging group instead of adding separate foreign counts

Rejected: silently discards visibility into cross-group activity that did happen during the merge.
An operator debugging an unexpected foreign-group side effect would have no way to see it occurred
at all. Separate, distinctly-labeled counts cost little extra — the classification (`old_edge.group_id
== merging_group_id`) is already computed by the same loop that gained the group-scoping fix.

### Also scope `get_full_edges_for_entity` by `group_id`

Rejected: merge legitimately needs to see all of an alias's edges, including foreign-group ones,
in order to rewrite them onto the canonical. Scoping collection would silently skip foreign-group
edges rather than rewrite them — replacing one silent-loss bug with another.

## Related

- `crates/core/src/db.rs` — `has_directed_edge`, `get_full_edges_for_entity`.
- `crates/core/src/corrections.rs` — `merge_entities_inner`, `merge_entities`, `apply_same_as`.
- `crates/core/src/handlers.rs` — `handle_merge_entities`.
- `crates/core/tests/merge_entities.rs` — `test_cross_group_edge_survives_merge`,
  `test_same_group_dedup_still_collapses`.
- `specs/368-entity-merge-silently-destroys/spec.md` — this issue's spec.
- `specs/162-knowledge-merge-entities-collapse/spec.md` — the original `merge_entities` spec;
  FR-009/FR-010/FR-011 define the same-group dedup/self-loop/invalidated-edge behavior this
  decision must not regress.
- #360 — multi-source replica topology, the scenario that makes cross-group edges reachable in
  practice.
