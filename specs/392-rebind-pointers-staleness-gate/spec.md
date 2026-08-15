# Feature Specification: Rebind Pointers Must Repair Already-Unbound Cross-Group Pointers

**Feature Branch**: `fabrik/issue-392`
**Created**: 2026-08-13
**Status**: Specified
**Input**: User description: "`knowledge_rebind_pointers` skips pointers that are already known to be `unbound`, because its staleness gate keys on WAL position alone. After a purge followed by a checkpoint-bounded restore, cross-group pointers stay permanently unbound and the public API cannot repair them."

## Background

`knowledge_rebind_pointers` is the only public tool for repairing cross-group pointers in the
layered-graph model. Its staleness gate compares a pointer's `bound_at_seq` against the source
group's current `WalPosition.applied_seq` and skips re-resolution when the position looks
unchanged.

That gate answers the wrong question when a group's *data* has changed without its *position*
changing — which happens when an operator purges a group and then restores it to a
checkpoint-bounded WAL position that matches where the pointers were originally bound. Reproduced
end-to-end against `main` @ `d81ed79` (three groups; `C` a layer graph holding five cross-group
edges into `A` and `B`):

1. `knowledge_wal_mark_create {name: "pre_purge_A", group_id: "A"}` → checkpoint at `seq: 4`.
2. `knowledge_delete_by_group {group_ids: ["A"], confirm: true}` → C's three pointers into A
   correctly become `unbound` (the purge uses `rebind_pointers_forced` internally).
3. `knowledge_rebuild_from_wal {group_id: "A", from_seq: 0, to_seq: 4, force_clear: true}` → A's
   entities are restored (`mutations_replayed: 5`, 2 entities back).
4. `knowledge_rebind_pointers {source_group_id: "A"}` → `{"checked": 0, "bound": 0, "unbound": 0,
   "ambiguous": 0}`. Zero pointers examined. C's bindings are unchanged and still `unbound`, even
   though A's data is present and resolvable by name.

The restore returned A's position to the value it held when the pointers were originally bound, so
every pointer looks fresh to the gate and is skipped — while its recorded `binding_state` says
`unbound`. `rebind_pointers_forced` already handles this correctly (its doc comment describes this
exact shape) and is used internally by `group_purge`, but it is not reachable from the public API:
`knowledge_rebind_pointers`'s schema takes only `source_group_id`.

A pointer whose recorded `binding_state` is `unbound` or `ambiguous` is a known-broken fact, not a
"might have changed" question. The staleness gate is an optimisation for the latter question; it
must never suppress the former. Left unfixed, this is a hole in #365's checkpoint-recovery story:
restoring a group to a named good position silently leaves every layer graph pointing into it
broken, with no public repair, and the `{"checked": 0, ...}` response is indistinguishable from
"nothing needed doing."

A workaround exists (any subsequent write to the source group advances its position past
`bound_at_seq`, re-opening the gate), but it is undiscoverable and undocumented, and it does not
change that the repair mechanism itself declines to repair a state it recorded as broken.

