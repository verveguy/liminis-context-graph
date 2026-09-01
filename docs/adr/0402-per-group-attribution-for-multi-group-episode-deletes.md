# ADR-0402: Per-Group Mutation Attribution for Multi-Group Episode Deletes

**Status**: Accepted
**Date**: 2026-09-01
**Issue**: #402

## Context

`knowledge_delete_chunk_episode` (`handle_delete_chunk_episode`, backed by
`Db::remove_episodes_by_chunk_id`) and `knowledge_delete_by_source` (`handle_delete_by_source`,
backed by `Db::remove_episodes_by_source`) can each delete `Episodic` nodes belonging to more than
one group in a single call, when the caller's `group_ids` names more than one group. Before this
issue, both handlers flushed the resulting mutations through exactly one group's WAL writer: the
caller's single named group when `group_ids` named exactly one, or `DEFAULT_GROUP_ID` ("liminis")
otherwise — regardless of how many groups the delete actually touched.

This is the identical defect shape [ADR-0385](0385-per-group-mutation-attribution-for-multi-group-writers.md)
fixed for `handle_delete_by_group` and `knowledge_rebind_pointers`: a multi-group mutation batch
collapsed into one WAL stream breaks [ADR-0378](0378-multi-stream-wal-per-group-directory.md)'s
premise that a group's own WAL stream must contain every mutation that changes its data, so a
group-scoped rebuild or recovery of that group alone reproduces its true state. Traced directly
against `main` (Research, issue #402): an episode created in group `b` is recorded on `b`'s own
WAL stream at creation time (`episode.rs`'s `wal_flush_chunk` call). A call to
`knowledge_delete_chunk_episode(chunk_id, group_ids: ["a", "b"])` issues one cross-group
`DETACH DELETE` and flushes it to `DEFAULT_GROUP_ID`'s stream instead of `b`'s, because
`group_ids` has two elements. A subsequent `knowledge_rebuild_from_wal {group_id: "b",
force_clear: true}` purges `b`'s live data and replays only `<wal_root>/b/*.jsonl` from `seq 0` —
which still contains the original `CREATE` but never the deletion, so the episode reappears. This
is confirmed empirically, not just by code reading: `crates/core/tests/wal_population.rs`'s
`test_delete_chunk_episode_multi_group_survives_group_scoped_rebuild` and
`test_delete_by_source_multi_group_survives_group_scoped_rebuild` both failed against pre-fix
`main` before the fix below was applied, and pass afterward.

Unlike `handle_delete_by_group`/`knowledge_rebind_pointers`, these two handlers were never among
[ADR-0378](0378-multi-stream-wal-per-group-directory.md)'s originally-enumerated four
FR-004-exempted call sites, and neither #385 nor #447's corrective passes covered them — #385
fixed exactly the two sites named at the time; #447 fixed the two database-wide maintenance
passes (`backfill.rs`/`canonicalize.rs`). `handle_delete_by_source`/`handle_delete_chunk_episode`
grew the identical fallback pattern independently, citing FR-004 by analogy in their own inline
comments, at a time (pre-#406) when `group_ids` was still optional and the fully-unscoped
("every group") case dominated discussion. [#406](https://github.com/verveguy/liminis-context-graph/issues/406)
(PR #412) made `group_ids` mandatory and non-empty for both handlers — exactly the precondition
ADR-0385 required to do per-group attribution ("a small, fully-known set of specific, identifiable
non-default groups per call") — which is what makes this issue's fix mechanically available using
infrastructure ADR-0385 already built and already proved in production.

## Decision

### Reuse ADR-0385's mechanism unmodified: `GroupedMutations` + per-group draining

No new type or `Conn` capability is introduced. `Db::remove_episodes_by_source` and
`Db::remove_episodes_by_chunk_id` change from one cross-group
`MATCH ... WHERE ep.group_id IN $gids ... DETACH DELETE` to a per-`group_id` loop — a
singleton-slice match query and delete per group, draining that group's mutations into a
`GroupedMutations` bucket (via `Conn::drain_mutations_into`) immediately after its own delete,
mirroring `group_purge::purge_groups`'s existing loop shape. Both methods' return type changes
from `Result<Vec<String>, Error>` to `Result<(Vec<String>, GroupedMutations), Error>`.

A single `WHERE group_id IN [...]` delete spanning several groups can't be retroactively split
into per-group WAL attribution after the fact — the same reasoning already recorded at
`group_purge.rs`'s per-group loop and restated in ADR-0385's Decision section applies unchanged
here.

### The per-group delete loop is wrapped in an explicit transaction

The pre-#402 single-query form was atomic for free. Splitting it into N per-group queries needs
the same explicit `BEGIN TRANSACTION`/`COMMIT`/`ROLLBACK` wrapping `purge_groups` already uses, so
a failure partway through the loop can't leave a partial cross-group delete (some groups' episodes
removed, others not). This is a deliberate strengthening beyond the minimum WAL-attribution fix,
justified by not regressing the atomicity the single-query form already had.

### `handlers.rs`: the same per-group flush loop as `handle_delete_by_group`

`handle_delete_by_source` and `handle_delete_chunk_episode` drop their
`target_group = match group_ids_owned.as_slice() { [single] => single.as_str(), _ =>
DEFAULT_GROUP_ID }` fallback entirely and replace it with the loop ADR-0385 already established:

```rust
for (group_id, mutations) in grouped {
    let seq = wal_exec::wal_flush_ungrouped(&state_c, &group_id, mutations);
    wal_exec::advance_wal_position(&conn, &group_id, seq, &state_c);
}
```

The single-group case is not special-cased — a `group_ids` slice of length one naturally produces
one bucket in `GroupedMutations`, so the loop collapses to exactly the same single-writer behavior
the two pre-existing single-group tests (`wal_population.rs`'s
`test_delete_by_source_with_single_group_routes_to_that_group` and
`test_delete_chunk_episode_with_single_group_routes_to_that_group`) already assert, unmodified.

`group_ids` naming the default group alongside one or more non-default groups is likewise not a
special case: the default group is just one more bucket in `GroupedMutations`, populated only with
the mutations that actually belong to it (`test_delete_chunk_episode_default_group_plus_non_default_attributes_separately`).

### In-place signature change, not a parallel `_grouped` API

`Db::remove_episodes_by_source`/`remove_episodes_by_chunk_id` have exactly one caller each
(`handle_delete_by_source`/`handle_delete_chunk_episode` respectively) — verified by grepping
`crates/core` and `crates/service` for every call site. Adding a parallel `_grouped` variant
alongside the originals, for a change with exactly one caller per method, would be needless
duplication; the signature change is made in place.

## Consequences

### Positive

- A named non-default group's own WAL stream now contains its own share of a multi-group delete's
  mutations — a group-scoped rebuild of that group alone no longer resurrects episodes the delete
  removed from it (FR-002, SC-001).
- `handle_delete_by_source` and `handle_delete_chunk_episode` are now the third and fourth call
  sites corrected under the ADR-0385 pattern, alongside `handle_delete_by_group` and
  `knowledge_rebind_pointers` — see the dated correction note in
  [ADR-0378](0378-multi-stream-wal-per-group-directory.md)'s FR-004 section.
- The per-group delete loop is wrapped in an explicit transaction, which preserves the atomicity
  the pre-#402 single-query form already had — splitting one `DETACH DELETE` into N per-group
  queries does not weaken the all-or-nothing guarantee across the whole call.
- No new `Conn` API surface, no new mutation-tagging mechanism — this issue is a direct
  application of already-existing, already-proven infrastructure to two more call sites, not a new
  design.

### Negative / Residual risks

- N single-group queries instead of one batched `IN $gids` query, bounded by `len(group_ids)` —
  operator-supplied and expected to be small (a handful of groups per call). Not a concern at
  realistic scale; no mitigation needed, matching ADR-0385's identical acceptance of the same
  tradeoff for `purge_groups`.
- Each per-group `wal_flush_ungrouped` call in the new flush loop is independently best-effort,
  same as `handle_delete_by_group`'s existing loop and the same tolerance ADR-0385 already
  documented and accepted: a crash between two groups' flushes can leave a later group's
  DB-committed deletion durably applied but unrecorded in its own WAL stream. Repaired by the next
  successful write to that group or a WAL rebuild; does not corrupt DB state.
- `handle_query_cypher` remains the sole FR-004-exempted call site — this issue does not touch it,
  and it has no fixed set of groups in scope to attribute to by design.

## Alternatives Considered

### Document the shared-default-group WAL attribution as safe (FR-003's other permitted outcome)

Rejected. The issue's own FR-003 permits "document why it's safe, no fix" as an alternative
outcome, but the empirical determination (Research, then reconfirmed by the pre-fix-failing
regression tests in this issue) is that restoration *does* occur — a group-scoped rebuild *does*
resurrect deleted episodes. That rules out the documentation-only path; a fix was required.

### A new `_grouped` variant alongside the existing `Db` methods

Rejected — see Decision above. No other caller exists for either method, so a parallel API adds
surface area for zero benefit over an in-place signature change.

## Related

- [ADR-0385](0385-per-group-mutation-attribution-for-multi-group-writers.md) — the precedent this
  issue directly reuses: `GroupedMutations`, `Conn::drain_mutations_into`, the per-group flush loop
  shape, and the transaction-wrapping rationale for a per-group delete loop.
- [ADR-0378](0378-multi-stream-wal-per-group-directory.md) — establishes the per-group WAL
  directory model; its FR-004 section carries this issue's dated correction note.
- [ADR-0361](0361-group-scoped-purge.md) — origin of `group_purge::purge_groups`'s per-group
  delete-loop-plus-drain pattern, reused unmodified by this issue's `Db`-layer fix.
- `crates/core/src/db.rs` — `Db::remove_episodes_by_source`, `Db::remove_episodes_by_chunk_id`,
  `GroupedMutations`, `Conn::drain_mutations_into`.
- `crates/core/src/handlers.rs` — `handle_delete_by_source`, `handle_delete_chunk_episode`'s
  per-group flush loops.
- `crates/core/tests/wal_population.rs` — regression coverage:
  `test_delete_chunk_episode_multi_group_survives_group_scoped_rebuild`,
  `test_delete_by_source_multi_group_survives_group_scoped_rebuild`,
  `test_delete_chunk_episode_default_group_plus_non_default_attributes_separately`.
- `specs/402-investigate-knowledge-delete-chunk/spec.md` — this issue's spec.
- [#406](https://github.com/verveguy/liminis-context-graph/issues/406) (PR #412) — made
  `group_ids` mandatory and non-empty for both handlers, the precondition this issue's fix
  depends on.
