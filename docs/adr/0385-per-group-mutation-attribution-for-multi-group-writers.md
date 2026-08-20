# ADR-0385: Per-Group Mutation Attribution for `delete_by_group` and `rebind_pointers`

**Status**: Accepted
**Date**: 2026-08-13
**Issue**: #385

## Context

[ADR-0378](0378-multi-stream-wal-per-group-directory.md) gave each `group_id` its own WAL
directory on the premise that every write handler names exactly one group at its flush site
(its own FR-004). Two handlers don't fit that premise — they are multi-group **by design**:

- `knowledge_delete_by_group` ([ADR-0361](0361-group-scoped-purge.md)) deletes a purged group's
  own data *and*, in the same transaction, force-rebinds pointers on a different, non-purged
  "owning" group's `RelatesToNode_` rows.
- `knowledge_rebind_pointers` ([ADR-0369](0369-resolvable-cross-group-pointers.md)) is invoked
  *for* a source group being re-resolved, but the mutations it produces land on the *owning*
  groups' edges — a different group (or groups) than the one named in the call.

Both routed their entire `drain_mutations()` output through
`wal_exec::wal_flush_ungrouped(&state, DEFAULT_GROUP_ID, …)`, citing ADR-0378's FR-004. That
citation was accurate as written but never meant to license this forever — FR-004 named an
accepted, documented limitation for call sites that couldn't attribute a single flush to one
group, not a blanket exemption for every multi-group operation. Reproduced against `main` @
`d5a3e14`: purging group A (with C, a layer group, owning a cross-group edge into A) followed by
a standalone `rebind_pointers` call created `<wal_root>/liminis/` — a group nothing was ever
otherwise written to — holding 18 mutations that actually belonged to A and C. Meanwhile `A/`
and `C/` were unchanged, so neither group's own stream described its own state. That breaks the
premise #378 exists to establish: a group's stream must be independently replayable, containing
every mutation that changes its data and no others.

This ADR covers only the two handlers above — the two of ADR-0378's four FR-004-exempted call
sites that mutate specific, identifiable non-default groups. Of the other two,
`handle_query_cypher`'s arbitrary-Cypher escape hatch has no fixed set of groups in scope at all
and remains under the original FR-004 rationale. `backfill.rs`/`canonicalize.rs`'s maintenance
passes were also originally left exempted here, but #447 later gave both a required `group_id`
parameter — narrower than what this ADR's per-mutation-boundary draining solves, since a call now
touches exactly one known group rather than an unbounded set — so they route directly to that
group's writer without needing this ADR's mechanism. See ADR-0378's FR-004 section, narrowed by
this ADR and by #447.

## Decision

### Drain at per-group boundaries, not once at the end

`Conn::executed_mutations` (drained via `drain_mutations()`) is a flat, order-preserving,
group-agnostic buffer — it has no per-mutation group tag, and adding one was explicitly rejected
by ADR-0378's own FR-004 (see Alternatives Considered). What both handlers already have, at
specific points in their own control flow, is the group each mutation about to be issued
belongs to: `group_purge::purge_groups` knows which `group_id` each delete targets, and
`cross_group::rebind_pointers_impl` has the owning group (`rn_group_id`) in scope for every
candidate it rewrites.

The fix drains the buffer at those existing boundaries — immediately after the writes for one
group (or one candidate) complete — into a local map, instead of draining once after the whole
operation finishes:

```rust
pub type GroupedMutations = BTreeMap<String, Vec<(String, serde_json::Value)>>;
```

`Conn::drain_mutations_into(&self, grouped: &mut GroupedMutations, group_id: &str)` drains and
merges into `grouped`'s `group_id` bucket in one step, a no-op if nothing was recorded (so a
group not actually mutated never gets an entry — see FR-004 below). `group_purge::purge_groups`
and `cross_group::rebind_pointers`/`rebind_pointers_forced` now return `(Counts,
GroupedMutations)` instead of bare `Counts`. Only `handlers.rs` (`handle_delete_by_group`,
`handle_rebind_pointers`, `clear_group_for_rebuild` — see below) touches `wal_exec`/`AppState`;
`group_purge.rs` and `cross_group.rs` stay pure DB-logic modules that return plain data, matching
ADR-0378's existing layering.

