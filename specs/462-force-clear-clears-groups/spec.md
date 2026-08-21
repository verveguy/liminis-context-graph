# Feature Specification: `force_clear` must not clear group data a rebuild cannot restore

**Feature Branch**: `fabrik/issue-462`
**Created**: 2026-08-21
**Status**: Specified
**Input**: User description: "`knowledge_rebuild_from_wal {force_clear: true}` clears every `group_id` found in the replayed directory's WAL content, but replay only restores rows from that same directory. A group whose data is split across streams loses whatever lives elsewhere — cleared, never recreated. Follow-up to #432, which introduced the content scan. That fix is right; this is a case it did not anticipate."

## Background

`force_clear`'s pre-clear guard exists to make a WAL rebuild idempotent: a `from_seq: 0` full
rebuild against a group that already has data should either fail fast or clear that data first,
rather than re-issuing a native `CREATE` for every already-existing row and hitting a
duplicate-primary-key failure per node (issue #353). Issue #378 introduced one WAL stream
directory per `group_id`, and #432 (ADR-0432) widened the guard from "clear only the request's
own `group_id`" to "clear every `group_id` actually referenced by the replayed directory's own WAL
content" — because a legacy stream migrated from before #378 can carry content for a `group_id`
other than the directory it now lives in, and the narrower guard let replay collide with that
group's already-populated data undetected.

That fix is correct as far as it goes, but it clears by `group_id` alone, with no regard for
*where else* that group's data lives. A `group_id` can have rows in more than one physical WAL
directory: some inside the directory this rebuild is about to replay, and some in a separate,
independently-written per-group stream elsewhere. The current guard clears the group everywhere
once it finds the group_id mentioned anywhere in the replayed directory's content — including rows
that live in a stream this replay will never touch and therefore can never recreate. Those rows
are gone.

This was caught by `mcp_real_corpus_mutation_e2e::mcp_write_path_over_real_corpus_fixture`, which
regressed on `main` between #432's and #442's merges (green at `a3b6375f`, red at `1964d8ca`): the
real-corpus fixture is a pre-#378 flat WAL migrated into the `liminis` directory whose content
carries `group_id: "apollo_program"`. The test writes a new chunk directly into `apollo_program`'s
own post-#378 stream, then runs `force_clear: true` with no `group_id` (defaulting to `liminis`).
The guard scans the `liminis` directory, finds `apollo_program` referenced, and clears that group's
data everywhere — including the entity just written to `apollo_program`'s own directory, which this
`liminis`-scoped replay never reads. Entity count drops from 1507 back to 1506.

The underlying invariant: `force_clear` must clear **exactly** what the imminent replay is about to
recreate — no less (or #432's collision bug returns), and no more (or this issue's data loss
returns).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Full rebuild after a migrated legacy stream has since received independent writes (Priority: P1)

An operator (or the system, via `knowledge_process_chunk`/`knowledge_add_episode`) writes new data
for a group whose only other footprint is a legacy WAL stream that was migrated, post-#378, into a
different group's directory by directory position rather than by the group_id its content actually
carries. Later, someone runs `knowledge_rebuild_from_wal {force_clear: true}` against the directory
that holds the migrated legacy content (typically the default group, since that's where migration
places anything it can't otherwise place). The rebuild must recreate the legacy content correctly
(per #432) without destroying the independent data that group has accumulated in its own,
separately-replayed stream since migration.

**Why this priority**: This is the exact defect reported: a rebuild that is supposed to be
count-preserving instead silently drops rows, and the caller has no way to know it happened. Any
workspace that predates #378, has since been written to normally, and is later rebuilt hits this.

**Independent Test**: Seed a workspace with a legacy (pre-#378) WAL stream migrated into a
directory whose owning group differs from the group_id its content carries. Write additional data
for that referenced group_id through its own, independent post-migration stream. Run
`knowledge_rebuild_from_wal {force_clear: true}` against the migrated directory. Assert: (a) the
legacy content is correctly recreated (no duplicate-primary-key collision — #432's fix still
holds), and (b) the independently-written data in the other stream still exists afterward.

**Acceptance Scenarios**:

1. **Given** a WAL directory whose content, when scanned, references a `group_id` that also has
   rows in a separate WAL stream directory not being replayed, **When**
   `knowledge_rebuild_from_wal {force_clear: true}` runs against the first directory, **Then** the
   rows in the separate, un-replayed stream still exist after the rebuild completes.
2. **Given** the same setup as #432 addresses — a migrated legacy stream whose entire referenced
   group's data lives inside the one directory being replayed, with no independent stream
   elsewhere — **When** `force_clear: true` runs, **Then** that group's data is still cleared
   before replay and replay still succeeds without duplicate-primary-key collisions (#432's fix is
   preserved).
3. **Given** a rebuild request with an explicit `to_seq`, **When** the content scan discovers a
   `group_id` whose only rows (in the replayed directory) fall past `to_seq`, **Then** that
   group's data outside the bounded replay window is still not cleared (the existing `to_seq`
   bound, from #432, continues to hold alongside this issue's fix).

---

### Edge Cases

- A group's data spans the replayed directory and one or more other stream directories
  simultaneously (the case this issue addresses): clearing must not remove the portion this replay
  cannot recreate.
- A group referenced by the scanned content has no data anywhere outside the replayed directory:
  clearing proceeds as #432 already established.
- A `to_seq`-bounded request whose scanned group_id's rows only appear past the bound: already
  excluded from clearing by #432's existing bound; this issue's fix must compose with that bound
  rather than replace it (FR-003).
- A rebuild scenario where a group cannot be safely cleared without either (a) risking loss of data
  outside this replay's reach, or (b) risking a duplicate-primary-key collision on replay because
  its in-scope rows were left uncleared: this situation must be surfaced to the caller rather than
  resolved silently in either direction (FR-004).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `force_clear` MUST NOT clear data for a `group_id` that the imminent replay will not
  restore. What gets cleared must be bounded by what this specific replay will write back, not by
  the full set of group_ids the scanned content merely mentions.
- **FR-002**: #432's fix MUST be preserved: a migrated legacy stream whose content carries a
  `group_id` other than its directory's owning group MUST still have that group's data cleared
  before replay when the replay *will* recreate it, so the duplicate-primary-key collisions #432
  fixed do not return.
- **FR-003**: The existing `to_seq` bound (from #432) MUST continue to be honored — it is the same
  underlying invariant ("don't clear what this replay won't recreate") applied along a different
  axis (sequence position rather than physical stream location), and both must hold together.
- **FR-004**: A rebuild MUST NOT silently drop data that `force_clear` would otherwise need to
  clear but cannot safely recreate. When this situation arises, it MUST be surfaced to the caller
  in a way that is programmatically observable from the rebuild's own result — not solely as a log
  line the caller has to know to go looking for.

### Key Entities

- **WAL stream directory**: The physical, per-`group_id` directory (post-#378) or migrated legacy
  location holding one or more `.jsonl` WAL files for a rebuild to replay.
- **Scanned content group_id**: A `group_id` value found while scanning a WAL directory's own
  content, independent of which group_id the directory is nominally "owned" by.
- **Replay restore set**: The specific rows a given rebuild invocation will actually recreate,
  bounded by the directory being replayed and by `from_seq`/`to_seq` — the set FR-001 says
  clearing must not exceed.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `mcp_real_corpus_mutation_e2e::mcp_write_path_over_real_corpus_fixture` passes.
- **SC-002**: #432's own regression coverage continues to pass — a migrated legacy stream whose
  content carries a foreign `group_id` still has that group cleared before replay, and replay
  still completes without duplicate-primary-key collisions.
- **SC-003**: A new test covers the split-stream case directly: content for group G exists in the
  replayed directory, additional rows for G exist in a separate, independent stream, a
  `force_clear: true` rebuild runs against the first directory, and the rows in the separate stream
  are asserted to survive.
- **SC-004**: If a rebuild encounters a case where a group cannot be cleared without either risking
  loss of un-replayed data or risking a replay collision (FR-004's scenario), that outcome is
  verifiable from the rebuild call's own response — a test can assert on it without inspecting log
  output.

## Assumptions

- The fix operates within the existing `knowledge_rebuild_from_wal` request/response shape; no new
  RPC method is introduced.
- "Another stream" means a WAL stream directory distinct from the one this rebuild invocation is
  replaying — whether that is a normal post-#378 per-group directory or another migrated legacy
  location is not distinguished by this issue's requirements.
- This issue does not require repairing data already silently dropped by a rebuild that hit this
  defect before this fix ships (consistent with #432's own stated scope boundary).

## Out of Scope

- Restructuring `migrate_wal_root_if_needed` to split a legacy stream by embedded group_id at
  migration time. #432 already considered and rejected this for not covering already-migrated
  deployments; nothing here reopens that decision.
- Refusing the rebuild outright whenever the scan finds a group with rows outside the replayed
  directory. Considered and rejected: it would block the legitimate migrated-legacy case #432
  exists to support whenever that group has also received normal post-migration writes — the
  ordinary state of any upgraded, actively-used workspace.
- Any change to the `from_seq > 0` incremental-resume path — the `force_clear` guard does not run
  there, and #432 confirmed no other WAL/group-scoped operation shares this guard's
  request-group-vs-content-group assumption.

## Source References

- Issue #462 (this issue) and its linked CI failure at `1964d8ca`.
- #432 / ADR-0432 (`docs/adr/0432-force-clear-guard-scans-wal-content-group-ids.md`) — the content
  scan this issue amends; its ADR should be updated in place rather than superseded.
- #378 — introduced per-group WAL stream directories, which is what makes a group's data
  splittable across streams.
- #414 — precedent for surfacing a situation the engine cannot safely resolve rather than
  proceeding silently.
- `crates/service/tests/mcp_real_corpus_mutation_e2e.rs:436` — the regressing assertion (SC-001).
- `crates/core/tests/fixtures/real_corpus_wal/expected_results.json` — the fixture whose embedded
  `group_id: "apollo_program"` triggers the content scan.
