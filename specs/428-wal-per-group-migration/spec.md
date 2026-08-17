# Feature Specification: Stamp WAL generation during legacy-layout migration

**Feature Branch**: `fabrik/issue-428`
**Created**: 2026-08-17
**Status**: Specified
**Input**: User description: "WAL per-group migration does not stamp .wal-generation.json — rebuild_from_wal refused on every upgraded workspace"

## Background

The per-group WAL migration (ADR-0378) moves a legacy flat WAL (`.lcg/wal/*.jsonl`, pre-0.13.0
layout) into `.lcg/wal/<group>/` on first boot under an upgraded binary. That migration never
writes `.wal-generation.json` (ADR-0387) for the resulting stream. #414's unknown-generation
guard — which ships in 0.13.2 — refuses to run `knowledge_rebuild_from_wal` against any group
that has a previously recorded position but an unknown (missing or corrupt) generation. The
migration path produces exactly that state: a per-group directory with content and a recorded
position, but no generation sidecar. The result is that `knowledge_rebuild_from_wal` is
unavailable on any workspace that has gone through this migration.

This was verified against the released `aarch64-apple-darwin` artifacts for 0.13.1 and 0.13.2,
given an identical legacy workspace seeded from this repo's own
`crates/core/tests/fixtures/real_corpus_wal/wal/` (16 files, 74,396,854 bytes, `max_seq: 12481`,
no `.lcg/db`). Both versions migrate the layout identically — all 16 files land in
`.lcg/wal/liminis/`, and `knowledge_status` reports `wal_groups.liminis` with `max_seq: 12481`.
They diverge on rebuild: 0.13.1 succeeds (1506 entities, 2392 relationships, 228 episodes,
`applied_seq` 12481 == `max_seq`); 0.13.2 refuses with an "unknown generation" error, and
`knowledge_status` on the migrated workspace reports `generation: null`,
`generation_status: "unknown"` for the group.

This is a regression in 0.13.2 against 0.13.1, on the upgrade path, not a new-workspace issue.
It matters because:

- It affects anyone upgrading from ≤0.12.x, **and** anyone whose workspace was migrated by 0.13.0
  or 0.13.1 — neither of those releases wrote a generation sidecar either, so their streams are
  equally unknown to 0.13.2.
- It bites whenever a rebuild is actually needed: a lost or corrupted `.lcg/db`, ADR-0009
  degraded-mode recovery, or a deliberate rebuild.
