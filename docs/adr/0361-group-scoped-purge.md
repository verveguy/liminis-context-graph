# ADR-0361: Group-Scoped Complete Purge

**Status**: Accepted
**Date**: 2026-08-12
**Issue**: #361

## Context

The only structured deletion methods that existed before this issue — `remove_episode`,
`remove_episodes_by_source`, `remove_episodes_by_chunk_id` — `DETACH DELETE` only `Episodic`
nodes, by deliberate, three-times-documented design (issue #219, ADR-0038): "never `Entity`
nodes... this never invalidates the `NameIndex`." Scoping one of those to a `group_id` therefore
removes the episodes but **orphans every `Entity`/`RelatesToNode_` they produced** — the data
survives, still returned by FTS/vector search, but with its provenance stripped. The only
complete removal was `knowledge_clear_all`, which clears every group, not one.

A per-source read replica needs the opposite: drop one source's contribution and replay it
without touching co-resident groups. Without a real group-scoped purge, the only correct refresh
is `clear_all` + full rehydrate of every source — impractical at the WAL sizes ADR-0026 measured
(43,821 files, ~7h estimated replay) to pay for refreshing one source out of N.

**The disjointness invariant this issue originally relied on is conditional, and the condition
has already been retired.** The argument that a group-scoped purge cannot orphan another group's
data — "sources are bounded contexts, no cross-source edges, no shared entities, so a group's
subgraph is disjoint from every other group's" — holds only in the absence of cross-group edges.
ADR-0369 (issue #369, merged before this issue's Implement stage) deliberately introduces them: a
layer graph, its own `group_id`, with edges connecting entities across source groups by
construction. ADR-0369 anticipated this issue by name (its "FR-011... needed no new code"
section) and defined the `unbound` `binding_state` a cross-group pointer is left in when its
resolved entity disappears. This issue is the promised follow-through: a purge is a case where
resolved entities disappear on purpose, and must produce exactly the `unbound` state ADR-0369
already defined, not a bespoke one.

This ADR also has to reckon with a design assumption that turned out to be stale. The spec's
"Why it is tractable" section (and its Assumptions) describe purge-then-replay as replaying "that
group's own WAL directory from its own cursor," per issue #360's one-`group_id`-one-WAL-directory
model. **#360 was closed as superseded (`NOT_PLANNED`) and replaced by #378, which is still open
and unimplemented.** Today there is exactly one WAL directory and one `WalPosition` singleton row
per database instance — not one per group. This is the single biggest constraint the
implementation below works around (see FR-005).

## Decision

### FR-005 (reset the purged source's applied position): deferred to #378, not implemented here

`applied_seq` (ADR-0353) is a DB-wide monotonic cursor advanced only by episode-adds, shared by
every co-resident group. There is no per-group equivalent to reset. Writing to it after purging
just group A would misrepresent group B's up-to-date state to the backfill guard (#353) and
degraded-mode checks — a **worse** bug than the one FR-005 exists to prevent, since it would
silently corrupt a group the purge was never asked to touch.

**Decision: the purge does not touch `applied_seq` at all.** `knowledge_delete_by_group`'s
response includes `"applied_seq_reset": false` explicitly, so a caller cannot assume a position
reset happened. FR-005/SC-006 are documented here as blocked on #378 (per-group WAL positions).
Acceptance Scenario 3/SC-004 ("purge then replay that group's own WAL directory restores the
pre-purge state") is tested via the only replay mechanism that exists today: purge group A, run
the existing DB-wide `knowledge_rebuild_from_wal` against a WAL that predates the purge, and
confirm group A is restored while group B (never purged) is unaffected by the same replay. That
is a faithful proxy for the eventual per-group mechanism, not the mechanism itself — true
per-source refresh isolation ships with #378. This is the same "coarser first cut, explicit about
it" call ADR-0369 already made for `rebind_pointers`'s staleness gate.

### FR-009 mechanism: reuse `cross_group::rebind_pointers`, via a forced bypass of its staleness gate

`rebind_pointers(conn, source_group_id, ts)` already does almost exactly what FR-009 needs: for
every pointer whose `source_group_id` matches, it re-resolves via the name index and, when the
pointee is gone, lands on `BindingState::Unbound`, deletes the stale hop, and syncs the direct
compat rel. Reusing it means `Unbound`-transition logic continues to live in exactly one place.

Its staleness gate (skip when `bound_at_seq >= WalPosition.applied_seq`) would silently no-op in
the purge's own common case: the purge's deletes go through `wal_exec::wal_flush_ungrouped`,
which never advances `applied_seq`, so a purge run immediately after pointer creation would see
`bound_at_seq >= current` and skip every pointer — leaving them falsely `Bound` at UUIDs that
were just deleted. **Decision: `cross_group.rs` is refactored into a shared
`rebind_pointers_impl(conn, source_group_id, ts, force)`, with `rebind_pointers` kept as the
existing `force: false` public function (unchanged signature, unchanged behavior — every existing
call site and test is untouched) and a new `rebind_pointers_forced` as the `force: true`
wrapper**, used only by the purge.

This is safe specifically because of a structural fact about purge ordering: the purge always
deletes every entity in the purged group(s) *before* the forced rebind pass runs. `resolve_endpoint`
can therefore never find a match there — every pointer the forced pass checks is guaranteed to
resolve `Unbound`, never `Bound`. The self-loop/duplicate collision path Research flagged as a
risk to dry-run's exactness guarantee (`rebind_pointers`'s `invalidate_edge` side effect on a
resolution collision) requires a `Bound` resolution to trigger — which is therefore structurally
unreachable from a purge's forced rebind pass. That is what lets `knowledge_delete_by_group`'s
`dry_run` and real-run share one pre-mutation counting query (see FR-012 below) rather than
needing to separately predict which pointers might collide.

### FR-004 index strategy: `rebuild_name_index()` unconditionally; HNSW/FTS confirmed self-maintaining

This is the first code in the codebase that deletes an `Entity` or `RelatesToNode_` node, so
whether lbug's HNSW vector index and FTS index correctly self-maintain on `DETACH DELETE` was
genuinely untested — the concern FR-004 calls out. **Empirical finding (regression-tested in
`crates/core/tests/group_purge.rs::hnsw_and_fts_self_maintain_on_entity_delete`): both self-maintain.**
An entity inserted, confirmed findable by both `fts_search_entities` and `vector_search_entities`,
then removed via `delete_entities_by_group_ids`, is absent from both searches immediately
afterward with no explicit index rebuild. No `drop_vector_indexes`/`create_vector_indexes` or FTS
equivalent is needed around the delete step.

The `NameIndex` is different: it is an in-process structure with no per-entry removal API (only
`mark_untrusted()` and a full `rebuild(entries)`), so it does *not* self-maintain — a purge that
skipped rebuilding it would leave the index silently resolving names for deleted entities. The
purge therefore calls `conn.rebuild_name_index()` unconditionally after every real (non-dry-run)
purge, mirroring `knowledge_clear_all`'s existing pattern; a rebuild failure falls back
non-fatally to `mark_name_index_untrusted()`, the same `[NAME INDEX]` pattern used elsewhere in
`handlers.rs`. Regression-tested by
`purged_entity_name_no_longer_resolves_via_name_index`.

### FR-008: no new guard code — scoping every query to `group_id IN $gids` is the whole mechanism

A group-scoped purge must never delete a `RelatesToNode_` owned by a group outside the call, even
when it deletes an `Entity` that node is attached to via a hop relationship. This needed no
special-case logic: `delete_relates_to_by_group_ids` matches `WHERE rn.group_id IN $gids` —
a `RelatesToNode_` owned by another group is simply never named by that query, so it survives by
construction. Separately, `delete_entities_by_group_ids`'s `DETACH DELETE` on a purged-group
`Entity` removes only that entity's own incident relationships (including a hop into a foreign
`RelatesToNode_`), never the neighboring node itself — that is what destroys the hop (correct;
FR-009 exists to re-resolve it) while leaving the foreign node's row intact. Both are existing
Cypher/graph-model guarantees, not new invariants introduced by this issue; ADR-0369's own
regression test (`rebind_pointers_follows_reextraction_to_new_uuid_generation`, which hand-simulates
a purge) already exercised the same guarantee, and `crates/core/tests/group_purge.rs`'s
`purge_preserves_foreign_relates_to_node_and_leaves_pointer_unbound` re-exercises it against the
real purge implementation.

### FR-012 (dry-run plan): one counting pass, shared verbatim by both branches

Follows `corrections::merge_entities_inner`'s precedent: a single function computes per-group
entity/episode/edge counts and the cross-group unbound-impact tally *before any mutation*, in
both the `dry_run: true` and real-purge paths. The impact tally counts every live cross-group
pointer whose `source_group_id` is among the purged groups, broken out by the `RelatesToNode_`'s
own `group_id` (the owning/layer group) — not by which group is being purged, since FR-012 asks
"which other groups' pointers would be affected," and the owning group is what identifies "whose
data this is." Because every one of those pointers is guaranteed to resolve `Unbound` post-purge
(the ordering argument above), this pre-mutation count is not a prediction subject to drift — it
*is* the post-purge outcome, computed early. `dry_run` returns it directly; a real purge computes
it the same way, then goes on to mutate, and returns the identical value. SC-009's "counts match
exactly" therefore holds by construction, not by keeping two code paths in sync by hand.

### Atomicity across multiple `group_ids` (FR-002)

The delete-relates-to → delete-entities → delete-episodics → per-group forced-rebind sequence
runs inside one `conn.exec_transaction_control("BEGIN TRANSACTION")` / `COMMIT`, with an explicit
`ROLLBACK` on any error before the error propagates — the same `exec_transaction_control`
primitive `replay.rs`'s `flush_batch` already established for WAL-replay transaction boundaries,
now reused by a live-write path for the first time. A failure partway through purging
`["A", "B"]` therefore cannot leave A purged and B not (or half of either purged) — the whole
call rolls back as one unit.

## Consequences

### Positive

- `Entity`/`RelatesToNode_` deletion, cross-group unbinding, dry-run prediction, and multi-group
  atomicity are all built from existing primitives (`rebind_pointers`'s resolution machinery,
  `exec_transaction_control`, `rebuild_name_index`, the `MergePlan`-style dry-run shape) — no new
  design surface beyond the group-scoped count/delete Cypher templates themselves.
- The HNSW/FTS self-maintenance question — the one genuinely unverified assumption going in — is
  now answered and regression-tested, closing an open question for any future code that deletes
  indexed nodes.
- SC-009's exactness guarantee is structural (purge always empties the source group before any
  rebind resolution runs) rather than maintained by hand-written reconciliation logic that could
  drift out of sync.

