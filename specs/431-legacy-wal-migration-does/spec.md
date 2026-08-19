# Feature Specification: Legacy WAL migration does not stamp .wal-generation.json

**Feature Branch**: `fabrik/issue-431`
**Created**: 2026-08-17
**Status**: Specified
**Input**: User description: "The per-group WAL migration (ADR-0378) never writes `.wal-generation.json` for the stream it produces, so #414's unknown-generation guard refuses `knowledge_rebuild_from_wal` on every upgraded workspace."

## Background

On first boot under an upgraded binary, `migrate_wal_root_if_needed` (`crates/core/src/wal_group.rs:240`)
moves a legacy flat WAL (`.lcg/wal/*.jsonl`, pre-0.13.0) into `.lcg/wal/<group>/`. It does not mint a
generation for the resulting stream. #414's guard, shipped in 0.13.2, refuses to replay a group that
has a recorded position but an unknown generation — which is exactly the state migration leaves
behind.

**This node is assumed to own the stream it is refusing to replay** — this is an assumption, not
something the migration can prove from directory contents alone (see *Assumptions* below for why it
holds today and what would invalidate it). Under that assumption, there is no publisher to discipline
and no reset to detect: generation identity exists so a *consumer* can tell a publisher's rebuild from
an append, and a node replaying its own WAL has no such exposure. The guard is firing where the
property it protects does not apply.

This issue replaces #428, which reached the same diagnosis but scoped the fix too widely (see *Out of
Scope* below).

### Reproduction (carried from #428)

Verified against the released `aarch64-apple-darwin` artifacts for 0.13.1 and 0.13.2, on an
identical legacy workspace seeded from this repo's own `crates/core/tests/fixtures/real_corpus_wal/wal/`
(16 files, 74,396,854 bytes, `max_seq: 12481`, no `.lcg/db`):

- Both versions migrate the layout identically — all 16 files land in `.lcg/wal/liminis/`, and
  `knowledge_status` reports `wal_groups.liminis` with `max_seq: 12481`.
- **0.13.1 rebuilds successfully**: 1506 entities, 2392 relationships, 228 episodes, `applied_seq`
  12481 == `max_seq`.
- **0.13.2 refuses** with an unknown-generation error. `knowledge_status` reports `generation: null`,
  `generation_status: "unknown"` for the group.

This is a regression in 0.13.2 against 0.13.1, on the upgrade path only.

### Why it matters

- It bites exactly when a rebuild is needed: a lost or corrupted `.lcg/db`, ADR-0009 degraded-mode
  recovery, or a deliberate rebuild.
- #398's lbug 0.19.1 upgrade documents a rollback procedure — stop the service, move `.lcg/db/`
  aside, start the older binary and let it rebuild from the WAL. That procedure does not work on a
  migrated workspace under 0.13.2.
- The error's remedy ("republish this stream's full directory") is meaningless for a locally
  migrated workspace: there is no publisher and nothing upstream to re-copy from.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Rebuild a migrated workspace after DB loss (Priority: P1)

An operator's `.lcg/db` is lost or corrupted (or is intentionally set aside, e.g. per #398's
rollback procedure, or under ADR-0009 degraded-mode recovery) on a workspace that was originally
created before 0.13.0 and migrated to the per-group WAL layout by an upgraded binary. The operator
expects `knowledge_rebuild_from_wal` to succeed, as it did under 0.13.1, rather than being refused.

**Why this priority**: This is the exact regression the issue reports. Without it, every upgraded
workspace that predates 0.13.0 loses the ability to rebuild from its own WAL — the primary recovery
path this feature protects.

**Independent Test**: Reproduce with the fixture-seeded legacy workspace (`crates/core/tests/fixtures/real_corpus_wal/wal/`)
under a build containing the fix: let migration run, then call `knowledge_rebuild_from_wal`, and
confirm it completes with the same counts 0.13.1 produced.

**Acceptance Scenarios**:

1. **Given** a legacy flat WAL workspace (pre-0.13.0 layout, `*.jsonl` files loose at the WAL root)
   that has never been opened by an upgraded binary, **When** the upgraded binary starts and
   `migrate_wal_root_if_needed` relocates the loose files into the destination group's directory,
   **Then** a readable `.wal-generation.json` is written for that group as part of the same
   migration.
2. **Given** a freshly migrated workspace, **When** `knowledge_status` is queried for the migrated
   group, **Then** it reports a non-null `generation` and `generation_status: "known"`.