- The lbug 0.19.1 upgrade (#398) documents a rollback procedure in its CHANGELOG entry — stop the
  service, delete or move aside `.lcg/db/`, and start the older binary, which rebuilds the graph
  from the WAL on startup. That procedure does not work on a migrated workspace under 0.13.2. This
  issue blocks that guidance being true.
- The remedy the current error text offers — "republish this stream's full directory" — is
  meaningless here: a locally migrated workspace has no publisher; there is nothing upstream to
  re-copy from. The message assumes the ADR-0387 publish/subscribe path and gives a user with a
  locally upgraded workspace no actionable route.
- The guard is refusing a case it does not need to protect: in the reproduction, `applied_seq` was
  0 against an empty database before the rebuild attempt, so there was no previously recorded
  position to protect and nothing at risk of corruption.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Upgrading a legacy workspace can rebuild from WAL (Priority: P1)

An operator upgrades a workspace from ≤0.12.x (flat WAL layout) directly to the version
containing this fix. The binary migrates the flat WAL into `.lcg/wal/<group>/` on first boot, as
it already does today. After migration, the operator (or ADR-0009 degraded-mode recovery, or a
deliberate rebuild) calls `knowledge_rebuild_from_wal` and it succeeds.

**Why this priority**: This is the core regression. Without it, `knowledge_rebuild_from_wal` is
unusable on any freshly migrated workspace, which breaks the documented lbug-rollback procedure
(#398) and ADR-0009 degraded-mode recovery.

**Independent Test**: Seed a workspace with a flat pre-0.13.0 WAL layout (e.g. from
`crates/core/tests/fixtures/real_corpus_wal/wal/`), boot the fixed binary to trigger migration,
then call `knowledge_rebuild_from_wal` and confirm it succeeds and `knowledge_status` reports a
non-null, readable generation for the group.

**Acceptance Scenarios**:

1. **Given** a workspace with a flat pre-0.13.0 WAL layout and no `.lcg/db`, **When** the fixed
   binary boots and migrates the layout, **Then** the resulting group directory has a readable
   `.wal-generation.json`.
2. **Given** the migrated workspace from scenario 1, **When** `knowledge_rebuild_from_wal` is
   called, **Then** it succeeds and reproduces the counts 0.13.1 produced against the same fixture:
   1506 entities, 2392 relationships, 228 episodes, with `applied_seq` == `max_seq` == 12481.

---

### User Story 2 - A workspace already migrated by 0.13.0/0.13.1 can recover (Priority: P1)

An operator is already running a workspace that was migrated to the per-group layout by 0.13.0 or
0.13.1 — the directory structure is already correct, but no generation sidecar was ever written,
because neither of those releases wrote one either. Upgrading further to the version containing
this fix must not leave that operator permanently stuck: they need a route to a working
`knowledge_rebuild_from_wal`, since a migration-time fix alone only helps workspaces migrated by
the fixed binary itself, not ones already sitting unstamped on disk from an earlier release.

**Why this priority**: Equally severe as Story 1 — this is explicitly called out in the issue as
a distinct case migration-time stamping alone does not cover, and it's the more common case in
practice since 0.13.0/0.13.1 have already been out and migrating workspaces.

**Independent Test**: Seed a workspace with the per-group layout already present (as 0.13.0/0.13.1
would leave it) and no `.wal-generation.json`, with a previously recorded position for the group.
Boot the fixed binary and confirm the workspace reaches a state where
`knowledge_rebuild_from_wal` succeeds, via whichever specific mechanism is chosen during Research
and Plan (see Assumptions).

**Acceptance Scenarios**:

1. **Given** a workspace with the per-group WAL layout present, a previously recorded position for
   the group, and no `.wal-generation.json`, **When** the fixed binary is used to reach a working
   rebuild (by whichever route is chosen), **Then** `knowledge_rebuild_from_wal` succeeds against
   that group.
2. **Given** the same starting workspace, **When** the operator instead has a stream whose
   generation is genuinely unknown for a reason unrelated to this local migration history (e.g. an
   externally published stream that never had a generation, per the ADR-0387 publish contract),
   **Then** `knowledge_rebuild_from_wal` still refuses it — the fix for this issue must not weaken
   #414's detection into a no-op.

---

### User Story 3 - Error message names the right remedy for a local unstamped stream (Priority: P2)

An operator hits a still-refused rebuild (whichever residual case remains refused after this fix,
per Story 2's scenario 2, or a transitional state before recovery completes) and reads the error
message to figure out what to do. Today the message tells them to "republish this stream's full
directory," which only makes sense for the ADR-0387 publish/subscribe path. For a locally migrated
workspace with no publisher, this advice is not actionable.

**Why this priority**: Improves diagnosability but does not itself unblock any rebuild — it's a
message-text fix layered on top of Stories 1 and 2.

**Independent Test**: Trigger the refusal path against a workspace whose stream is local and
unstamped (not one that was published and stripped of its dot-namespace), and confirm the error
text names a remedy that applies to that situation rather than the publish-contract remedy.

**Acceptance Scenarios**:

1. **Given** a refusal caused by a local, unstamped stream, **When** the error is returned,
   **Then** its text describes a remedy applicable to a locally migrated workspace, not "republish
   this stream's full directory."

---

### Edge Cases

- A workspace with multiple groups where some groups have a stamped generation and others do not
  (partial migration history across groups): the fix must not affect an unrelated group's already-
  working rebuild, and each group's refusal/recovery is independent.
- A workspace where `applied_seq` becomes non-null via a `knowledge_status` call's own backfill
  before any explicit `knowledge_rebuild_from_wal` call is made — the existing described behavior
  is that this counts as "a previously recorded position" for guard purposes; this fix must not
  change that semantics, only the migration/backfill/message pieces described above.
- A stream that is genuinely unknown because it was never created by this lcg instance and never
  had a generation (e.g. externally published, stripped dot-namespace) — must remain refused
  regardless of `applied_seq`'s value, since this is exactly the case #414 exists to catch and the
  issue does not ask to change detection for genuinely external streams (only for the case where
  there is demonstrably no previously recorded position — see Assumptions on design point 2).
- A rollback per #398's documented procedure (delete/move aside `.lcg/db/`, start an older or the
  fixed binary) must end in a working rebuild on a migrated workspace, matching 0.13.1's behavior.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When migrating a legacy flat WAL layout into `.lcg/wal/<group>/`, the system MUST
  write a readable `.wal-generation.json` for the resulting group's stream as part of that
  migration.
- **FR-002**: A workspace whose per-group layout was already produced by an earlier migration
  (0.13.0 or 0.13.1) that did not stamp a generation MUST be able to reach a working
  `knowledge_rebuild_from_wal` for that group, without requiring the operator to manually recreate
  or fabricate a generation file by hand.
- **FR-003**: Whatever mechanism satisfies FR-002 MUST NOT cause the unknown-generation guard to
  stop refusing a stream that is genuinely unknown for reasons unrelated to this local migration
  history (e.g. an externally published stream stripped of its dot-namespace per the ADR-0387
  publish contract). #414's detection must remain effective after this fix, not become a no-op.
- **FR-004**: The `knowledge_rebuild_from_wal` refusal error message MUST correctly identify the
  remedy for a workspace whose stream is local and unstamped (a migration/backfill gap), as
  distinct from the existing "republish this stream's full directory" remedy, which applies only
  to a stream that was published and then stripped of its generation sidecar.
- **FR-005**: The fix MUST NOT change any IPC or MCP tool schema, response shape, or dispatch
  method — this ships as a patch release, per the issue's explicit constraint.
- **FR-006**: A test MUST demonstrate both directions of guard behavior after this fix: (a) a
  migrated/backfilled stream that previously would have been refused now succeeds, and (b) a
  stream with a genuinely unknown generation and a previously recorded position is still refused.

### Key Entities

- **WAL generation** (`.wal-generation.json`, ADR-0387): a stable, opaque identity minted once per
  group stream, used to distinguish "the same stream, further along" from "a different stream that
  happens to reuse the same `seq` numbering."
- **Legacy flat WAL layout**: the pre-0.13.0 on-disk shape, `.lcg/wal/*.jsonl` directly under the
  WAL root with no per-group subdirectory.
- **Per-group WAL layout** (ADR-0378): the current on-disk shape, `.lcg/wal/<group>/` with its own
  `*.jsonl` files, `.checkpoints/`, `.wal-bounds.json`, and `.wal-generation.json`.
- **Unknown-generation guard** (#414 / ADR-0414): the check in `knowledge_rebuild_from_wal` that
  refuses to replay a group whose generation is unknown (missing or corrupt sidecar) when that
  group already has a previously recorded position.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A legacy flat-WAL workspace migrated by the fixed build has a readable
  `.wal-generation.json` for its group, and `knowledge_rebuild_from_wal` succeeds on it.
- **SC-002**: A workspace already migrated by 0.13.0/0.13.1 (per-group layout present, sidecar
  absent) reaches a working `knowledge_rebuild_from_wal`, via whichever route Research/Plan
  selects.
- **SC-003**: Replaying the fixture WAL (`crates/core/tests/fixtures/real_corpus_wal/wal/`)
  reproduces the counts 0.13.1 produced: 1506 entities, 2392 relationships, 228 episodes,
  `applied_seq` == `max_seq` == 12481.
- **SC-004**: The unknown-generation guard still refuses a genuinely unknown stream after this fix
  — verified by a test covering both the now-succeeding and still-refused directions.
- **SC-005**: No IPC or MCP tool schema, response shape, or dispatch method changes as a result of
  this fix.

## Assumptions

- Two implementation-level design points are intentionally left for Research/Plan to settle rather
  than fixed by this spec, per the issue's own framing:
  1. Whether the fix for FR-002 is a stamp-on-open backfill for an unstamped-but-present group, an
     explicit repair path/tool, or some other mechanism. Research/Plan should prefer whichever
     keeps the guard meaningful — a backfill that stamps any unknown stream on sight unconditionally
     would make the guard unable to detect a genuine unknown stream, which is the failure mode
     #414 exists to catch, and would violate FR-003.
  2. Whether the unknown-generation guard should be narrowed to permit the specific case where
     there is demonstrably no previously recorded position to protect (`applied_seq == 0` against
     an empty database) even when the generation is unknown. If adopted, this is complementary to
     (1), not a substitute for it — a stamped stream is still the correct end state for a migrated
     workspace either way.
- The reproduction and acceptance counts (1506 entities, 2392 relationships, 228 episodes,
  `max_seq: 12481`) are specific to the `real_corpus_wal` fixture and are used as a regression
  oracle, not as general behavior requirements.
- This fix targets the migration and guard behavior only; it does not address the stale
  flat-layout test fixtures (`real_corpus_e2e`, `mcp_real_corpus_admin_data_e2e`) that let this
  regression ship unnoticed, nor the `| tee` without `pipefail` CI defect — both are filed
  separately per the issue's Out of Scope section.

## Out of Scope

- The `| tee` without `pipefail` CI defect that let this regression reach a release unnoticed
  (`ci.yml:295`'s `test` gate, and all five e2e jobs) — filed separately.
- Updating the stale test fixtures still using the flat pre-0.13.0 WAL layout (`real_corpus_e2e`,
  `mcp_real_corpus_admin_data_e2e`), which is why no existing test caught this regression before
  release.
- Anything in #398's lbug 0.19.1 upgrade itself, beyond confirming its documented rollback
  procedure works again once this fix lands.
- Any change to the ADR-0387 publish/subscribe contract or to how externally published streams are
  expected to carry their generation.

## Source References

- #414 — introduced the unknown-generation guard shipped in 0.13.2
- ADR-0387 — `.wal-generation.json` and stream identity
- ADR-0378 — the per-group WAL root this migration targets
- ADR-0009 — degraded-mode startup and recovery, which depends on rebuild being available
- #398 — its CHANGELOG documents the rollback procedure this issue breaks
- `docs/operations.md` — WAL stream-publish contract and generation/guard behavior documentation
- `crates/core/tests/fixtures/real_corpus_wal/wal/` — fixture used in the issue's reproduction and
  reused as the regression-test oracle
