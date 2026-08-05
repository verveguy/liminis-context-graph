# Feature Specification: Re-derive `WalWriter` `global_seq` after rebuild/clear to prevent duplicate WAL seqs

**Feature Branch**: `fabrik/issue-352`
**Created**: 2026-08-05
**Status**: Draft
**Input**: User description: "Re-derive WalWriter global_seq after rebuild/clear to prevent duplicate WAL seqs"

## Background

`WalWriter`'s `global_seq` is derived exactly once, from `scan_max_seq(wal_dir)` in
`WalWriter::new` (`crates/core/src/wal.rs:60`), and thereafter only incremented in memory
(`wal.rs:107`). Nothing re-derives it for the lifetime of the process.

If the WAL directory is populated *after* the service starts — an external seed, an EFS/backup
restore, or a git pull of a distributed WAL, each followed by `knowledge_rebuild_from_wal` — the
writer keeps its stale `global_seq` (typically `0`). The next `knowledge_process_chunk` then
re-emits sequence numbers already present in the WAL.

Confirmed on `main` at `1183160a`: neither `handle_rebuild_from_wal` nor `clear_db_for_rebuild`
re-derives `global_seq`; there is no `scan_max_seq` call anywhere in `handlers.rs`.

### Impact

Duplicate `seq` values in a WAL that is published and consumed by other nodes. For a deployment
treating the WAL as the source of truth (#351's orac/zen model), the sequence space stops being
a monotonic identifier — two different mutations share a `seq`, and any consumer ordering or
de-duplicating by `seq` gets a wrong answer.

Consumers work around it today by seeding the WAL *before* starting lcg. That ordering constraint
is undocumented and easy to violate.

### Why this blocks #351

Issue `#351` proposes a persisted `applied_seq` compared against `max_seq` to decide whether a node's DB
is consistent with the WAL. That comparison is only sound if `seq` is unique and monotonic across
the WAL. While this bug exists, a WAL can contain duplicate seqs, and both `applied_seq` and
`max_seq` become ambiguous — the feature would be built on an unreliable key.

Fix this first, then #351's comparison rests on a sound invariant.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - WAL seeded after service start, then rebuilt (Priority: P1)

An operator starts the service against an empty (or stale) WAL directory, then populates that
directory out-of-band — an external seed, a restore from backup, or a pull of a distributed WAL —
and calls `knowledge_rebuild_from_wal` to bring the database in line with the newly-present WAL
content. The next mutation the service performs (e.g. via `knowledge_process_chunk`) must not
collide with a `seq` already present in the WAL it just replayed.

**Why this priority**: This is the core defect. It is a silent data-corruption bug — nothing
errors, the duplicate `seq` just appears — and it is the prerequisite for #351's `applied_seq`
model to be sound.

**Independent Test**: Start the service against an empty WAL dir, write WAL files directly to
that dir whose max `seq` is `N`, call `knowledge_rebuild_from_wal`, then call
`knowledge_process_chunk`. Confirm the `seq` written by that call is `> N`, and that no WAL file
in the directory contains a duplicate `seq` afterward.

**Acceptance Scenarios**:

1. **Given** a service running against an empty WAL directory, **When** the directory is
   populated with WAL files whose highest `seq` is `N` and `knowledge_rebuild_from_wal` is
   called, **Then** the next `seq` emitted by `knowledge_process_chunk` is strictly greater than
   `N`.
2. **Given** the same setup, **When** `knowledge_rebuild_from_wal` is called with
   `force_clear: true` instead, **Then** the same guarantee holds.
3. **Given** the sequence of operations in scenario 1, **When** all WAL files in the directory
   are inspected afterward, **Then** no `seq` value appears more than once.
4. **Given** a service that started with an already-populated WAL directory (the case that works
   correctly today, with no external population after start and no rebuild), **When** it emits
   new mutations, **Then** behavior is unchanged from before this fix — no regression.

---

### Edge Cases

- **Rebuild over an empty WAL directory**: re-derivation must not push `global_seq` backwards
  below what the writer has already emitted this process. Re-derivation is monotonic —
  effectively `max(current_in_memory_value, derived_value)` — never a bare assignment from the
  rebuild's findings.
- **Concurrent `knowledge_process_chunk` during a rebuild or clear**: confirmed by inspection —
  both `handle_rebuild_from_wal`'s replay (streaming and non-streaming paths) and
  `clear_db_for_rebuild` already hold `state.write_lock` in write mode for their duration, and
  `knowledge_process_chunk` (via `episode::add_episode`) also acquires that same lock before
  writing. The existing lock already excludes this interleaving; re-derivation does not need its
  own additional synchronization. This is recorded as a confirmed fact, not an assumption — the
  spec's job was to check, per the issue's own instruction, rather than assume it away.
- **A WAL whose highest `seq` lives in a line that failed to replay**: the on-disk scan
  (`scan_max_seq`) and the replay's own bookkeeping (`last_committed_seq`, which reflects only
  successfully committed batches) can disagree — a failed line can still have a higher on-disk
  `seq` than the last commit. Re-derivation must take the max of both sources, not trust replay
  bookkeeping alone. A test must pin this case specifically (a WAL file containing a line with a
  higher `seq` than any line that actually replays successfully).
