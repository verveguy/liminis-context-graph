# Feature Specification: Bounded WAL replay — `to_seq` upper bound for `knowledge_rebuild_from_wal`

**Feature Branch**: `fabrik/issue-362`
**Created**: 2026-08-08
**Status**: Draft
**Input**: User description: "`ReplayOptions` (`crates/core/src/replay.rs`) and `knowledge_rebuild_from_wal` expose a lower replay bound (`from_seq`) but no upper bound. Every rebuild runs from the chosen start point to the end of the WAL, so there is no way to replay 'up to just before seq N' — meaning a bad mutation that is itself recorded in the WAL cannot be excluded from any rebuild. Add an optional `to_seq` upper bound, inclusive, so `knowledge_rebuild_from_wal {from_seq, to_seq, force_clear}` replays only lines where `from_seq <= seq <= to_seq`. Omitting `to_seq` must reproduce today's unbounded behaviour exactly."

## Background

The WAL is a complete backup of the graph — every state the graph has ever been in is recoverable from it, in principle. But today's replay can only restore *forward to the end of the WAL*, never up to an arbitrary earlier point. `ReplayOptions` (`crates/core/src/replay.rs:181`) exposes `from_seq` (a lower bound: skip lines with `seq < from_seq`) with no counterpart upper bound, and the corresponding filter (`replay.rs:430`) is one-sided.

This matters concretely for the recovery scenario reported in community discussion #207 (section 4): an operator runs a mutation that corrupts their graph. That mutation is not somehow outside the WAL — it was a mutation, faithfully recorded like every other. So today:

- `knowledge_rebuild_from_wal {from_seq: 0, force_clear: true}` replays everything, **including the mistake that corrupted the graph in the first place**.
- There is no way to express "replay up to just before the bad mutation's seq."