This is deliberately narrower than general mutation-level attribution: it changes *when* two
specific call paths drain the buffer, not what `Conn` records. `Conn::executed_mutations` gains
no new field, and no new per-mutation tagging is introduced anywhere else. ADR-0378 FR-004's
original rejection of that broader design stands untouched.

### `purge_groups`'s batched multi-group deletes become per-group singleton calls

Before this issue, `delete_relates_to_by_group_ids`/`delete_entities_by_group_ids`/
`delete_episodics_by_group_ids` each took the *entire* `group_ids` slice in one Cypher call
(`WHERE group_id IN [...]`). A single mutation line spanning several purge targets can't be
retroactively split into per-group attribution after the fact without either recording it in
more than one group's stream (violating "a mutation is written to the stream of the group whose
data it changes, **and to no other**" — FR-001) or re-issuing the delete once per group.

`purge_groups` now loops `group_ids`, issuing all three deletes as singleton-slice calls
(`&[gid]`) and draining into `grouped` after each group's own three deletes, before moving to the
next group. This trades one combined table scan per node type for N (N = number of purge
targets) — accepted because group purges are rare, low-cardinality admin operations, not a
per-request hot path.

Within one group's turn, the delete ordering is unchanged from the pre-#385 batched form and for
the same reason: `RelatesToNode_` first (only same-group edges — FR-008 of ADR-0361 never
touches a foreign group's `RelatesToNode_`), then `Entity` (`DETACH DELETE` destroys any hop into
a *surviving* foreign `RelatesToNode_`, which is exactly what leaves that edge needing rebinding),
then `Episodic` last.

### Delete loop and forced-rebind loop stay two separate passes, not interleaved

The forced-rebind pass (`for gid in group_ids { rebind_pointers_forced(conn, gid, ts) }`) still
runs as its own pass after every group's deletes complete, exactly as before this issue — this
ADR only changes what each pass returns, not their relative order. Interleaving per group
(delete A, rebind A, delete B, rebind B, …) would let group B's own forced-rebind pass write a
transient attribute update to an edge B itself owns, moments before B's own delete removes that
same row — wasted work, not incorrect, but avoided for free by keeping the existing two-phase
order.

### Flush timing is unchanged — still strictly after `COMMIT`

Producing `GroupedMutations` only moves data from `Conn`'s in-memory buffer into another
in-memory map; it writes nothing to disk. `purge_groups` still wraps its deletes and forced
rebinds in one `BEGIN TRANSACTION`/`COMMIT`/`ROLLBACK`, and the caller (`handlers.rs`) still only
flushes `GroupedMutations` to WAL after `purge_groups(...)?` returns `Ok` — i.e., strictly after
`COMMIT`. On `Err`, the `?` short-circuits before the flush loop and the accumulated map is
simply dropped, exactly mirroring the pre-#385 behavior where the whole `executed_mutations`
buffer was dropped on a rolled-back transaction. This is the same invariant Research flagged as
load-bearing: **the WAL is never written to for a mutation that isn't durably committed.**

A standalone `handle_rebind_pointers` call has no surrounding transaction — `cross_group`'s
writes commit via lbug's normal autocommit as they run — so there's no analogous "before commit"
window to protect there; `rebind_pointers_impl`'s per-candidate draining works identically in
both the standalone (autocommit) and forced-in-purge (transactional) contexts because grouping
and flush timing are decoupled: grouping happens as mutations are issued, flushing happens once,
after the caller's control flow decides it's safe to.

### `handlers.rs`: one `wal_flush_ungrouped` call per group instead of one call to the default group

`handle_delete_by_group`, `handle_rebind_pointers`, and `clear_group_for_rebuild` each replace
their single `wal_exec::wal_flush_ungrouped(&state, DEFAULT_GROUP_ID, conn.drain_mutations())`
call with a loop over the returned `GroupedMutations`:

```rust
for (group_id, mutations) in grouped {
    wal_exec::wal_flush_ungrouped(&state_c, &group_id, mutations);
}
```