### Negative / Residual risks

- FR-005/SC-004/SC-006 are not fully satisfiable until #378 lands. A caller relying on
  `knowledge_delete_by_group` for genuine per-source refresh isolation today must externally track
  which WAL entries predate a purge (e.g. via `knowledge_wal_mark_create`) rather than depending on
  `applied_seq` to do it — `applied_seq_reset: false` in the response is the explicit signal not
  to assume otherwise.
- `rebind_pointers_forced`'s reuse of `rebind_pointers_impl` means any future change to that shared
  implementation (e.g. new binding-state logic) automatically applies to the purge's forced pass
  too. This is intentional (one place for `Unbound`-transition logic), but means the purge's
  behavior is not independently pinned — a regression in `rebind_pointers` is a regression in the
  purge's FR-009 behavior as well, not an independent failure mode.
- The unbound-impact tally counts *every* live pointer whose `source_group_id` is being purged,
  including ones that were already `Unbound` before the purge (their state doesn't change, but
  they are still "left unbound" as a true post-purge fact) — not only ones that transition from
  `Bound`/`Ambiguous`. This is the simpler, more defensible reading of "would be left unbound" and
  keeps the pre-mutation query exact, but a caller expecting only newly-affected pointers should
  be aware the count is a snapshot of final state, not a diff.

