# Feature Specification: WAL checkpoints — named recovery positions stored in the WAL directory

**Feature Branch**: `fabrik/issue-365`
**Created**: 2026-08-08
**Status**: Draft
**Input**: User description: "Bounded replay (#362, merged) makes it possible to restore a graph to an earlier WAL position, but it does not tell you *which* position to restore to. Add a checkpoint: a named, retained WAL position meaning 'this graph was known-good here.' Checkpoint metadata must not be stored in the database — it must live in the WAL directory, so it survives exactly the failure (database loss/corruption) it exists to recover from. Supersedes #363, which was closed unimplemented because its spec assumed database-resident storage; this issue carries the corrected design."

## Background

[#362](https://github.com/verveguy/liminis-context-graph/issues/362) (merged `0e86dab`) added `to_seq` to `knowledge_rebuild_from_wal`, so a rebuild can stop at an arbitrary earlier WAL position instead of always replaying to the end. That closed half the recovery gap: it is now *possible* to exclude a bad, WAL-recorded mutation from a rebuild. It did not close the other half — an operator still has to answer "what seq was this graph last good at?" from outside the system: correlating timestamps, reading WAL lines by hand, or bisecting with repeated bounded rebuilds. At the scale ADR-0026 documents (43,821 WAL files, seq 4,641,989) that is not a realistic recovery procedure. A **checkpoint** — a named, retained WAL position meaning "this graph was known-good here" — is the missing piece.

### Storage — decided, not open for this spec to weigh

**Checkpoint metadata MUST NOT be stored in the database.** A checkpoint stored in the database is destroyed by exactly the event it exists to recover from: after a database wipe, corruption, or loss, the operator holds the WAL and nothing else, and the checkpoint list — the one artifact that says *which* position to restore to — would be gone with the database. It also contradicts the project's core model: if the database is a derived cache, nothing required to rebuild that cache can live inside it. This is a real distinction from `applied_seq` ([#353](https://github.com/verveguy/liminis-context-graph/issues/353) / [ADR-0353](../../docs/adr/0353-persist-and-expose-applied-wal-seq.md)), which is correctly DB-resident: `applied_seq` describes *this database's* position and is meaningless without the database, whereas a checkpoint describes *the WAL stream* and must outlive any database built from it. Consequently there is no new database node table and no extension of the ADR-0353 schema-parity divergence.

This issue supersedes [#363](https://github.com/verveguy/liminis-context-graph/issues/363), which was closed unimplemented because its spec was written against database-resident storage — a design later ruled out for the reasons above.

### Placement: a WAL-directory subdirectory, verified safe against three non-recursive scans

The checkpoint store cannot be a `.jsonl` file in the WAL directory root. Three call sites discover WAL files by extension alone, non-recursively:

- `crates/core/src/replay.rs:297` — the replayer's file list
- `crates/core/src/wal.rs:277` — the `.jsonl` file count
- `crates/core/src/wal.rs:289` — `scan_max_seq`, which derives `global_seq`

Each is of the form `fs::read_dir(dir).filter(|p| p.extension() == Some("jsonl"))`. A `checkpoints.jsonl` sitting beside the WAL would be replayed as mutations by the first scan and fed into sequence allocation by the third, corrupting `global_seq`. Because all three are non-recursive, a subdirectory such as `<wal_dir>/.checkpoints/` is invisible to every one of them with no change to the scan logic — this is the placement this spec adopts.

### Naming: avoiding a collision with an existing, unrelated tool

`knowledge_prepare_checkpoint` already exists ([#29](https://github.com/verveguy/liminis-context-graph/issues/29)/[#35](https://github.com/verveguy/liminis-context-graph/issues/35), `specs/29-tier-2-wal-admin/spec.md`). It rotates and flushes the live WAL writer so pending mutations are on disk before an external filesystem backup — a disk-flush operation, not a named position. It shares the word "checkpoint" with this feature by coincidence, not by relation. Shipping a `knowledge_checkpoint_create` alongside `knowledge_prepare_checkpoint` would put two unrelated meanings of "checkpoint" in the same tool namespace. This spec resolves the collision by using distinct tool names (`knowledge_wal_mark_*`, see FR-001) that keep the WAL association explicit, rather than requiring every reader to hold the disambiguation in their head.

### The publisher angle (context, not this issue's scope)

In the orac/zen distribution model a producer's WAL tail is frequently a torn write — an episode partially extracted, entities recorded without their edges, ingest interrupted mid-chunk. A consumer hydrating at that instant gets a structurally incomplete graph with no way to detect it. A producer publishing "this channel is consistent through seq N" would give consumers a coherent hydration target instead of whatever bytes happened to land. WAL-directory placement makes this a natural extension of the same name-and-position record rather than a rewrite, unlike the database-resident design #363 was blocked on. [#360](https://github.com/verveguy/liminis-context-graph/issues/360) (multi-source hydration with per-source applied positions) is the home for that per-source variant and must reuse this feature's position-naming scheme rather than invent a parallel one — but building it is not this issue's scope (see Out of Scope).

### Explicitly not snapshotting

A checkpoint is a *number* (a name plus a WAL seq). A snapshot would be *materialized state*. Restoring to a checkpoint costs a full bounded replay to that position — correct, but slow (ADR-0026 measures ~7h for a full production replay). No open issue tracks a faster restore path; one should be filed if and when that becomes a priority. This issue is scoped to the correctness of recovery (knowing *which* position to restore to), not the cost of restoring.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Mark the current position as known-good (Priority: P1)

An operator, having verified their graph is in a good state, gives that position a durable name — e.g. "pre-migration" — so they can refer back to it later without external bookkeeping (timestamps, manual WAL inspection).

**Why this priority**: This is the entire reason the feature exists — without the ability to create a named position, there is nothing to list, restore to, or reason about.

**Independent Test**: Against a WAL directory with an attached, running database at a known `applied_seq`, call `knowledge_wal_mark_create {name: "pre-migration"}` and confirm a checkpoint record appears under `<wal_dir>/.checkpoints/` referencing that seq, with no WAL file scan or replay performed.

**Acceptance Scenarios**:

1. **Given** a running database whose `applied_seq` (per ADR-0353) is a known non-negative integer, **When** `knowledge_wal_mark_create {name: "pre-migration"}` is called, **Then** a checkpoint named `"pre-migration"` is recorded with `seq` equal to the database's `applied_seq` at that moment, and the call completes without scanning or replaying the WAL.
2. **Given** the database's `applied_seq` is `null` (unknown — e.g. a freshly-upgraded database that has not backfilled, per ADR-0353), **When** `knowledge_wal_mark_create` is called, **Then** the call fails with a clear, specific error, and no checkpoint record — placeholder or otherwise — is written.
3. **Given** an existing, active (non-deleted) checkpoint named `"pre-migration"`, **When** `knowledge_wal_mark_create {name: "pre-migration"}` is called again, **Then** the call fails with a clear, specific error and the existing record is unmodified.
4. **Given** a fresh workspace with an empty WAL and a fresh database (`applied_seq == 0`, a *known* position, not unknown, per ADR-0353), **When** `knowledge_wal_mark_create {name: "empty"}` is called, **Then** it succeeds and records a checkpoint at `seq: 0`.

---

### User Story 2 - List checkpoints and see which are still restorable (Priority: P1)

An operator preparing a restore lists all named checkpoints and sees, for each, whether it is still reachable given the WAL content currently on disk — before attempting a restore, not as a failure discovered mid-restore.

**Why this priority**: Tied with Story 1 as the minimum viable recovery workflow — a checkpoint that cannot be listed and assessed for reachability cannot be safely chosen as a restore target.

**Independent Test**: Create two checkpoints, delete WAL files covering one of their seqs, then call `knowledge_wal_mark_list` and confirm the surviving-content checkpoint is reported reachable and the other is reported unreachable.

**Acceptance Scenarios**:

1. **Given** two checkpoints exist, both covered by WAL content currently on disk, **When** `knowledge_wal_mark_list` is called, **Then** both are returned with their names and seqs, each marked reachable.
2. **Given** a checkpoint's seq is no longer covered by the WAL content currently available for replay (e.g. earlier WAL files were externally removed), **When** `knowledge_wal_mark_list` is called, **Then** that checkpoint is reported unreachable, distinguishably from the reachable ones — this must be visible at list time, not surface only as a failure during a restore attempt.
3. **Given** no checkpoints have ever been created, **When** `knowledge_wal_mark_list` is called, **Then** it returns an empty list, not an error.
4. **Given** a checkpoint was created and later deleted, **When** `knowledge_wal_mark_list` is called, **Then** the deleted checkpoint does not appear.

---

### User Story 3 - Delete an obsolete or mistaken checkpoint (Priority: P2)

An operator removes a checkpoint that is no longer relevant, or was created under a wrong name, freeing that name for reuse.

**Why this priority**: Necessary for a usable, long-lived checkpoint list, but the feature is meaningfully usable (create + list + manual restore) without it.

**Independent Test**: Create a checkpoint, delete it by name, confirm it no longer appears in `knowledge_wal_mark_list`, then create a new checkpoint reusing the same name and confirm that succeeds.

**Acceptance Scenarios**:

1. **Given** an active checkpoint named `"pre-migration"`, **When** `knowledge_wal_mark_delete {name: "pre-migration"}` is called, **Then** it no longer appears in `knowledge_wal_mark_list`, and the deletion is recorded as an appended tombstone record — the store file is never rewritten in place.
2. **Given** no active checkpoint named `"typo-name"` exists (never created, or already deleted), **When** `knowledge_wal_mark_delete {name: "typo-name"}` is called, **Then** it fails with a clear, specific error rather than silently succeeding.
3. **Given** a checkpoint named `"pre-migration"` was deleted, **When** `knowledge_wal_mark_create {name: "pre-migration"}` is called again, **Then** it succeeds and creates a new, independent checkpoint under that name.

---

### User Story 4 - Checkpoints survive a process restart (Priority: P2)

A checkpoint created before a service restart (planned or crash) is still present and listable after the service comes back up.

**Why this priority**: This is the feature's retention guarantee, not a graceful-degradation nicety — a checkpoint that doesn't survive a restart cannot serve as a durable reference for a runbook.

**Independent Test**: Create a checkpoint, stop the service, restart it against the same WAL directory, and confirm `knowledge_wal_mark_list` still reports it.

**Acceptance Scenarios**:

1. **Given** a checkpoint was created and the service is then restarted (or a different process attaches to the same WAL directory), **When** `knowledge_wal_mark_list` is called, **Then** the checkpoint is still present with the same name and seq.

---

### User Story 5 - Checkpoints travel with the WAL through git (Priority: P3)

An operator distributes a WAL directory via git (the orac/zen model); a checkpoint created by the producer is visible to a consumer who clones or pulls that WAL directory, with no separate distribution step.

**Why this priority**: A direct, low-effort consequence of WAL-directory placement rather than new work, but not required for the core single-instance recovery workflow this issue is scoped around.

**Independent Test**: Create a checkpoint in a WAL directory tracked by git, commit and clone (or `git pull`) that directory into a second location, and confirm `knowledge_wal_mark_list` against the clone reports the same checkpoint.

**Acceptance Scenarios**:

1. **Given** a checkpoint exists in a WAL directory under git version control, **When** that directory is committed and made available to a second checkout (clone or pull), **Then** `knowledge_wal_mark_list` run against the second checkout reports the same checkpoint without any additional distribution step.

---

### Edge Cases

- **Unknown position on create** (`applied_seq` is `null`): rejected with a clear error; no placeholder record is written (Story 1, Scenario 2).
- **Duplicate active name on create**: rejected; existing record untouched (Story 1, Scenario 3).
- **Name reuse after deletion**: allowed — duplicate-rejection applies only to currently active (non-tombstoned) names (Story 3, Scenario 3).
- **Delete of a nonexistent or already-deleted name**: rejected with a clear error, not a silent no-op (Story 3, Scenario 2).
- **Fresh workspace, `applied_seq == 0`**: a *known* position (nothing applied yet), not treated as unknown — create succeeds (Story 1, Scenario 4).
- **Checkpoint's seq no longer covered by on-disk WAL content**: reported unreachable at list time (Story 2, Scenario 2), never left to surface only as a restore-time failure.
- **Concurrent `create` of the same name from two processes sharing a WAL directory**: exactly one succeeds; the other receives the duplicate-name error — never two records under one name.
- **`knowledge_dump_wal` ([#161](https://github.com/verveguy/liminis-context-graph/issues/161)/[ADR-0028](../../docs/adr/0028-db-wal-dump-compaction.md)) output**: a dumped/compacted WAL directory contains no checkpoints from the source directory — checkpoint seqs are meaningless after dump_wal's renumbering, so none are copied (see FR-012). This is documented behavior, not a defect to report.
- **Database degraded/unavailable**: `knowledge_wal_mark_list` and `knowledge_wal_mark_delete` remain available (they touch only the WAL-directory store); `knowledge_wal_mark_create` is unavailable under the same conditions that make `applied_seq` unknown, per the "unknown position" rule above.
- **Restore procedure**: consuming a checkpoint is exactly `knowledge_rebuild_from_wal {from_seq: 0, to_seq: <checkpoint seq>, force_clear: true}` — no new restore method is introduced by this issue.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST provide three operations, exposed as `knowledge_wal_mark_create`, `knowledge_wal_mark_list`, and `knowledge_wal_mark_delete`. These names MUST NOT reuse or be confused with `knowledge_prepare_checkpoint` (an unrelated WAL flush/rotate operation, #29/#35); this feature's own tool descriptions MUST make the distinction explicit even though the names no longer collide.
- **FR-002**: Checkpoint metadata MUST be stored outside the database, as append-only JSONL record(s) under a `.checkpoints/` subdirectory of the WAL directory (`<wal_dir>/.checkpoints/`) — never inside the graph database, and never as a `.jsonl` file directly in the WAL directory root. This MUST NOT introduce any new database node/relationship table and MUST NOT extend the ADR-0353 schema-parity divergence.
- **FR-003**: `knowledge_wal_mark_create` MUST accept a checkpoint `name` and capture the current position automatically, from the attached, running database's `applied_seq` (ADR-0353 / `knowledge_status`'s `wal.applied_seq`) at the moment of the call. It MUST NOT accept a caller-supplied seq, and creation MUST be O(1) relative to WAL size — no WAL file scan, no replay.
- **FR-004**: If the current position is unknown (`applied_seq` is `null`) at the time of `knowledge_wal_mark_create`, the call MUST fail with a clear, specific error, and MUST NOT store a placeholder `null` or `0` value.
- **FR-005**: `knowledge_wal_mark_create` MUST reject a `name` that already identifies an active (non-deleted) checkpoint, with a clear, specific error, without modifying the existing record. A name whose only prior checkpoint has been deleted MAY be reused by a later `create`.
- **FR-006**: Deletion MUST be recorded as an appended tombstone record referencing the checkpoint's name — never as an in-place rewrite or removal of any existing record in the store.
- **FR-007**: `knowledge_wal_mark_delete` on a name that does not currently identify an active checkpoint (never created, or already deleted) MUST fail with a clear, specific error rather than silently succeeding.
- **FR-008**: `knowledge_wal_mark_list` MUST return every active (non-deleted) checkpoint's name and seq, and MUST report, per checkpoint, whether its seq is currently reachable by a bounded replay given the WAL content presently available on disk. The exact reachability-determination mechanism (e.g. comparing against the minimum available seq, versus a dry-run bounded replay) is left to the Research/Plan stages.
- **FR-009**: Checkpoints MUST persist across a process restart — this is the feature's retention guarantee. No automatic eviction is performed; retention is unbounded and operator-managed.
- **FR-010**: `knowledge_wal_mark_list` and `knowledge_wal_mark_delete` MUST remain available even when the attached database is degraded or unavailable, since both operate solely on the `.checkpoints/` store in the WAL directory. `knowledge_wal_mark_create` depends on a database-reported `applied_seq` (FR-003) and is therefore unavailable exactly when FR-004's "unknown position" condition applies.
- **FR-011**: The `.checkpoints/` store MUST be safe under concurrent access from multiple processes sharing the same WAL directory (e.g. a producer and a local reader). Concurrent `create` calls for the same name from different processes MUST NOT both succeed — exactly one wins, and every other concurrent caller receives the duplicate-name error (FR-005).
- **FR-012**: `knowledge_dump_wal`'s output directory MUST NOT contain a copy of the source WAL directory's `.checkpoints/` store — a dumped/compacted WAL directory starts with no checkpoints. This MUST be stated in `knowledge_dump_wal`'s tool description, since a checkpoint's seq is meaningless after dump_wal's seq renumbering and must never be silently copied over as if it still applied.
- **FR-013**: No new restore/rebuild method is introduced. Recovery to a checkpoint MUST be performed entirely through the existing bounded-replay primitive: `knowledge_rebuild_from_wal {from_seq: 0, to_seq: <checkpoint seq>, force_clear: true}`.

### Key Entities *(if the feature involves data)*

- **Checkpoint (WAL mark)**: a named, retained WAL position — a `{name, seq}` pair recorded in the checkpoint store. `name` is unique among currently active (non-deleted) checkpoints; a name may be reused after its prior checkpoint is deleted. `seq` is the WAL sequence number the checkpoint refers to, captured from the attached database's `applied_seq` at creation time (FR-003).
- **Checkpoint store**: the append-only JSONL log(s) under `<wal_dir>/.checkpoints/` holding checkpoint-create and checkpoint-delete (tombstone) records. Never rewritten in place — every state change is a new appended record.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `knowledge_wal_mark_create` against a database with a known `applied_seq` completes without any WAL file scan or replay, and the resulting checkpoint is visible via `knowledge_wal_mark_list` both immediately and after a process restart.
- **SC-002**: `knowledge_wal_mark_create` against a database whose `applied_seq` is `null` fails with a clear, specific error, and no checkpoint record is written as a result.
- **SC-003**: `knowledge_wal_mark_create` with a `name` matching an existing, active checkpoint fails with a clear, specific error, and the existing record is unmodified.
- **SC-004**: `knowledge_wal_mark_list` reports, for every active checkpoint, whether its seq is currently reachable given the WAL content on disk — verified both for a checkpoint fully covered by available WAL content and one that is not.
- **SC-005**: `knowledge_wal_mark_delete` removes a checkpoint from subsequent `knowledge_wal_mark_list` output and permits its name to be reused by a later `create`; the underlying store file(s) grow only by appended records — verified never to shrink or have prior bytes rewritten as a result of a delete.
- **SC-006**: `knowledge_wal_mark_delete` on a nonexistent or already-deleted name fails with a clear, specific error.
- **SC-007**: A checkpoint created by one process is visible via `knowledge_wal_mark_list` from a second, concurrently-running process sharing the same WAL directory; concurrent `create` calls for the same name from two processes result in exactly one success and one duplicate-name error, never two records under one name.
- **SC-008**: `knowledge_rebuild_from_wal {from_seq: 0, to_seq: <checkpoint seq>, force_clear: true}` against a checkpoint created at a given `applied_seq` reproduces a graph whose entity/relationship/episode counts match what existed in the source database at checkpoint-creation time.
- **SC-009**: The presence of `<wal_dir>/.checkpoints/` does not change the WAL file count reported by `knowledge_status`'s `wal.file_count`, does not affect `global_seq` allocation, and is not included in the replayer's file list — verified against the three scan sites named in the Background (`replay.rs:297`, `wal.rs:277`, `wal.rs:289`).
- **SC-010**: `knowledge_dump_wal`'s output directory contains no `.checkpoints/` content copied from its source, and its tool description states this explicitly.

## Assumptions

- **Position source**: `create` captures the attached, running database's `applied_seq` automatically; it does not accept a caller-supplied seq in this issue's scope. `applied_seq` is the only candidate source with a defined "unknown" state (ADR-0353's `null`/`0`/integer contract), which is exactly what the "fail loudly on unknown position" requirement depends on — a source like the WAL writer's `global_seq` has no such "unknown" state to fail on, and would mark the WAL's current write tip rather than "this graph was known-good here." Explicit caller-supplied-seq creation, and creating a checkpoint against a bare WAL directory with no attached database at all (the publisher use case), are deferred — see Out of Scope.
- **Tool naming**: `knowledge_wal_mark_create` / `_list` / `_delete`, per the issue's own recommended option — it avoids the `knowledge_prepare_checkpoint` collision entirely rather than relying on documentation discipline to keep two meanings of "checkpoint" apart forever.
- **Format**: append-only JSONL under `.checkpoints/`, with deletion as a tombstone append rather than an in-place rewrite — consistent with WAL semantics and safe under the concurrent-access requirement (FR-011). Exact file naming/rotation within `.checkpoints/` is left to Research/Plan.
- **Reachability mechanism**: left unspecified at the "what," per the issue's own framing — a Research/Plan decision, not resolved here.
- **`knowledge_dump_wal` interaction**: resolved as "does not carry checkpoints forward, documented explicitly" (FR-012) — the simplest of the three options the issue raised (translate / drop-with-warning / document-only), and the only one that can never silently apply a stale seq, since nothing is copied.
- **Distribution**: this repository's own root `.gitignore` contains no pattern that would exclude a dotted subdirectory like `.checkpoints/` (verified against `main` during Specify). Whether an operator's separate WAL-publication tooling (outside this repository) does the same is a Research-stage confirmation, not something verifiable from this repo alone.
- **Record shape reuse**: the `{name, seq}` shape is deliberately generic so that #360's per-source multi-source-hydration variant can reuse it rather than invent a parallel naming scheme; #360 itself is not implemented by this issue.

## Out of Scope

- Fast/materialized restore (snapshotting). A checkpoint is a number, not materialized state; no open issue tracks a faster restore path — see "Explicitly not snapshotting" in Background.
- Per-source checkpoint positions for multi-source hydration ([#360](https://github.com/verveguy/liminis-context-graph/issues/360)). This issue defines the single-WAL-directory primitive #360 must reuse, not the multi-source variant itself.
- Explicit caller-supplied-seq creation, and creating a checkpoint against a bare WAL directory with no attached database at all (the "publisher" creation path described in the issue's "publisher angle" section).
- Translating checkpoint seqs across a `knowledge_dump_wal` renumbering — checkpoints simply do not carry forward into a dump's output (FR-012).
- Any new database-resident schema/table, or any extension of the ADR-0353 schema-parity divergence — explicitly ruled out.
- Automatic checkpoint eviction or retention limits — retention is unbounded and operator-managed.

## Source References

- [#362](https://github.com/verveguy/liminis-context-graph/issues/362) (merged `0e86dab`) — bounded WAL replay (`to_seq`), the primitive this issue's recovery procedure depends on
- [#363](https://github.com/verveguy/liminis-context-graph/issues/363) — superseded, closed unimplemented (database-resident storage design ruled out)
- [#353](https://github.com/verveguy/liminis-context-graph/issues/353) / [ADR-0353](../../docs/adr/0353-persist-and-expose-applied-wal-seq.md) — `applied_seq` / `WalPosition`, the position source this issue's `create` relies on
- [ADR-0026](../../docs/adr/0026-episode-cursor-wal-resume.md) — episode-cursor WAL resume; the WAL-as-source-of-truth model
- [#161](https://github.com/verveguy/liminis-context-graph/issues/161) / [ADR-0028](../../docs/adr/0028-db-wal-dump-compaction.md) — `knowledge_dump_wal`
- [#360](https://github.com/verveguy/liminis-context-graph/issues/360) — multi-source hydration; must reuse this issue's checkpoint naming
- [#29](https://github.com/verveguy/liminis-context-graph/issues/29) / [#35](https://github.com/verveguy/liminis-context-graph/issues/35), `specs/29-tier-2-wal-admin/spec.md` — `knowledge_prepare_checkpoint`, the existing tool this issue's naming avoids colliding with
- Community discussion #207, section 4 — the originating recoverability report
- `crates/core/src/replay.rs:297`, `crates/core/src/wal.rs:277`, `crates/core/src/wal.rs:289` — the three non-recursive WAL scans motivating `.checkpoints/` subdirectory placement