`wal_flush_ungrouped` already no-ops on an empty `Vec` without touching
`AppState::with_wal_writer` (ADR-0378), so a group with no entry in `GroupedMutations` never gets
a directory created — FR-004 ("a group never written to must not acquire a WAL directory as a
side effect of another group's operation") and the zero-unbound-pointers edge case hold by
construction, with no new logic needed at the flush site itself.

### `clear_group_for_rebuild` gets the identical fix, though not named by the issue

`clear_group_for_rebuild` (`handlers.rs`, used by `knowledge_rebuild_from_wal`'s `from_seq: 0`
path) calls `group_purge::purge_groups` directly and had the exact same bug — its forced rebind
can touch a foreign, non-purged group's `RelatesToNode_` rows, and it flushed everything to
`DEFAULT_GROUP_ID`. It wasn't named by the issue or found by Research (which searched only the
two IPC-exposed handlers), but `purge_groups`'s signature change is unconditional — every caller
must update to compile — and leaving this call site flattening a correctly-computed
`GroupedMutations` back onto the default group would have reintroduced this exact bug, reachable
through a different entry point, for zero design cost avoided by excluding it. It receives the
same per-group flush loop.

### `GroupedMutations`'s `BTreeMap` iteration order

Flushing a call's buckets in `BTreeMap`'s (i.e., lexicographic `group_id`) order has no
correctness impact — each group's `WalWriter` and on-disk stream are fully independent
(ADR-0378), so the relative order two different groups' mutations are appended to their own,
unrelated streams in has no observable effect. Noted here only so a future reader doesn't wonder
whether flush order is meaningful; it isn't.

## Consequences

### Positive

- `A/`'s stream contains A's own purge deletions; a fresh database rebuilt from `A/` alone no
  longer resurrects purged data (FR-005/SC-003).
- `C/`'s (the owning group's) stream contains its own pointer rebind mutations — both from a
  purge's forced rebind and from a standalone `rebind_pointers` call — regardless of which group
  was named as `source_group_id` in the call (FR-002/FR-003).
- `liminis/` is no longer conjured into existence by an operation that never legitimately touches
  the default group (FR-004).
- `group_purge.rs` and `cross_group.rs` remain free of any `wal_exec`/`AppState` dependency — the
  layering ADR-0378 established is preserved, not compromised to thread group context through to
  the flush site.

### Negative / Residual risks

- Multi-group purges now issue N Cypher calls per node type instead of 1 (N = number of purge
  targets), trading a combined table scan for N narrower ones. Accepted: purges are rare,
  low-cardinality admin operations, not a request-volume-sensitive path.
- `GroupedMutations` adds a second return value to `purge_groups`/`rebind_pointers`/
  `rebind_pointers_forced`; every call site (including ~14 test call sites) had to be updated to
  destructure the new tuple. One-time churn, not an ongoing cost.