The WAL holds every historical state, and exactly one — the current one — is reachable. A single bad mutation is therefore unrecoverable through replay alone. (#204 removed the specific null-and-recanonicalize dance that triggered the original report, but the underlying property — a bad mutation cannot be undone — is unchanged.)

`to_seq` is also the primitive that both checkpointing ("this graph was known-good at seq N") and snapshotting (materialized state at seq N, independently reproducible) would depend on to replay to a specific point — but building either of those is explicitly out of scope here; this issue ships the bounded-replay primitive on its own merit (recovery, inspection, bisection — "what did this graph look like before X").

This is additive: `ReplayOptions` gains one optional field, and the existing `from_seq`-only filter gains a second, symmetric comparison. The three live call sites that construct `ReplayOptions` from request parameters (`handlers.rs:1688` sync reload, `handlers.rs:1871` and `handlers.rs:2002` background/streaming reload) all need to thread the new parameter; the startup-recovery construction site (`recovery.rs:285`) is deliberately excluded — degraded-mode startup recovery always wants the full WAL tail.

**Non-durability is a first-class part of this issue's scope, not an afterthought.** After a bounded rebuild to seq N, WAL entries beyond N are still on disk — they are simply not applied. The database is now deliberately behind its own WAL. Nothing about a bounded rebuild is destructive or persistent: a later unbounded rebuild, or a `from_seq` resume that covers the excluded range, will reapply everything that was excluded, including a previously-excluded bad mutation. This issue ships that non-destructive behavior deliberately (see Assumptions) and requires it to be documented plainly, so operators do not mistake a bounded rebuild for a durable rollback. Durable rollback (truncating/archiving the WAL tail, or recording a persistent skip-range) is explicitly a follow-on, not part of this issue.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Exclude a bad mutation from a rebuild (Priority: P1)

An operator has identified the WAL sequence number of a mutation that corrupted their graph. They rebuild the graph from the WAL, bounding replay to just before that mutation, so the corrupting mutation is never applied.

**Why this priority**: This is the entire motivation for the issue — the community-reported scenario (#207 section 4) where a bad, WAL-recorded mutation was otherwise impossible to exclude from any rebuild.

**Independent Test**: Populate a WAL with a sequence of mutations including one deliberately "bad" one at a known seq. Run `knowledge_rebuild_from_wal {from_seq: 0, to_seq: <seq before the bad mutation>, force_clear: true}` and confirm the resulting graph reflects every mutation up to but not including the bad one.

**Acceptance Scenarios**:

1. **Given** a WAL containing a mutation at seq `N` that should be excluded, **When** `knowledge_rebuild_from_wal` is called with `to_seq: N - 1`, **Then** the rebuilt graph reflects all mutations with `seq <= N - 1` and none with `seq >= N`.
2. **Given** a bounded rebuild has just completed with `to_seq: M`, **When** a client calls `knowledge_status`, **Then** the reported applied WAL position equals the highest seq actually replayed (`<= M`), not the WAL's on-disk maximum.
3. **Given** a bounded rebuild has just completed, **When** a subsequent ingest writes a new WAL entry, **Then** it is assigned a sequence number above the true on-disk WAL maximum — never a seq that collides with or overwrites an existing, unapplied WAL entry.

---

### User Story 2 - Inspect historical graph state (Priority: P2)

An operator or developer wants to know what the graph looked like at an earlier point in its history — for diagnosis, bisecting when a problem was introduced, or auditing — without permanently altering the current database.

**Why this priority**: A direct, lower-stakes consequence of the same primitive; valuable independent of the recovery use case, but not the scenario that motivated filing the issue.

**Independent Test**: Run a bounded rebuild against a scratch/disposable database directory (or `dry_run: true` against the live one) with `to_seq` set to an earlier point in a known WAL, and confirm the resulting entity/relationship counts match what existed at that point in history.

**Acceptance Scenarios**:

1. **Given** a WAL with a known history of mutations, **When** `knowledge_rebuild_from_wal` is called with `dry_run: true` and a `to_seq` earlier than the WAL's end, **Then** the returned replay statistics reflect only lines with `seq <= to_seq`, and the live database is left untouched.

---

### Edge Cases

- **`to_seq` omitted**: behaves exactly as today — unbounded replay to the end of the WAL. This must be a true no-op change for every existing caller that does not pass `to_seq`.
- **`to_seq < from_seq`**: rejected before any WAL line is read or any mutation applied; the database is left untouched. The error is as specific as the existing `from_seq` validation errors (e.g. distinguishing "not a non-negative integer" from "upper bound below lower bound").
- **`to_seq` present but not a non-negative integer** (boolean, negative number, non-integer, string, etc.): rejected with the same class of validation error already used for `from_seq`.
- **`to_seq` equal to `from_seq`**: valid — replays at most the single line at that exact seq, if one exists.
- **`to_seq` greater than or equal to the WAL's true on-disk maximum seq**: valid, and behaviorally identical to omitting `to_seq` — there is nothing beyond the maximum to exclude.
- **`to_seq` combined with `dry_run: true`**: the statistics preview is bounded by `to_seq` exactly as a real replay would be; no mutation occurs regardless.
- **`to_seq` combined with `from_seq: 0, force_clear: true`** (the corrupted-mutation recovery scenario): the existing non-empty-database fail-fast/`force_clear` gate is keyed on `from_seq` alone and is unaffected by `to_seq` — a bounded full rebuild clears and replays exactly as an unbounded one would, just stopping earlier.
- **Background/streaming rebuild (progress-token path)**: a caller-supplied `to_seq` is honored identically whether or not MCP progress notifications are in use — both `handlers.rs:1871` and `handlers.rs:2002` construct `ReplayOptions` and must both thread the field.
- **Startup/degraded-mode recovery** (`recovery.rs:285`): unaffected — always unbounded, regardless of this feature.
- **A bounded rebuild is later followed by an unbounded rebuild, or a `from_seq` resume covering the previously-excluded range**: the excluded mutations (including a previously-excluded bad one) are reapplied. This is expected, documented behavior, not a bug — bounded replay alone does not durably remove anything from the WAL.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `ReplayOptions` MUST gain an optional upper bound distinguishing "unbounded" from "bounded to seq 0" — i.e. its absence must not be representable as, or default to, a bound of `0`. The replay filter MUST apply it as an inclusive upper bound alongside the existing `from_seq` lower bound: a line is replayed only when `from_seq <= seq <= to_seq` (when `to_seq` is present) or `from_seq <= seq` (when absent).
- **FR-002**: `knowledge_rebuild_from_wal` MUST accept an optional `to_seq` parameter through its MCP tool schema, following the same integer/non-negative validation convention already used for `from_seq`. Omitting it MUST reproduce today's behavior exactly — no change for any existing caller.
- **FR-003**: A request where `to_seq` is present and less than `from_seq` MUST be rejected with a clear, specific error before any WAL line is read or any mutation is applied. The database MUST be left completely untouched by a rejected request.
- **FR-004**: All three live, request-driven `ReplayOptions` construction sites (sync reload and both background/streaming reload paths) MUST thread a caller-supplied `to_seq` through to the replay. The startup/degraded-mode recovery construction site MUST NOT be changed — it keeps the unbounded default unconditionally.
- **FR-005**: After a bounded rebuild, the applied WAL position exposed via `knowledge_status` MUST equal the highest seq actually replayed (which is `<= to_seq`), never the WAL's on-disk maximum seq.
- **FR-006**: After a bounded rebuild, the next WAL sequence number allocated to a subsequent write MUST be derived from the true on-disk WAL maximum, not from the (now lower) applied position — no new write may collide with or overwrite an existing, unapplied WAL entry.
- **FR-007**: The `knowledge_rebuild_from_wal` tool description (and any WAL-admin operator documentation covering rebuild/recovery) MUST state plainly that a bounded rebuild (`to_seq` set) is not durable: WAL entries beyond `to_seq` remain on disk, unapplied, and will be reapplied by a later unbounded replay or by a `from_seq` resume that covers them.

### Key Entities *(if the feature involves data)*

- **Replay bound pair**: the `(from_seq, to_seq)` values governing a single replay — `from_seq` an inclusive lower bound defaulting to `0`, `to_seq` an inclusive upper bound with no default (absence means unbounded).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Every `knowledge_rebuild_from_wal` call that omits `to_seq` produces byte-for-byte identical replay behavior (statistics, applied position, resulting graph content) to today's behavior — proving the change is purely additive.
- **SC-002**: A call with `to_seq < from_seq` is rejected with a specific, actionable error, and neither the database nor the WAL is modified as a result.
- **SC-003**: A bounded rebuild to `to_seq: N` over a WAL containing a mutation at some `seq > N` produces a graph that does not reflect that later mutation.
- **SC-004**: After a bounded rebuild to `to_seq: N`, `knowledge_status`'s reported applied WAL position is `<= N` and matches the highest seq actually replayed — not the WAL's true on-disk maximum.
- **SC-005**: After a bounded rebuild, the next mutation written to the WAL receives a sequence number strictly greater than the WAL's true on-disk maximum at the time of that write (no overwrite of existing, unapplied entries).
- **SC-006**: The tool description and any operator-facing WAL admin documentation for `knowledge_rebuild_from_wal` explicitly state that a bounded rebuild is non-durable against a subsequent unbounded replay or an overlapping `from_seq` resume.

## Assumptions

- This issue ships the non-destructive design only: the WAL tail beyond `to_seq` is left entirely in place (never truncated, archived, or marked for skip). Durable rollback — truncating/archiving the tail, or recording a persistent skip-range that future replays honor — is explicitly out of scope and is a follow-on once this primitive exists, per the issue's own stated preference.
- The existing `warn_on_rebuild_seq_gap` advisory (`handlers.rs:1484`), which warns when `from_seq` skips forward past the prior applied position, is keyed only on `from_seq` and does not inspect a rebuild's resulting (landing) position — it is unaffected by adding `to_seq` and requires no change to avoid misfiring.
- `to_seq` follows the same MCP JSON Schema convention as `from_seq` (`type: integer`, `minimum: 0`) but has no `default`, since its absence (unbounded) is semantically distinct from a bound of `0`.

## Out of Scope

- Durable rollback of a bad mutation: WAL truncation/archival, or a persisted skip-range marker honored by future replays. This issue provides the bounded-replay primitive those approaches would build on, not the durable mechanism itself.
- Checkpointing ("this graph was known-good at seq N") and snapshotting (materialized state at seq N) features. `to_seq` is a prerequisite for both but neither is built here.
- A new, distinct operator advisory specifically for "this rebuild moved the applied position backwards" (beyond the existing gap-warning being unaffected, per Assumptions). May be added at implementation's discretion or as a follow-up; not required for acceptance.

## Source References

- Community discussion #207, section 4 — the originating report (never filed as a standalone issue)
- #204 — removed the specific incident trigger; did not change the underlying "a bad mutation cannot be undone" property
- [ADR-0026](../../docs/adr/0026-episode-cursor-wal-resume.md) — establishes the WAL-as-source-of-truth model this issue depends on
- `specs/353-persist-and-expose-an/spec.md` — the applied-WAL-seq persistence feature this issue's FR-005/FR-006 rely on (`knowledge_status`'s `wal.applied_seq` / `wal.max_seq`)
- `crates/core/src/replay.rs:181` (`ReplayOptions`), `crates/core/src/replay.rs:430` (the `from_seq` filter this issue extends)
- `crates/core/src/handlers.rs:1484` (`warn_on_rebuild_seq_gap`), `:1688`, `:1871`, `:2002` (the three `ReplayOptions` construction sites this issue must thread), `:3170` (`validate_from_seq`)
- `crates/core/src/recovery.rs:285` — the startup-recovery `ReplayOptions` construction site that must remain unbounded
- `crates/service/src/mcp/tools.rs` — `knowledge_rebuild_from_wal`'s MCP tool schema and description