**Priority**: filed against 0.14.0, then reprioritized to **0.13.1** — purge (#361) and
checkpoint-bounded restore (#365) both shipped in 0.13.0 and are both operator-facing, so this gap
is reachable through a documented, supported flow in the same release that shipped the feature it
breaks. It is an integrity issue in the layered-graph model's repair guarantee, not merely a
convenience gap in an uncommon hydration path.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator repairs pointers after a purge + checkpoint-bounded restore (Priority: P1)

An operator purges a group, restores it to a named checkpoint, and calls
`knowledge_rebind_pointers` against that group to repair every layer pointer that points into it —
without needing to know about, or trigger, any internal staleness-gate workaround.

**Why this priority**: This is the exact gap the issue reports. Without it, the public repair tool
can silently decline to repair pointers it has itself recorded as broken, and #365's
checkpoint-recovery story has no public completion step.

**Independent Test**: Run the reproduction above end-to-end: purge group A, restore A to the
`pre_purge_A` checkpoint, call `knowledge_rebind_pointers {source_group_id: "A"}`, and confirm C's
pointers into A are `bound` — with no intervening write to A.

**Acceptance Scenarios**:

1. **Given** a cross-group pointer whose `binding_state` is `unbound`, **When**
   `knowledge_rebind_pointers` is called against its source group, **Then** the pointer is
   re-resolved regardless of how `bound_at_seq` compares to the source group's current position,
   and becomes `bound` if the target is resolvable.
2. **Given** a cross-group pointer whose `binding_state` is `ambiguous`, **When**
   `knowledge_rebind_pointers` is called against its source group, **Then** the pointer is
   re-resolved regardless of `bound_at_seq`.
3. **Given** a cross-group pointer whose `binding_state` is `bound` and whose `bound_at_seq` is at
   or after the source group's current position, **When** `knowledge_rebind_pointers` is called,
   **Then** the pointer is skipped without re-resolution — the existing staleness optimisation is
   preserved for pointers that are not known-broken.
4. **Given** a `knowledge_rebind_pointers` call whose result includes pointers left unchanged,
   **When** the response is returned, **Then** it distinguishes pointers that were examined and
   found already correct from pointers that were skipped by the staleness gate, so a `checked: 0`
   result is never ambiguous about whether anything was actually looked at.

---

### User Story 2 - Internal callers keep their exact current behavior (Priority: P1)

`group_purge` calls `rebind_pointers_forced` today; #387's generation-change reset self-heal calls
the plain, non-forced `rebind_pointers`, relying on the fact that a genuine reset always advances
the source group's position past `bound_at_seq`, which reopens the existing staleness gate on its
own. Both must continue to behave exactly as they do now — this issue changes what the public,
gated tool examines, not either internal caller's outcome.

**Why this priority**: Regressing either path would break group purge or generation-change
recovery, both already shipped and depended on by downstream consumers.

**Independent Test**: Run the existing purge and generation-change/self-heal coverage; confirm
output and behavior are byte-for-byte unchanged.

**Acceptance Scenarios**:

1. **Given** a group purge, **When** `rebind_pointers_forced` runs internally as part of it,
   **Then** pointers into the purged group become `unbound` exactly as before.
2. **Given** a generation-change self-heal (#387), **When** it invokes the non-forced
   `rebind_pointers`, **Then** its behavior and outcome are unchanged from today.

---

### Edge Cases

- A pointer currently `bound` whose underlying target data has silently gone stale (e.g. renamed)
  without a position advance is not re-examined — this is the accepted tradeoff of preserving the
  staleness optimisation (FR-002 / SC-002), not a case this issue fixes.
- A `knowledge_rebind_pointers` call against a source group with a mix of `bound`,
  `unbound`, and `ambiguous` pointers must re-resolve only the latter two and still report a
  correct, non-ambiguous count for all three categories.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_rebind_pointers` MUST re-resolve pointers whose current `binding_state` is
  `unbound` or `ambiguous`, regardless of `bound_at_seq` relative to the source group's current
  position.
- **FR-002**: The existing staleness optimisation MUST be preserved for pointers whose current
  `binding_state` is `bound` — this issue must not turn every rebind call into a full
  re-resolution pass.
- **FR-003**: The `knowledge_rebind_pointers` response MUST distinguish "examined and already
  correct" from "skipped by the staleness gate," so a `checked: 0` result is never ambiguous about
  whether anything was actually looked at.
- **FR-004**: `group_purge` (which calls `rebind_pointers_forced`) and #387's generation-change
  reset self-heal (which calls the non-forced `rebind_pointers`) MUST both retain their current
  behavior unchanged.

### Key Entities

- **Cross-group pointer**: A reference from one group's graph (e.g. a layer graph) into another
  group, carrying a `binding_state` (`bound` / `unbound` / `ambiguous`) and a `bound_at_seq`
  recording the source group's WAL position at the time it was last resolved.
- **Source group WAL position**: `WalPosition.applied_seq` for the group a pointer targets; the
  staleness gate compares this against a pointer's `bound_at_seq`.
- **`knowledge_rebind_pointers` response**: Currently `{checked, bound, unbound, ambiguous}`;
  needs to additionally make clear which pointers were skipped by the staleness gate versus
  examined and left unchanged (FR-003).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The reproduction in Background ends with C's pointers into A `bound`, achieved via
  the public `knowledge_rebind_pointers` tool alone, with no intervening write to A.
- **SC-002**: A rebind call against a group whose pointers are all `bound` and whose position has
  not advanced performs no re-resolution work — the staleness optimisation still holds.
- **SC-003**: `knowledge_delete_by_group` and #387's generation-change reset self-heal behave
  exactly as they do today.

## Assumptions

- The preferred fix is to gate `knowledge_rebind_pointers` on binding state as well as
  `bound_at_seq` — always re-checking pointers currently `unbound`/`ambiguous`, while keeping the
  seq-based gate for pointers currently `bound`. This makes the public tool correct by default
  with no new API surface, and preserves the optimisation exactly where it remains valid.
- This touches the gating logic #387 shipped, so the change wants deliberate review rather than a
  drive-by edit.
- A regression test already exists: `crates/service/tests/mcp_multistream_e2e.rs` (merged in
  #395) asserts today's broken behavior in its phase-9 assertion, deliberately marked
  `TODO(#392)` rather than `#[ignore]`d. Its own failure message documents the fix: flip the
  assertion to require every pointer into A be `bound` and remove the `TODO(#392)` comment. This
  test runs in the default suite (~4s) and exercises the fix through the real MCP surface, not
  just at unit level.

## Out of Scope

- Exposing a `force` parameter on `knowledge_rebind_pointers`. Considered and explicitly rejected
  in favor of the binding-state gate: a caller should not need to know about the staleness gate to
  get correct default behavior.
- Any change to `rebind_pointers_forced`'s own behavior or its internal callers beyond confirming
  they are unaffected (FR-004 / SC-003).
- Concurrency/locking changes to the rebind path beyond what already exists.

## Source References

- Reproduction: end-to-end multi-stream harness (three groups, layer graph, purge → checkpoint
  restore → rebind) against `main` @ `d81ed79`; 9 of 10 checks pass, this issue is the tenth.
- #361 — group purge (uses `rebind_pointers_forced` internally).
- #365 — checkpoint-bounded restore (`knowledge_rebuild_from_wal`); this issue completes its
  public-repair story.
- #387 — generation-change reset self-heal (uses the non-forced `rebind_pointers`, relying on a
  genuine reset always advancing position past `bound_at_seq` to reopen the gate; FR-004 / SC-003
  cover it).
- #395 — `crates/service/tests/mcp_multistream_e2e.rs`, containing the `TODO(#392)` regression
  assertion this fix flips.
- Milestone 0.13.1: "Patch release. Integrity fix for the layered-graph model: cross-group
  pointers must be repairable through the public API after any operation that leaves them unbound
  (#392). Follows 0.13.0's multi-stream WAL and resolvable-pointer work."
