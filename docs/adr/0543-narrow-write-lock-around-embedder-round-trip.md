# ADR-0543: Narrow `state.write_lock`'s Critical Section Around the Embedder Round Trip

**Date**: 2026-09-05
**Status**: Accepted

## Context

Since issue #444 (PR #487), `knowledge_assert_entity` and `knowledge_assert_relationship`
resolve whether their target entity/edge already exists *before* deciding whether to call the
embedder — a correct fix for a different bug (calling the embedder before knowing it was even
needed). A side effect of that reordering is that the create branch's embedder call happens
**while `state.write_lock` is held**, because the lock was acquired once, up front, and released
only after either the update or the insert completed. `write_lock` is the only thing preventing
two concurrent creates of the same not-yet-existing `(name, group_id)` entity, or the same
not-yet-existing `(source, target, predicate, group_id)` edge, from both resolving "not found"
and both inserting — so the old code bought that correctness guarantee by serializing *every*
writer in the process behind *any* in-flight embedder round trip.

ADR-0510 and #541 addressed the acute version of this hazard — an embedder that never
responds, which could previously wedge every write on the instance until the process restarted —
by adding request/connect timeouts (HTTP transport, ADR-0510) and a bound on the UDS transport
(#541). Both bound the *worst case*: with a 30-second default timeout, a single slow-but-healthy
embedder response could still serialize every writer behind it for up to 30 seconds. That was the
normal, non-degraded cost this issue removes: it was never about hangs — it was about a single
slow (but successful) embedder call being able to stall unrelated writes for the duration of that
call, every time a new entity or edge was created.

ADR-0510 named the structural fix explicitly as something it deliberately left out of scope, to
be filed as a follow-up (see its Out of Scope section): drop the lock for the embed call, then
re-acquire and re-resolve immediately before the insert, falling back to the update path if a
concurrent writer won the race in the meantime. This ADR is that follow-up.

There is no DB-level uniqueness constraint on `(name, group_id)` for entities or on the edge
identity tuple — `Entity.lookup_key`'s ART index is a lookup accelerator, not a uniqueness
constraint (`create_entity_lookup_key_index`). `write_lock` is therefore not a defense-in-depth
nicety here; it is the *only* mechanism preventing a duplicate insert, so any narrowing of its
scope has to preserve that guarantee exactly, not merely approximately.

## Decision

### Two separately-guarded passes per create branch

Both handlers now split their create branch into two independently-scoped critical sections
instead of one:

1. **Pass 1** (unchanged in spirit): acquire `write_lock`, resolve existence in a blocking
   section. If the target already exists, update it in place and flush — done, guard drops,
   return. If not found, the guard drops immediately at the end of this block; no embed call has
   happened yet.
2. **Embed** (only reached on not-found): compute the embedding(s) with **no lock held** —
   `state.embedder.embed(...).await` runs entirely outside any guard's scope.
3. **Pass 2** (new): re-acquire `write_lock`, re-run *the same* resolution logic in a fresh
   blocking section. If a concurrent writer created or altered the target in the meantime, take
   the update path against it (discarding the just-computed embedding and any embed error) and
   flush. Otherwise insert the new row with the computed embedding(s) and flush. Guard drops.

This is not a novel idiom for this codebase: `handle_status`'s WAL backfill phase already
acquires `write_lock` twice within one handler (a narrow `write()` guard for a backfill pass,
dropped, then a separate guard for the rest of the handler) — issue #543 generalizes an already-
accepted pattern rather than introducing one.

### Pass 2 reuses Pass 1's resolution logic verbatim, via a shared helper

The critical risk this design has to avoid is the two passes drifting apart — if Pass 2's re-
check were a simplified existence query instead of the exact same resolution used in Pass 1, a
future change to the rename-collision guard or `Merged`-tombstone forwarding could silently apply
to only one of the two call sites, reopening the exact race this ADR closes.

- `handlers::resolve_and_update_entity` extracts the resolve-then-update-in-place logic
  (`assert::resolve_entity_by_name`/`resolve_entity_by_uuid`, the rename-collision guard via
  `count_active_entities_by_name_ci`, `update_entity_core`, WAL flush) into one function called
  identically by both passes. It returns `Some(uuid)` when an existing row was found and updated,
  `None` when the caller must create one.
- `handlers::resolve_and_update_edge` does the same for edges
  (`find_active_relates_to_uuid`, `update_relates_to_core`, WAL flush).

`entity_uuid` is only ever supplied on Pass 1: `resolve_entity_by_uuid` either finds the row or
hard-errors, so a uuid-addressed call never resolves "not found" and therefore never reaches Pass
2 at all (confirmed during Research). Pass 2's entity re-check is always by name.

### Edge Pass 2 re-checks by already-resolved endpoint UUIDs, not by re-resolving names

`handle_assert_relationship`'s Pass 2 calls `resolve_and_update_edge` with the `source_uuid`/
`target_uuid` already resolved in Pass 1 — it does not re-run `resolve_entity_by_name` for either
endpoint. The spec's FR-004 and Acceptance Scenario 2 only require re-checking the edge identity
tuple `(source_uuid, target_uuid, predicate, group_id)`; whether an endpoint entity was itself
renamed or merged during the dropped-lock window is a separate, pre-existing hazard around
endpoint mutation that this issue does not introduce and does not claim to fix. Re-resolving both
endpoint names by string on every create would be unnecessary work for a race this issue's
acceptance criteria don't test.

### The race loser's response is indistinguishable from an ordinary update

If Pass 2 finds the target now exists (the caller lost the race), the just-computed embedding —
and any error encountered while computing it — is discarded rather than surfaced. The response
(`created: false`, the winner's uuid, no `embedding_warning`) is identical to what an ordinary
"assert against an already-existing entity/edge" call produces today. Nothing shortcuts the
loser's embed call itself — it still runs to completion (or failure) before Pass 2 begins; only
its result is thrown away when the update path is taken instead.

## Alternatives Considered

- **A single `Mutex`-guarded "reservation" table for in-flight creates**, keyed by identity
  tuple, so a second caller could detect an in-flight create without needing the full
  `write_lock`. Rejected: this would add a second, narrower synchronization primitive whose own
  correctness (cleanup on panic, cross-entity vs. cross-edge scoping) would need the same level of
  scrutiny as `write_lock` itself, for a marginal benefit over "just re-resolve under the existing
  lock" — the two-pass design reuses infrastructure that already exists and is already trusted.
- **A DB-level uniqueness constraint** on `(name, group_id)` / the edge tuple, making the insert
  itself fail on a collision instead of relying on lock discipline. Out of scope per the issue
  (see Out of Scope in the spec): changing the identity/upsert keys or adding schema-level
  constraints is a larger, separate change with its own migration and error-handling
  implications, and lbug/Kuzu's constraint support was not investigated as part of this issue.

## Out of Scope

- UDS transport timeout hardening (tracked separately, #541).
- Any change to embedder timeout values or configuration (ADR-0510).
- Retry-on-embedder-failure behavior — unchanged from the existing zero-vector-embedding fallback
  with a warning.
- Re-resolving relationship endpoint names on Pass 2 (see Decision above) — endpoint
  rename/merge races are a pre-existing, unrelated hazard.
- Any embedder call site that does not currently hold `state.write_lock` across the call
  (episode/batch ingestion, cross-group edge creation, search, dedup/canonicalization) — Research
  confirmed none of these share this hazard.

## Consequences

- A concurrent write that doesn't take the create branch (an update against an already-existing,
  unrelated entity/edge) is no longer gated by another in-flight call's embedder latency at all —
  not "bounded by a shorter timeout," but structurally independent of it.
- Two concurrent creates of the same not-yet-existing identity still yield exactly one row: the
  loser's call now does strictly more work (it still computes an embedding, which is then
  discarded) than it would in a design that could short-circuit the loser's embed call, but
  correctness is unaffected and this was an explicit, accepted tradeoff (Assumptions: "nothing
  shortcuts the loser's embedder call").
- `handle_assert_entity`/`handle_assert_relationship` each acquire `write_lock` up to twice per
  call instead of once — only on the create path; the update path (the common case for a
  long-lived graph) is unaffected and still acquires it exactly once.
- The rename-collision guard and `Merged`-forwarding walk are now defined in exactly one place
  each (`resolve_and_update_entity`/`resolve_and_update_edge`), reused by both passes, closing off
  the drift risk a hand-duplicated second check would have introduced.
- `hung_embedder_on_create_path_releases_write_lock_for_concurrent_write`
  (`crates/core/tests/concurrent_rw_integration.rs`) now asserts the concurrent update completes
  in well under the embedder's timeout window, not merely "bounded by" it — its narrative comment
  was rewritten to describe the new two-pass behavior rather than the old lock-held-until-timeout
  behavior.
