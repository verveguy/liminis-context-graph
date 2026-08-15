# ADR-0392: `rebind_pointers`'s Staleness Gate Keys on Binding State, Not Only Position

**Status**: Accepted
**Date**: 2026-08-15
**Issue**: #392

## Context

`knowledge_rebind_pointers` (ADR-0369) is the only public tool for repairing cross-group
pointers. Its staleness gate, in `rebind_pointers_impl` (`crates/core/src/cross_group.rs`),
existed purely as an optimisation: skip re-resolving a pointer if the source group's applied WAL
position hasn't advanced since the pointer was last bound, since nothing could have changed. The
gate compared only two numbers — a pointer's `bound_at_seq` and the source group's current
`WalPosition.applied_seq` — and never consulted the pointer's own `binding_state`.

That gate answers the wrong question whenever a group's *data* changes without its *position*
changing. This happens on a documented, supported flow: purge a group (#361), then restore it to
a checkpoint-bounded WAL position (#365) that matches where the pointers into it were originally
bound. `group_purge` uses `rebind_pointers_forced` internally, which correctly and
unconditionally flips every affected pointer to `unbound`. The checkpoint-bounded restore then
rewinds the group's `applied_seq` back down to the pre-purge value — the same value recorded on
each pointer's `bound_at_seq`. A subsequent, public `knowledge_rebind_pointers` call sees
`bound_at_seq >= current` for every one of those pointers and skips all of them, even though
their recorded `binding_state` is `unbound`.

A pointer's `binding_state` is not a guess — it's the last thing `rebind_pointers_impl` itself
wrote. `unbound`/`ambiguous` means "this is known broken as of the last time anyone looked."
Skipping exactly those pointers because a position comparison looks unchanged means the repair
mechanism declines to repair a fact it recorded itself. The response
(`{"checked": 0, "bound": 0, "unbound": 0}`) gave no indication anything was wrong — it reads
identically to "there was nothing to do."

## Decision

**The gate in `rebind_pointers_impl`'s non-forced branch now skips a pointer only when its
current `binding_state` is `Bound` *and* `bound_at_seq >= current_seq`.** A pointer whose current
`binding_state` is `Unbound` or `Ambiguous` is always pushed into re-resolution, regardless of
`bound_at_seq`. `Bound` pointers keep the exact existing position-based skip, so a second call
against a group whose pointers are all correct and whose position hasn't moved is still a true
no-op — the original optimisation is preserved for the case it was built for.

```rust
if !force && existing.binding_state == BindingState::Bound {
    if let (Some(bound_at), Some(current)) = (existing.bound_at_seq, current_seq) {
        if bound_at >= current {
            counts.staleness_skipped += 1;
            continue;
        }
    }
}
```

A new `RebindCounts::staleness_skipped` field distinguishes "skipped because it looked fresh and
was already `Bound`" from "examined and found already correct" (folded into `checked`, as
before) and from "examined and re-resolved" (`bound`/`unbound`/`ambiguous`). This makes a
`checked: 0` result unambiguous: it now always means "every candidate pointer was `Bound` and
looked fresh," never "some pointers may have been silently skipped despite being known-broken."

`rebind_pointers_forced` — used by `group_purge` and #387's generation-change self-heal — passes
`force: true` and bypasses this entire block, so it is structurally unaffected. The two self-heal
call sites (`handlers.rs`) reach the shared `rebind_pointers_impl` through the *non-forced*
wrapper, so they do pick up the new binding-state check — this is additive-only (a reset can now
repair strictly more previously-known-broken pointers than before, never fewer) and does not
change the behavior the existing self-heal regression test exercises, since that test's pointer
is already `Bound` pre-reset.

## Consequences

- The public `knowledge_rebind_pointers` tool now correctly repairs pointers left `unbound` or
  `ambiguous` by any prior operation, even when the source group's position happens to look
  unchanged — closing the gap in #365's checkpoint-recovery story that this issue reported.
- `RebindCounts` gains a field (`staleness_skipped`), threaded through
  `handle_rebind_pointers`'s hand-built JSON response, `knowledge_rebind_pointers`'s `ToolSpec`
  description, and `docs/ipc-mcp-reference.md`. The two self-heal JSON sites that serialize the
  whole struct via `Serialize` pick it up automatically.
- No new public parameter. A caller does not need to know the gate exists, or that it was ever
  keyed on position alone, to get correct default behavior.

## Alternatives Considered

- **Expose a `force` parameter on `knowledge_rebind_pointers`.** Rejected: this would fix the
  symptom for a caller who already knows to pass it, but leaves the default call — the one
  reachable through the documented purge → restore flow — silently wrong. The spec explicitly
  requires correct-by-default behavior with no new API surface.
- **Always re-resolve every pointer, dropping the position-based gate entirely.** Rejected: this
  turns every `knowledge_rebind_pointers` call into a full re-resolution pass regardless of
  whether anything changed, which the existing idempotency test
  (`rebind_pointers_is_idempotent_with_no_intervening_change`) and FR-002/SC-002 both require to
  remain a no-op for pointers that are already `Bound` and unchanged.

## Related

- [ADR-0369](0369-resolvable-cross-group-pointers.md) — the pointer model and original
  (position-only) staleness gate this ADR corrects.
- [ADR-0378](0378-multi-stream-wal-per-group-directory.md) — the per-group `applied_seq` the gate
  compares against.
- [ADR-0385](0385-per-group-mutation-attribution-for-multi-group-writers.md) — the mutation
  bucketing this fix's changed resolutions still flow through unchanged.
- [ADR-0387](0387-wal-stream-generation-identity.md) — the generation-change self-heal that
  reaches this same gate through the non-forced wrapper.
- [ADR-0361](0361-group-scoped-purge.md) — group purge's use of `rebind_pointers_forced`,
  unaffected by this change.
- [ADR-0365](0365-wal-checkpoints-directory-per-name-store.md) — the checkpoint-bounded restore
  that, composed with purge, produces this issue's exact reproduction.