3. **Given** a freshly migrated workspace, **When** `knowledge_rebuild_from_wal` is invoked,
   **Then** it succeeds and produces the same entity/relationship/episode counts and
   `applied_seq == max_seq` that 0.13.1 produced (1506 entities, 2392 relationships, 228 episodes,
   `applied_seq == max_seq == 12481` for the reproduction fixture).

---

### User Story 2 - A genuinely unknown stream is still refused (Priority: P1)

A stream exists with a recorded position but no generation for reasons unrelated to this migration —
for example, a stream received from a publisher whose sidecar (`.wal-generation.json`) was stripped
per the ADR-0387 publish contract, or otherwise never wrote one through the migration path this issue
fixes. The unknown-generation guard introduced by #414 must continue to refuse replay for that
stream, unchanged.

**Why this priority**: The fix must not weaken the safety property #414 exists to protect. A fix
that stamps every unknown-generation stream indiscriminately would reintroduce the exact blind spot
#414 closed. This holds equal priority to Story 1 — neither is acceptable without the other.

**Independent Test**: Take a per-group stream with a recorded position, remove its
`.wal-generation.json` by hand (simulating a stream that arrived without one, independent of this
migration), and confirm `knowledge_rebuild_from_wal` still refuses.

**Acceptance Scenarios**:

1. **Given** a per-group stream with a recorded position and no `.wal-generation.json` that did not
   arise from `migrate_wal_root_if_needed`'s migration path, **When** `knowledge_rebuild_from_wal`
   is invoked, **Then** it is refused, exactly as before this change.

---

### User Story 3 - Refusal message covers both local and received-stream remedies (Priority: P2)

An operator who hits the unknown-generation refusal needs a remedy that works whether they are
looking at a locally-owned stream (no publisher exists, so "republish" is not an available remedy)
or a received stream (where republishing from the publisher is the correct next step). The two
situations are indistinguishable on disk (see *Assumptions*), so the message cannot pick one — it
must state both, so a reader isn't left with only a remedy that doesn't apply to their case.