- **Dry-run rebuilds** (`dry_run: true`): a dry run is documented and implemented as a read-only
  preview that never executes Cypher or mutates the database. This fix's re-derivation is scoped
  to rebuilds and clears that actually replay/mutate — a dry run MUST NOT re-derive
  `global_seq`, to keep "dry run" meaning "no observable side effect," including on in-process
  writer state.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: After a non-dry-run `knowledge_rebuild_from_wal` completes, `WalWriter`'s
  `global_seq` MUST be re-derived so the next emitted `seq` is strictly greater than every `seq`
  present in the WAL directory at that point.
- **FR-002**: The same MUST hold after `clear_db_for_rebuild` runs as part of a
  `force_clear: true` rebuild.
- **FR-003**: The re-derivation MUST account for both available sources — the replay's
  `last_committed_seq` and a fresh on-disk scan of the WAL directory (the same scan
  `WalWriter::new` performs at startup) — and take the max of whichever are available, rather
  than trusting either alone. A WAL may contain lines beyond the last successfully committed one.
- **FR-004**: A WAL directory populated after process start, followed by a rebuild, MUST NOT
  produce a duplicate `seq` on the next `knowledge_process_chunk`.
- **FR-005**: Re-derivation MUST be monotonic with respect to the writer's current in-memory
  `global_seq` — it can only raise the value (`max(current, derived)`), never lower it. This
  matters specifically for a rebuild over an empty or low-max-seq WAL directory: it must not undo
  sequence numbers the writer has already assigned to mutations still pending in this process.
- **FR-006**: Dry-run rebuilds (`dry_run: true`) MUST NOT trigger re-derivation — a dry run has no
  observable side effects today, and this fix must not introduce one.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Start the service against an empty WAL dir; populate the dir with a WAL whose max
  `seq` is `N`; call `knowledge_rebuild_from_wal`; call `knowledge_process_chunk`. The `seq`
  written is `> N`.
- **SC-002**: The same holds when the rebuild uses `force_clear: true`.
- **SC-003**: No duplicate `seq` appears across all WAL files after that sequence of operations.
- **SC-004**: The existing start-with-populated-WAL path is unchanged — no regression in the case
  that works today.
- **SC-005**: A test demonstrates the "failed-to-replay line has the highest on-disk `seq`" edge
  case from FR-003 producing a correct (not falsely low) re-derived `global_seq`.
- **SC-006**: A test demonstrates that a `dry_run: true` rebuild does not alter the writer's
  `global_seq`.

## Assumptions

- `state.write_lock` (held in write mode by both the rebuild/replay path and
  `clear_db_for_rebuild`, and acquired by `knowledge_process_chunk`'s underlying
  `episode::add_episode`) already serializes rebuild/clear against concurrent chunk processing.
  This was confirmed by inspection of `crates/core/src/handlers.rs` and
  `crates/core/src/episode.rs` during Specify, per the issue's own instruction to check rather
  than assume. Research/Plan should still confirm no code path processes a chunk (or otherwise
  advances `global_seq`) while holding only a lesser lock.
- `WalWriter::global_seq` and its mutation are guarded by whatever lock already protects
  `state.wal_writer` (a `Mutex<Option<WalWriter>>`); re-derivation reuses that existing guard
  rather than introducing new synchronization.
- The fix is scoped to the two call sites named in the issue (`handle_rebuild_from_wal`'s replay
  completion and `clear_db_for_rebuild`) — not a general "re-scan on every write" change, which
  would have a performance cost this bug does not require paying.
- Milestone **0.12.2** — a patch. This is a correctness bug in a released version with a small,
  contained fix, and it is the prerequisite for #351.

## Out of Scope

- #351's `applied_seq`/`max_seq` consistency feature itself — this issue only makes that future
  feature's invariant sound; it does not implement it.
- Closing the community report #351 — that closes only when #351's feature ships, not this fix.
- Any change to how `global_seq` is derived at initial process startup
  (`WalWriter::new` / `scan_max_seq`) — that path is already correct; only the missing
  re-derivation after rebuild/clear is in scope.
- Documenting or removing the "seed the WAL before starting lcg" operator workaround — this fix
  makes that workaround unnecessary but updating operator-facing docs about it is not required by
  this issue.

## Source References

References below are as of the pre-fix state (`main` at `1183160a`) that motivated this issue;
line numbers shift once the fix lands (see `WalWriter::resync_global_seq` and
`wal_exec::resync_global_seq_after_rebuild` for the post-fix implementation).

- `crates/core/src/wal.rs:60` — `WalWriter::new`, where `global_seq` is derived once via
  `scan_max_seq`.
- `crates/core/src/wal.rs:107` — where `global_seq` is incremented in memory per mutation.
- `crates/core/src/wal.rs:257` — `scan_max_seq`, the on-disk scan to reuse for re-derivation.
- `crates/core/src/handlers.rs` — `handle_rebuild_from_wal` (streaming and non-streaming replay
  paths) and `clear_db_for_rebuild`; neither currently calls `scan_max_seq` or touches
  `WalWriter`.
- `crates/core/src/episode.rs:628` — where `knowledge_process_chunk`'s underlying `add_episode`
  acquires `state.write_lock`, confirming the concurrency edge case.
- `crates/core/src/replay.rs:155,1066` — `ReplayStats::last_committed_seq` and where it is
  computed as a running max across committed batches.
- `crates/core/src/app_state.rs` — `AppState::wal_writer` (`Arc<Mutex<Option<WalWriter>>>`) and
  `AppState::write_lock`.
- Issue `#351` — the community report this issue was split out of; blocked by this fix.