## Alternatives Considered

### Bespoke inline unbind logic instead of reusing `rebind_pointers`

Rejected: would duplicate the resolution/hop-diffing/self-loop-handling logic `rebind_pointers`
already implements and tests, for no behavioral difference — the purge's forced pass needs
exactly what a "the source group just went empty" re-resolution produces, which is what
`rebind_pointers` already computes when given an empty group to resolve against.

### Changing `rebind_pointers`'s signature to add a `force` parameter directly

Rejected: would touch all 11+ existing call sites and their tests for a change relevant only to
the purge. A new `rebind_pointers_forced` wrapper sharing a private `rebind_pointers_impl` keeps
the existing public function's signature and behavior completely unchanged.

### Resetting `applied_seq` to the purged group's own "last known" value (approximated)

Rejected: there is no per-group `applied_seq` to approximate from — the singleton is genuinely
shared, so any write to it changes what every co-resident group's position means, not just the
purged one's. Leaving it untouched and documenting the gap was judged safer than a
partially-correct reset that could mask a real staleness signal for an untouched group.

## Related

- `crates/core/src/group_purge.rs` — `GroupPurgeCounts`, `UnboundImpact`, `PurgeCounts`,
  `purge_groups`.
- `crates/core/src/db.rs` — `count_entities_by_group_ids`, `count_episodics_by_group_ids`,
  `count_relates_to_by_group_ids`, `delete_entities_by_group_ids`, `delete_episodics_by_group_ids`,
  `delete_relates_to_by_group_ids`.
- `crates/core/src/cross_group.rs` — `rebind_pointers_impl`, `rebind_pointers`,
  `rebind_pointers_forced`.
- `crates/core/src/handlers.rs` — `handle_delete_by_group`.
- `crates/service/src/mcp/tools.rs` — `knowledge_delete_by_group` (admin scope).
- `crates/core/tests/group_purge.rs` — acceptance-scenario and SC-001–SC-009 coverage, including
  the HNSW/FTS self-maintenance probe.
- [ADR-0353](0353-persist-and-expose-applied-wal-seq.md) — `WalPosition.applied_seq`, the
  DB-wide singleton this issue's FR-005 cannot yet reset per group.
- [ADR-0368](0368-group-scoped-edge-dedup-in-merge.md) — the precedent for group-scoped
  self-loop/duplicate handling `rebind_pointers` (and so this purge) reuses.
- [ADR-0369](0369-resolvable-cross-group-pointers.md) — defines the `unbound` `binding_state` and
  the two-hop pointer model this issue's FR-008–FR-010 consume; explicitly named this issue as the
  planned follow-through for its FR-011 contract.
- #353 — the `DETACH DELETE`-`Episodic`-only invariant this issue is the deliberate, sole
  exception to.
- #360 — superseded (`NOT_PLANNED`) by #378; the spec's original "replay that group's own WAL
  directory" assumption depended on #360's model.
- #378 — multi-stream WAL (one `group_id`, one WAL directory); FR-005/SC-004/SC-006 are blocked on
  it landing.
- `specs/361-group-scoped-complete-purge/spec.md` — this issue's spec.