**Why this priority**: This improves operator experience when the guard fires for a legitimately
unknown stream (Story 2's case). It is secondary to fixing the regression (Story 1) and preserving
the guard (Story 2), since those determine correctness; this determines clarity of the resulting
error.

**Independent Test**: Trigger the refusal on a stream lacking migration provenance (per Story 2's
setup) and confirm the message states both remedies, not just the prior "republish this stream's
full directory" wording alone.

**Acceptance Scenarios**:

1. **Given** `knowledge_rebuild_from_wal` refuses a stream because its generation is unknown,
   **When** the refusal message is rendered, **Then** the message states both possible remedies —
   republishing from a publisher for a received stream, and hand-creating the sidecar for a local
   workspace with no publisher — since the two situations that can produce this refusal are
   indistinguishable on disk and the message cannot assert which one applies.

### Edge Cases

- Migration runs on a workspace where `.wal-generation.json` already exists for the destination
  group (e.g., a partially-completed prior migration, or a group that already received writes
  through some other path) — the existing file must not be overwritten (FR-002).
- A crash occurs mid-migration, after some loose entries have been moved into the group directory
  but before all of them have, and before the generation stamp is written. On the next call to
  `migrate_wal_root_if_needed`, the remaining loose entries are moved and the stamp is (still)
  written, consistent with the function's existing crash-safe, idempotent design.
- A workspace was already migrated by a 0.13.0 or 0.13.1 binary before this fix existed. It has no
  stamp, and `migrate_wal_root_if_needed` returns early on it (no loose top-level entries remain to
  migrate), so it is not touched by this fix. This population is explicitly out of scope — see
  *Out of Scope*.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `migrate_wal_root_if_needed` MUST write a readable `.wal-generation.json` for the
  group it migrates content into, as part of that migration. This treats a legacy flat WAL as
  locally owned — see *Assumptions*, which is where that rests and why it holds today.
- **FR-002**: The migration MUST remain idempotent and MUST NOT overwrite an existing
  `.wal-generation.json` if one is already present for the destination group.
- **FR-003**: The unknown-generation guard (introduced by #414) MUST be unchanged in its refusal
  behavior. A stream that is genuinely unknown for reasons unrelated to this migration — e.g. an
  externally published stream stripped of its dot-namespace per ADR-0387's publish contract — MUST
  still be refused. This issue adds a stamp on one local code path; it does not relax the check.
- **FR-004**: `knowledge_rebuild_from_wal`'s refusal message MUST state both remedies for the two
  situations it can arise from — republishing from a publisher (received stream) and hand-creating
  the sidecar (local workspace with no publisher) — since the two are indistinguishable on disk and
  the message cannot assert which one applies. The existing "republish this stream's full directory"
  remedy remains correct for a received stream; it is joined by, not replaced with, the local remedy.
- **FR-005**: No IPC or MCP tool schema, response shape, or dispatch method may change — this ships
  as a patch release.
- **FR-006**: Tests MUST cover both directions: (a) a workspace migrated from the legacy flat layout
  replays successfully afterwards, and (b) a stream with a genuinely unknown generation and a
  recorded position is still refused.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The reproduction described in *Background* rebuilds successfully with the same counts
  0.13.1 produced (1506 entities, 2392 relationships, 228 episodes, `applied_seq == max_seq ==
  12481`) when run against a build containing this fix.
- **SC-002**: `knowledge_status` on a freshly migrated workspace reports a non-null `generation` and
  `generation_status: "known"` for the migrated group.
- **SC-003**: A per-group stream whose `.wal-generation.json` is removed by hand, with a recorded
  position, is still refused — the guard is not weakened.

## Assumptions

**A legacy flat WAL at the root is assumed to be locally owned. This is an assumption, not a
proof.** Nothing on disk distinguishes a flat WAL this node wrote from a flat WAL copied in from
somewhere else — the migration sees loose `*.jsonl` files at the WAL root and cannot tell where they
came from.

It holds today for two reasons, one structural and one factual:

- **Structural**: the flat layout predates 0.13.0, which is where per-group streams, the publish
  contract, and generation identity were introduced. A stream published under the ADR-0387 contract
  is per-group and carries its own dot-namespace; it is not flat, and it is not placed at the WAL
  root. Received streams are never migrated — an upstream node migrates and stamps its own stream
  before publishing it.
- **Factual**: there are no flat upstream streams in the wild at time of filing. This is direct
  knowledge of the deployed population, not an inference from the code.

**What would invalidate it**: any publisher that distributes a flat WAL, or any operational practice
of hand-copying a flat WAL root between nodes. Under either, this migration would stamp a received
stream with a locally-minted identity — reintroducing precisely the blind spot #414 exists to close,
and doing it silently, since a stamped stream looks healthy. If that becomes possible, this
assumption is the thing to revisit first, and the general stream-ownership question (see *Out of
Scope*) stops being deferrable.

## Out of Scope

- **Workspaces already migrated by 0.13.0 or 0.13.1.** Those were moved to the per-group layout
  without a stamp and will not re-enter `migrate_wal_root_if_needed`, which returns early when there
  are no loose top-level entries. Automatically backfilling them is deliberately not attempted: on
  disk, a locally-migrated unstamped stream and a received stream stripped of its sidecar are
  indistinguishable — same directory shape, same recorded position, same absent file — so any
  filesystem-driven backfill would also stamp the stripped published stream and turn #414 into a
  no-op. #428 tried to have both (its FR-002 and FR-003) and they cannot hold together. That
  population is small, days old, and entirely dev/test; it is covered by a documented one-time
  operator step (create the sidecar for the affected group by hand), a deliberate assertion of
  ownership by someone who knows the answer rather than a guess made by inspecting a directory.
- **A general stream-ownership model.** "Is this stream ours to write or someone else's to consume?"
  is a real modelling gap, and it is the property both the minting rule (`global_seq == 0` as a
  proxy for it) and #414's guard are really reaching for. It is not needed here: at migration time,
  ownership is asserted by the migration itself, under the assumption stated above, rather than
  inferred from ambiguous on-disk state. Worth filing separately if a case appears that genuinely
  cannot be answered locally; it should not ride along on a patch unblocking upgrades.

## Source References

- **ADR-0378** — the per-group migration this issue amends (`docs/adr/0378-multi-stream-wal-per-group-directory.md`).
- **ADR-0387** — generation identity; its Story 5 tolerated a legacy no-generation stream
  indefinitely, which #414 reversed (`docs/adr/0387-wal-stream-generation-identity.md`).
- **ADR-0414** — the unknown-generation refusal (`docs/adr/0414-wal-generation-unknown-refuses-replay.md`).
  Worth amending to record that a locally migrated stream is stamped at migration rather than left
  unknown.
- `crates/core/src/wal_group.rs:240` — `migrate_wal_root_if_needed`.
- `crates/core/tests/fixtures/real_corpus_wal/wal/` — the reproduction fixture.
- #428 — prior attempt at this fix, superseded by this issue's narrower scope.
- #398 — lbug 0.19.1 upgrade / rollback procedure that depends on this working.