- The per-group flush loop in `handlers.rs` (`for (group_id, mutations) in grouped { wal_exec::wal_flush_ungrouped(...) }`)
  calls `wal_flush_ungrouped` once per attributed group, and each call is independently
  best-effort — `wal_flush_ungrouped` already logs-and-continues on a write failure rather than
  propagating it (see `wal_exec.rs`'s module doc: "WAL failures are non-fatal: the DB write
  already committed; the WAL is a recovery artifact, not a write gate"). Before this ADR, one
  purge/rebind call made one such fallible call touching one stream; after, it can make N,
  touching N streams. A crash or write failure after an earlier group's flush succeeds but before
  a later group's flush runs (or itself fails) leaves that later group's DB-committed mutations
  durably applied but unrecorded in its own WAL — the same class of divergence
  `wal_flush_ungrouped` already tolerated within a single group's mutation list, now possible
  across groups within one call as well. Accepted for the same reason the existing per-mutation
  tolerance is accepted: the WAL is a recovery aid, not the durability boundary (the DB commit
  already happened before any flush in this loop runs), and a missed WAL entry is repaired by the
  next successful write to that group or a WAL rebuild — it does not corrupt DB state.
- This ADR does not fix [#383](https://github.com/verveguy/liminis-context-graph/issues/383)
  (`applied_seq` never advances for `wal_flush_ungrouped`-routed writes) — a related but
  independent defect on the same flush path, explicitly out of scope for this issue. The two
  don't fix each other: this ADR changes *which* group each mutation is attributed to, not
  whether `applied_seq` is bumped afterward.
- `handle_query_cypher` is unchanged and continues routing to the default group under ADR-0378's
  original FR-004 rationale (narrowed, not repealed, by this ADR) — see that ADR's FR-004 section
  as amended. `backfill.rs` and `canonicalize.rs` were also unchanged when this ADR shipped, but
  #447 later gave both a required `group_id` and routed them to that group's own writer directly —
  see ADR-0378's FR-004 section for the #447 correction.

## Alternatives Considered

### General mutation-level attribution (tagging each mutation with its group as it's recorded)

Rejected, per ADR-0378's FR-004, which stands unchanged: `Conn::executed_mutations` would need a
new per-entry group field threaded through every `exec_params`/`raw_query`/`cypher_query` call
site in the codebase, for a need that exists at exactly two call sites. Per-boundary draining
gets the same per-group correctness for the two handlers that actually need it, with zero change
to the shared recording path every other writer already relies on.

### A non-destructive `Conn` peek/length accessor (record `(group_id, start_idx, end_idx)` boundaries, split after one real `drain_mutations()`)

Considered (Research's option (a)). Would work, but adds new `Conn` API surface (a peek that
doesn't consume the buffer) for a need fully met by calling the existing, already-safe-to-call
`drain_mutations()` more often. Per-boundary draining needs no new primitive on `Conn` beyond the
thin `drain_mutations_into` convenience wrapper.

### Flush immediately inside `cross_group.rs` when `rn_group_id` becomes known

Rejected: `rebind_pointers_impl` is shared by the standalone call (autocommit, no surrounding
transaction) and the forced-in-purge call (inside `purge_groups`'s `BEGIN TRANSACTION`). Flushing
to disk as soon as a group is known would violate the "no flush before commit" invariant for the
forced-in-purge case — a rollback after a mid-transaction flush would leave WAL entries for
mutations that were never durably applied. Decoupling *grouping* (done eagerly, in memory) from
*flushing* (done once, by the caller, after it knows the mutations are durable) is what lets one
implementation serve both contexts safely.

## Related

- [ADR-0378](0378-multi-stream-wal-per-group-directory.md) — establishes the per-group WAL
  directory model this issue restores; its FR-004 section is narrowed by this ADR to remove
  `handle_delete_by_group`/`handle_rebind_pointers`/`clear_group_for_rebuild` from the
  default-group-routing exemption list.
- [ADR-0361](0361-group-scoped-purge.md) — introduces `purge_groups`'s forced-rebind pass; its own
  text already flagged this issue's resolution as deferred to #378/#385.
- [ADR-0369](0369-resolvable-cross-group-pointers.md) — defines the pointer model
  (`CrossGroupPointer`, `resolve_endpoint`, `bound_at_seq`) `rebind_pointers_impl` implements; the
  owning-group attribution here rides on the same per-candidate loop.
- `crates/core/src/db.rs` — `GroupedMutations`, `Conn::drain_mutations_into`.
- `crates/core/src/group_purge.rs` — `purge_groups`'s per-group delete loop and forced-rebind
  merge.
- `crates/core/src/cross_group.rs` — `rebind_pointers_impl`'s per-candidate draining at its three
  write-exit points.
- `crates/core/src/handlers.rs` — `handle_delete_by_group`, `handle_rebind_pointers`,
  `clear_group_for_rebuild`'s per-group flush loops.
- `crates/core/tests/group_purge.rs`,
  `crates/core/tests/cross_group_incremental_replay.rs` — regression coverage for SC-001 through
  SC-004 and the Edge Cases scenarios.
- `specs/385-delete-by-group-and/spec.md` — this issue's spec.
- Related: [#383](https://github.com/verveguy/liminis-context-graph/issues/383) — the
  independent `applied_seq` defect on the same flush path, not fixed by this ADR.
