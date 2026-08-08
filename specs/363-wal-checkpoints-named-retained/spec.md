# Feature Specification: WAL Checkpoints — Named, Retained Recovery Positions

**Feature Branch**: `fabrik/issue-363`
**Created**: 2026-08-08
**Status**: Draft
**Input**: User description: "Bounded replay (#362) makes it possible to restore a graph to an earlier WAL position. It does not tell you which position to restore to. An operator recovering from a bad mutation has to answer 'what seq was the graph last good at?' from the outside — by correlating timestamps, reading WAL lines by hand, or bisecting with repeated bounded rebuilds. At the scale ADR-0026 documents (43,821 WAL files, seq 4,641,989) that is not a realistic recovery procedure. A checkpoint is the missing piece: a named, retained WAL position meaning 'this graph was known-good here.'"

## Background

`applied_seq` (#353 / ADR-0353) already persists a WAL position in the database — but it is *derived, not chosen* (it advances automatically after every commit), *singleton* (a `MERGE ... SET` overwrite means there is no history, only "where the database is right now"), and *positional, not semantic* (it records where the database is, not that any particular position was known-good).

A checkpoint is not a new kind of measurement — it is an operator-chosen label attached to a position that `applied_seq` already produces: "this graph was known-good here." Concretely:

```
knowledge_checkpoint_create { name: "pre-migration", note: "before the entity-type recanonicalisation" }
  -> { name: "pre-migration", seq: 4641900, created_at: "..." }

knowledge_checkpoint_list
  -> [ { name, seq, created_at, note, reachable }, ... ]

knowledge_checkpoint_delete { name: "pre-migration" }
```

`create` captures the database's *current* `applied_seq` at call time — it does not scan the WAL, replay anything, or accept an operator-supplied seq. That keeps creation O(1) and side-effect-free, which is the whole point of separating a checkpoint from a snapshot.

Recovery then reads:

```
knowledge_rebuild_from_wal { from_seq: 0, to_seq: <checkpoint.seq>, force_clear: true }
```

using the `to_seq` upper bound `knowledge_rebuild_from_wal` gained in #362.

### Why this is not snapshotting

A checkpoint is a *number* — a named pointer into the WAL. A snapshot is *materialized state*. A snapshot implies a checkpoint (it was taken at some position); a checkpoint requires no snapshot. Restoring to a checkpoint costs a full bounded replay to that position, which is correct but slow (ADR-0026 measures ~7h for a full production replay). Making restore *fast* is a distinct concern from making it *possible* — this feature is scoped to correctness of recovery (naming a good position, and being honest about whether it is still reachable), not cost of recovery. The existing `knowledge_dump_wal` DB→WAL compaction capability (#161 / ADR-0028) writes a fresh, compact WAL of the current graph state to a separate target directory; it addresses WAL file-count growth and dialect modernization, not restore latency, and does not by itself invalidate or renumber any checkpoint. Making restore-to-checkpoint fast (e.g. via a true point-in-time snapshot mechanism) is out of scope here and is not yet tracked by an existing issue in this repository as of this spec — the issue's original reference to "#35 (WAL compaction)" pointed at the wrong issue (#35/#29 is the already-merged Tier 2 WAL admin work — `prepare_checkpoint`/`rebuild_from_wal`/`rebuild_status` — not a compaction issue); a follow-up issue should be filed if and when fast restore becomes a priority.

### Naming note: distinct from `knowledge_prepare_checkpoint`

This codebase already has an admin method called `knowledge_prepare_checkpoint` (#29/#35), which rotates/flushes the live WAL writer so pending mutations are on disk before an *external* backup or filesystem checkpoint of the database files. That is an unrelated concept — a disk-flush operation, not a named position. This spec's `knowledge_checkpoint_create`/`knowledge_checkpoint_list`/`knowledge_checkpoint_delete` name a *WAL sequence position*, not a disk flush. The two features are independent and may be used together (e.g. `knowledge_prepare_checkpoint` before taking a filesystem backup, `knowledge_checkpoint_create` to label the WAL position that backup corresponds to), but the shared word "checkpoint" across two different meanings is a real point of operator confusion worth calling out explicitly in whatever documentation ships with this feature.

### The publisher angle (context, not in scope)

In the orac/zen distribution model, a producer's WAL tail is frequently a torn write — an episode partially extracted, entities recorded without their edges, ingest interrupted mid-chunk. A consumer hydrating at that instant gets a structurally incomplete graph with no way to detect it. A producer that publishes "this channel is consistent through seq N" would give consumers a coherent hydration target instead of whatever bytes happened to land — the same primitive proposed here (a named, retained seq), just distributed rather than local. #360 (multi-source hydration with per-source applied positions) is the natural home for a per-source extension of this same idea, and any future distribution mechanism should reuse this feature's position-naming rather than invent a second one. This spec covers the **local, single-database** case only; exporting checkpoint metadata as a WAL-directory sidecar file for git distribution is explicitly deferred (see Out of Scope) but is called out here so the local storage decision doesn't accidentally foreclose it.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Label a known-good position before a risky change (Priority: P1)

An operator is about to run a mutation they're not fully confident in (a bulk edit, an entity-type recanonicalization, an experimental ingest). Before running it, they create a named checkpoint capturing the graph's current WAL position, so that if the change goes wrong they have a precise, named position to recover to instead of having to reconstruct one after the fact.

**Why this priority**: This is the entire reason the feature exists — every other capability (list, delete, recovery) only matters because a checkpoint was created first.

**Independent Test**: Call `knowledge_checkpoint_create { name: "pre-migration" }` against a running service with a non-null `applied_seq`; verify the response echoes the name and the database's current `applied_seq` as `seq`, and that no WAL scan or replay occurred (the call completes in O(1) time regardless of WAL size).

**Acceptance Scenarios**:

1. **Given** a running service with `applied_seq` = 4641900, **When** the operator calls `knowledge_checkpoint_create { name: "pre-migration", note: "before the entity-type recanonicalisation" }`, **Then** the response is `{ name: "pre-migration", seq: 4641900, created_at: <timestamp>, note: "before the entity-type recanonicalisation" }` and no replay or WAL scan occurs.
2. **Given** an existing checkpoint "pre-migration", **When** the operator creates a second checkpoint "post-migration" at a later `applied_seq`, **Then** both checkpoints coexist unchanged — creating "post-migration" does not alter "pre-migration"'s stored seq, nor does it alter the database's own `applied_seq`.

---

### User Story 2 - Recover to a named checkpoint after a bad mutation (Priority: P1)

A mutation after "pre-migration" corrupts the graph. The operator lists checkpoints to find the right one, then bounds a rebuild to that checkpoint's seq to restore the graph to the known-good position — without having to correlate timestamps, read WAL lines by hand, or bisect with repeated rebuilds.

**Why this priority**: This is the recovery workflow the feature exists to enable; it is what makes User Story 1 valuable rather than inert bookkeeping.

**Independent Test**: Create a checkpoint, apply a further mutation, then call `knowledge_rebuild_from_wal { from_seq: 0, to_seq: <checkpoint.seq>, force_clear: true }` and verify the resulting graph excludes the later mutation's effects and the resulting `applied_seq` equals the checkpoint's seq.

**Acceptance Scenarios**:

1. **Given** a checkpoint "pre-migration" at seq 4641900 and a subsequent bad mutation at a later seq, **When** the operator calls `knowledge_checkpoint_list` and then `knowledge_rebuild_from_wal { from_seq: 0, to_seq: 4641900, force_clear: true }`, **Then** the rebuilt graph does not reflect the bad mutation, and this end-to-end path (create checkpoint → bad mutation → bounded rebuild) is covered by an automated test.

---

### User Story 3 - Manage checkpoint lifecycle (list, delete, detect staleness) (Priority: P2)

An operator periodically reviews which checkpoints exist, removes ones that are no longer useful, and needs to know if a checkpoint's target position has become unreachable (for example, because the underlying WAL files it points into were removed or replaced) before relying on it for recovery.

**Why this priority**: Retention is unbounded and operator-managed per the issue's design (checkpoints are cheap, so no automatic eviction) — without list/delete and a staleness signal, checkpoints accumulate indefinitely and a stale one can silently fail (or worse, silently succeed against the wrong data) at the moment an operator needs it most.

**Independent Test**: Create several checkpoints, delete one by name, call `knowledge_checkpoint_list` and verify the deleted one is absent and the others are unaffected. Separately, arrange for a checkpoint's seq to no longer be present in the currently configured WAL directory (e.g. by pointing the service at a WAL directory that does not contain that seq) and verify `list` marks it as unreachable rather than a subsequent restore attempt failing without explanation.

**Acceptance Scenarios**:

1. **Given** checkpoints "a", "b", and "c" exist, **When** the operator calls `knowledge_checkpoint_delete { name: "b" }`, **Then** `knowledge_checkpoint_list` subsequently returns only "a" and "c", each unchanged.
2. **Given** a checkpoint whose seq is no longer present among the WAL files currently reachable for replay, **When** the operator calls `knowledge_checkpoint_list`, **Then** that checkpoint is reported with an explicit unreachable indicator rather than appearing identical to a healthy checkpoint.

---

### Edge Cases

- **`applied_seq` is unknown (`null`) at create time** (e.g. a freshly-upgraded pre-existing database that hasn't backfilled yet, per ADR-0353's `null`/`0`/integer contract): `knowledge_checkpoint_create` must fail with a clear error rather than silently storing a `null` or `0` seq that does not represent the operator's actual intent.
- **Creating a checkpoint whose name already exists**: rejected with an explicit error identifying the name conflict (see FR-002 and the Assumptions section for the rationale — a checkpoint name is meant to be a durable, trustworthy reference, so silent overwrite is the wrong default).
- **Deleting a checkpoint name that does not exist**: rejected with an explicit error rather than a silent no-op (see FR-006 and Assumptions).
- **`knowledge_checkpoint_list` on an empty checkpoint set**: returns an empty list, not an error.
- **A checkpoint's seq is still present in the WAL but earlier than any seq the WAL's current tail scan considers "trusted"** (e.g. due to a partial or manual WAL directory swap): the reachability check in FR-007 is defined in terms of the WAL content actually available for replay at query time, so this reduces to the same unreachable case as full removal.
- **The database is restarted between create and list/delete**: checkpoints must still be present and correct — this is the retention guarantee (FR-003), not an edge case that degrades gracefully; it is a hard requirement.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST provide `knowledge_checkpoint_create { name, note? }` that captures the database's current `applied_seq` as of the call and stores it under the given `name`, without scanning or replaying the WAL. `note` is optional free-text.
- **FR-002**: `knowledge_checkpoint_create` MUST reject the call with a clear error if a checkpoint with the given `name` already exists, rather than silently overwriting it. (Deleting and recreating is the supported way to redefine a name.)
- **FR-003**: The system MUST provide `knowledge_checkpoint_list` returning every retained checkpoint (`name`, `seq`, `created_at`, `note`, and a reachability indicator per FR-007), and this set MUST survive a full process restart.
- **FR-004**: Creating, listing, or deleting one checkpoint MUST NOT alter any other checkpoint's stored `name`/`seq`/`created_at`/`note`, and MUST NOT alter the database's own `applied_seq`.
- **FR-005**: The system MUST provide `knowledge_checkpoint_delete { name }` that permanently removes the named checkpoint.
- **FR-006**: `knowledge_checkpoint_delete` MUST reject the call with a clear error if no checkpoint with the given `name` exists.
- **FR-007**: `knowledge_checkpoint_list` MUST indicate, per checkpoint, whether its `seq` is still reachable — i.e. whether the WAL content currently available to this database for replay still covers that position — rather than that determination only surfacing as a failure at restore time.
- **FR-008**: `knowledge_checkpoint_create` MUST fail with a clear error if the database's `applied_seq` is unknown (`null`) at call time, rather than storing a placeholder value.
- **FR-009**: The end-to-end recovery path — create a checkpoint, apply a subsequent mutation, bound a rebuild (`knowledge_rebuild_from_wal { to_seq: <checkpoint.seq> }`) to that checkpoint — MUST be covered by an automated test.
- **FR-010**: Checkpoint storage MUST NOT impose a limit on the number of retained checkpoints; retention is unbounded and operator-managed (no automatic eviction).

### Key Entities *(if the feature involves data)*

- **Checkpoint**: An operator-created, named, retained WAL position. Attributes: `name` (unique identifier, operator-chosen), `seq` (the WAL sequence number captured at creation time, matching the unit and meaning of `applied_seq`), `created_at` (timestamp of creation), `note` (optional free-text description), and a derived reachability status (not stored, computed at read time per FR-007). Distinct from `WalPosition`/`applied_seq` (#353), which is a single, automatically-advancing position with no history and no name.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An operator can name the graph's current WAL position and later recover to exactly that position using only the checkpoint's name (via its returned `seq`) — no manual WAL inspection, timestamp correlation, or bisection required.
- **SC-002**: Checkpoint creation completes in time independent of WAL size (no full WAL scan or replay), consistent with `applied_seq` already being an O(1) persisted lookup per ADR-0353.
- **SC-003**: Multiple checkpoints can exist simultaneously without interfering with each other or with `applied_seq`, verified by an automated test that creates several, deletes one, and confirms the others are unchanged.
- **SC-004**: Checkpoints persist across a process restart with no loss or corruption of `name`/`seq`/`created_at`/`note`.
- **SC-005**: A checkpoint pointing at a WAL position no longer reachable for replay is distinguishable, via `list`, from a healthy checkpoint — an operator never discovers unreachability only by way of a failed restore.
- **SC-006**: The full recovery workflow described in User Story 2 (create checkpoint → bad mutation → bounded rebuild to the checkpoint) is demonstrated end-to-end by an automated test.

## Assumptions

- **Duplicate-name behavior (FR-002)**: Creating a checkpoint under a name that already exists is rejected rather than silently overwritten. Rationale: a checkpoint name is meant to be a durable, trustworthy reference ("this graph was known-good here") — an operator or runbook referencing "pre-migration" should always mean the same position unless the operator explicitly deletes and recreates it. This mirrors `name` being the storage's primary key (per the issue's proposed storage shape). If this default is wrong for the intended workflows, it can be revisited before Plan.
- **Delete-of-missing-name behavior (FR-006)**: Deleting a nonexistent checkpoint name is rejected with an error rather than treated as a no-op, on the grounds that it is very likely an operator mistake (e.g. a typo) and an explicit error gives clearer feedback than silent success.
- **Reachability check granularity (FR-007)**: This spec requires that unreachability be detectable and reported by `list`, but does not prescribe the exact mechanism (e.g. comparing against the WAL directory's minimum available seq vs. attempting a dry-run bounded replay) — that is a Research/Plan-stage decision.
- **Storage shape**: A second table alongside `WalPosition`, keyed by `name`, extending the existing ADR-0353 graphiti schema-parity divergence rather than establishing a new one (per the issue's own Storage section). The exact schema (column types, table name) is a Plan-stage decision.
- **Ordering of `knowledge_checkpoint_list` results**: Not specified by this feature; any deterministic order is acceptable.
- **Timestamp format**: `created_at` is assumed to follow this codebase's existing timestamp conventions used elsewhere in IPC responses; the exact format is a Plan-stage decision.
- **Scope bucket for new IPC methods**: Per this project's convention for `handle()` dispatch additions, these three new `knowledge_*` methods will need corresponding `ToolSpec` entries in the MCP tool registry — left to Research/Plan, but flagged here so it isn't missed (see CLAUDE.md's "When adding a new `knowledge_*` dispatch method").

## Out of Scope

- **Fast restore / point-in-time snapshotting.** Restoring to a checkpoint still costs a full bounded replay. Making that fast is a distinct, separately-tracked concern (see Background) and is not addressed here.
- **Per-source checkpoints for multi-source hydration** (#360). This spec covers a single local database's checkpoints only; per-source extension is a natural follow-up once #360 lands, and should reuse this feature's naming rather than invent a parallel scheme.
- **Exporting checkpoint metadata as a WAL-directory sidecar file for git distribution** (the "publisher angle"). Explicitly deferred per the issue; this spec is local-database-only. A future issue should revisit this once the local mechanism has shipped.
- **Explicit operator-supplied seq on create.** `knowledge_checkpoint_create` only captures the database's *current* `applied_seq`; it does not accept an arbitrary caller-supplied seq to label.
- **Update/rename of an existing checkpoint.** Only create, list, and delete are in scope, per the issue's acceptance criteria; redefining a name is delete-then-create.

## Source References *(optional)*

- #353 / ADR-0353 — `applied_seq` / `WalPosition` (the position this feature labels)
- #362 — bounded WAL replay (`to_seq` on `knowledge_rebuild_from_wal`), the mechanism a checkpoint is restored through
- #29/#35 — Tier 2 WAL admin, including the unrelated `knowledge_prepare_checkpoint` (disk-flush) method
- #161 / ADR-0028 — `knowledge_dump_wal` DB→WAL compaction (related but distinct: compacts WAL file count, does not provide fast restore-to-checkpoint)
- #360 — multi-source hydration with per-source applied positions (natural follow-up for per-source checkpoints)
- ADR-0026 — episode-cursor WAL resume (the scale context: 43,821 WAL files, ~7h full replay)
