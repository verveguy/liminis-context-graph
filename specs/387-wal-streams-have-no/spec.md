# Feature Specification: WAL Stream Generation Identity

**Feature Branch**: `fabrik/issue-387`
**Created**: 2026-08-13
**Status**: Specified
**Input**: User description: "A WAL stream has no identity. Sequence numbers identify a position within a stream, but nothing identifies which stream a position belongs to — so a consumer cannot distinguish 'my publisher advanced' from 'my publisher reset and republished from scratch.' lcg should provide that identity itself rather than leaving each downstream consumer to infer it from file inspection."

## Background

A WAL stream today is identified only by its directory (per #378, one directory per `group_id`)
and its `seq` numbering, which starts at `0` and counts up. `seq` identifies a position *within*
a stream — it says nothing about *which* stream a position belongs to. Nothing distinguishes "the
same stream, further along" from "a different stream that happens to also number its lines from
`0`."

That distinction matters because a publisher can legitimately reset a group's stream: re-extract
its corpus and republish from `seq: 0` with entirely different content and — because entity UUIDs
are minted fresh (`Uuid::new_v4()`) at insert — entirely different entity identities. A consumer
that only tracks `applied_seq` cannot tell this apart from ordinary forward progress:

- **A reset that produces a *shorter* stream** is already detectable today: `applied_seq >
  max_seq` is a contradiction that can only mean the stream restarted.
- **A reset that produces a *longer* stream is not detectable by any signal lcg currently
  exposes.** A consumer at `applied_seq(A) = 24` who later observes `max_seq(A) = 30` reads that
  as "six new lines," replays from `25`, and applies new-generation mutations on top of
  old-generation data. Because the native write path emits `CREATE` rather than `MERGE` for
  `Entity`/`Episodic`/`RelatesToNode_` (ADR-0046), the two generations do not reconcile — they
  co-reside. The graph ends up holding two disjoint copies of the same knowledge, with the older
  one unreachable from the publisher's perspective but fully visible to queries.

The corruption compounds across groups: cross-group pointers (#369) re-resolve by name against
whatever the source group currently holds. If a group holds two generations of the same entity,
name resolution is either `ambiguous` or binds to whichever generation wins the name-index
tie-break — silently mis-binding every layer-graph pointer into that group, while the re-bind pass
reports success.

This is a known, previously-named gap. ADR-0353 (`applied_seq`) named "a corpus reset" explicitly,
in its own opening context, as one of the divergent cases a consumer "must self-heal" from — and
it explicitly rejected the workaround consumers had been using in its place:

> The downstream workaround — hashing WAL file contents to detect change — is unreliable: lcg
> writes compact `serde_json`, but the distributed copy is re-serialized by a Python publisher
> with spaced formatting, so the bytes differ while the semantics are identical, forcing a full
> rebuild on every boot even when nothing changed.

`applied_seq` (#353) answers "has this stream moved?" It does not answer "is this the same
stream?" One existing consumer (orac) currently answers that question by inspecting WAL files
directly — a mechanism that works for orac today, but is exactly the kind of downstream inference
ADR-0353 already argued should not be left to consumers, and every future consumer would otherwise
have to reimplement it independently.

This issue gives every WAL stream a generation identifier: a value that stays stable for the life
of a stream and changes only when that stream is genuinely re-created from scratch, so "is this
the same stream" becomes a value lcg publishes and consumers compare, not a heuristic each
consumer must invent.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A reset is detected instead of replayed incrementally, in both directions (Priority: P1)

A publisher resets a group's stream — republishing from `seq: 0` with new content — and a
consumer sitting at a prior position notices, rather than blending the new generation on top of
the old one. This holds whether the republished stream ends up longer or shorter than the
consumer's last known position; both must take the same detection path, because the "shorter"
case is already partially covered by an `applied_seq > max_seq` contradiction and the "longer"
case is not covered by any existing signal.

**Why this priority**: This is the failure this issue exists to close. Every other requirement
(status reporting, checkpoint reachability, pre-existing-stream handling) exists in service of
making this detection possible and correctly triggered.

**Independent Test**: Hydrate a database from group `A`'s WAL stream to some position. Replace
`A`'s stream on disk with a freshly-created one (new generation, `seq` restarting at `0`,
different content) that ends up longer than the consumer's last position; confirm the consumer
detects a reset rather than replaying incrementally, and does not end up holding both generations'
entities. Repeat with a freshly-created replacement stream that ends up shorter; confirm the same
detection path is taken.

**Acceptance Scenarios**:

1. **Given** a consumer holding `(generation: G1, applied_seq: 24)` for group `A`, **When** `A`'s
   stream is replaced by a new one — generation `G2`, 30 lines — **Then** the consumer detects a
   generation mismatch rather than reading this as "six new lines," and does not replay
   incrementally across it.
2. **Given** the same setup, **When** the replacement stream instead has only 10 lines (shorter
   than the consumer's prior position), **Then** the consumer detects the mismatch via the same
   code path as the longer case — not a separate `applied_seq > max_seq` special case.
3. **Given** a detected generation mismatch, **When** the consumer recovers, **Then** the group
   ends up holding exactly one generation's data — the new one — with no residual entities,
   episodes, or relationships from the old generation.
4. **Given** an unchanged stream that is still being appended to in the ordinary course of
   ingest, **When** a consumer that is caught up checks again, **Then** no reset is reported and
   no full replay is triggered — forward progress on the same generation remains a cheap
   comparison, not a scan.

---

### User Story 2 - A generation is cheap to check, without reading WAL files (Priority: P1)

An operator or automated consumer calls `knowledge_status` and gets each group's generation
alongside its existing `applied_seq`/`max_seq`, so comparing "am I on the same stream" costs the
same as the existing "am I caught up" check already does — not a new, separate WAL scan.

**Why this priority**: Detection (Story 1) is only usable in practice if checking for it is cheap
enough to run on every boot and every routine poll — the exact cost concern ADR-0353 and ADR-0375
already established for `applied_seq`/`max_seq`.

**Independent Test**: Call `knowledge_status` against a multi-group instance and confirm each
group's reported object includes a generation value alongside `applied_seq`/`max_seq`, resolvable
without a full WAL directory scan once a stream's generation has already been observed once.

**Acceptance Scenarios**:

1. **Given** a group with an established WAL stream, **When** `knowledge_status` is called,
   **Then** the response includes that group's current generation alongside its `applied_seq` and
   `max_seq`.
2. **Given** repeated `knowledge_status` calls against an unchanged stream, **When** comparing
   their cost to the existing `applied_seq`/`max_seq` reporting, **Then** reporting the generation
   adds no full-directory WAL scan beyond what those existing fields already require.

---

### User Story 3 - Cross-group pointers re-bind cleanly after a detected reset (Priority: P2)

After a reset is detected and the affected group is fully recovered, layer-graph pointers that
point into that group re-resolve against the new generation only — with no residual binding to
entities that belonged to the generation that was just replaced.

**Why this priority**: This is the concrete downstream harm the issue's Background section
describes (`ambiguous`/mis-bound cross-group pointers). Detecting a reset (Story 1) only closes
the corruption path if the recovery that follows also clears out stale cross-group bindings — a
half-fixed reset (new generation's entities added, old generation's pointers still dangling) is
not meaningfully better than the undetected case.

**Independent Test**: Set up a layer-graph edge whose cross-group pointer resolves into group
`A`. Trigger a detected reset and full recovery of `A`. Confirm the pointer re-binds to an entity
in `A`'s new generation, and that no residual `bound`/`ambiguous` state references the old
generation's (now-purged) entities.

**Acceptance Scenarios**:

1. **Given** a cross-group pointer bound into group `A`'s prior generation, **When** `A`'s reset
   is detected and recovery completes, **Then** the pointer's binding state reflects the new
   generation — either freshly `bound` to a same-named entity in the new generation, or `unbound`
   if no matching name exists there, but never referencing the purged generation's UUID.

---

### User Story 4 - A checkpoint taken before a reset is reported unreachable afterward (Priority: P2)

A named checkpoint (#365) recorded a position in a stream that has since been replaced by a reset.
Listing checkpoints reports that one as unreachable, rather than appearing restorable against a
stream that, semantically, no longer exists.

**Why this priority**: An operator trusting a stale "reachable: true" and restoring to it would
silently attempt to restore against the wrong generation's data — a checkpoint-specific instance
of the same "looks like forward progress but isn't" problem Story 1 solves for ordinary replay.

**Independent Test**: Create a checkpoint against group `A`'s current generation. Replace `A`'s
stream with a new generation. List checkpoints for `A` and confirm the pre-reset checkpoint now
reports unreachable.

**Acceptance Scenarios**:

1. **Given** a checkpoint recorded against generation `G1`, **When** the group's stream is later
   replaced by generation `G2`, **Then** listing checkpoints reports that checkpoint as
   unreachable, distinct from (and in addition to) the existing bounds-based reachability check.
2. **Given** a checkpoint recorded against the group's current generation, **When** no reset has
   occurred, **Then** its reachability is governed by the existing bounds-based check exactly as
   before this issue — this feature narrows reachability, it does not change it for the
   unaffected case.

---

### User Story 5 - A pre-existing stream with no recorded generation doesn't force a spurious rebuild (Priority: P3)

An instance upgrades from a version predating this feature. Its existing WAL streams have no
generation recorded. On first encounter, this is treated as a defined, documented "unknown
generation" state, not as an automatic mismatch — so upgrading does not force every group into a
full rebuild the first time it's checked, and does not force one on every subsequent boot either.

**Why this priority**: Lower priority than Stories 1-2 because it only affects the one-time
upgrade transition, not ongoing operation — but it must be handled explicitly, or every existing
deployment hits a spurious full rebuild exactly once (or worse, repeatedly) on upgrade.

**Independent Test**: Seed a WAL stream directory with no generation file (simulating a
pre-upgrade stream) and a consumer position with no recorded generation. Check it once; confirm no
reset is reported and no full rebuild is forced. Check it again on a later boot with the stream
unchanged; confirm the same.

**Acceptance Scenarios**:

1. **Given** a WAL stream with no generation recorded and a consumer with no recorded generation
   for that group, **When** the consumer checks its position, **Then** no reset/mismatch is
   reported.
2. **Given** the same starting state, **When** the check runs again on a later boot with the
   stream unchanged, **Then** no reset is reported and no full rebuild is triggered — the
   no-generation state does not force a rebuild on every boot.
3. **Given** a stream with no recorded generation that is genuinely reset afterward (republished
   from scratch, now carrying a generation identifier for the first time), **When** the consumer
   next checks, **Then** the reset is still detected — the no-generation upgrade path does not
   permanently disable detection for that stream going forward.

---

### User Story 6 - A dumped WAL always carries a new generation (Priority: P3)

`knowledge_dump_wal` produces a new, renumbered stream from existing data. Its output is a new
stream, not a copy of the source's identity, so it MUST carry its own new generation — never the
source's.

**Why this priority**: Narrower in scope than the other stories (it affects one specific
operation), but a dump that silently carried the source's generation forward would defeat this
issue's entire mechanism the first time someone used `knowledge_dump_wal` for a legitimate export
or migration, since consumers would treat the dumped output as "the same stream" when its `seq`
numbering has actually been rewritten from scratch.

**Independent Test**: Run `knowledge_dump_wal` against an existing group's stream. Confirm the
dumped output's generation differs from the source stream's generation.

**Acceptance Scenarios**:

1. **Given** a WAL stream with an established generation, **When** `knowledge_dump_wal` produces
   an output stream from it, **Then** the output's generation is newly minted and differs from the
   source's.

---

### Edge Cases

- A generation record that exists on disk but is corrupted or unreadable is treated the same as
  "no generation recorded" (the Story 5 path) rather than treated as a mismatch — detection must
  not turn a damaged-but-harmless artifact into a false-positive reset.
- Two processes race to create the same group's stream directory for the first time (both
  observing "no existing content" and both attempting to mint a generation). Exactly one
  generation must end up recorded for that stream; this is a correctness requirement even though
  the mechanism that guarantees it is left to Research/Plan.
- A group's stream directory exists (and has a recorded generation) but has no `.jsonl` content
  yet — e.g., a directory created but not yet written to. This still counts as "a stream that
  exists," not as "no stream," for generation purposes.
- A checkpoint recorded against a generation that is later purged and replaced, where the group's
  *next* generation is itself later reset again — the checkpoint from two generations back must
  still report unreachable, not merely "reachable if it happens to match the *current*
  generation by coincidence."

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Every WAL stream MUST carry a generation identifier, persisted in that stream's own
  directory and replicated with it (alongside `.checkpoints/` and `.wal-bounds.json`), so it
  travels with the published artifact and remains invisible to existing non-recursive `.jsonl`
  scans.
- **FR-002**: Creating a stream where none previously existed MUST mint a new generation.
  Appending to an existing stream MUST NOT change its generation.
- **FR-003**: `knowledge_status` MUST report each group's generation alongside its existing
  `applied_seq` and `max_seq`, at a cost consistent with those existing fields — not a new
  full-directory WAL scan on every call.
- **FR-004**: The persisted consumer-side position (lcg's own recorded position for a group, as
  established by #353/#378) MUST be scoped to a generation. A recorded `applied_seq` from a
  different generation MUST NOT be treated as a valid position in the current one.
- **FR-005**: `knowledge_rebuild_from_wal` MUST detect a generation mismatch between the recorded
  position and the stream on disk, and MUST NOT perform an incremental replay across it. On
  detecting a mismatch, it MUST self-heal by purging the affected group (#361) and performing a
  full replay (#378 FR-006) rather than requiring a separate operator-triggered recovery step —
  consistent with ADR-0353's own precedent that a corpus reset is one of the cases a consumer
  "must self-heal" from, not one that should require every consumer to implement its own recovery
  path. The result of that call MUST clearly indicate that a reset-triggered full replay occurred
  (not present it as indistinguishable from an ordinary incremental replay), so an operator or
  automated caller can still tell what happened.
- **FR-006**: `knowledge_dump_wal` produces a new stream — it renumbers seqs — so its output MUST
  carry a **new** generation, never the source's. This mirrors #365's FR-013, which already
  requires a dumped directory to start with no checkpoints, because their seqs are meaningless
  after renumbering.
- **FR-007**: A checkpoint (#365) MUST record the generation it was taken in, and MUST be reported
  unreachable when the stream's current generation differs from the checkpoint's recorded
  generation — in addition to, not instead of, the existing bounds-based reachability check.
- **FR-008**: Detection MUST NOT depend on WAL file bytes, content hashing, file counts, or
  timestamps, for the reasons ADR-0353 already documents about the byte-hashing workaround it
  replaced.
- **FR-009**: A stream with no recorded generation (predates this feature) MUST be handled
  explicitly: on first encounter, the currently-observed generation is adopted as the baseline for
  that stream/consumer pair rather than compared for a mismatch — mirroring the existing
  `backfill_applied_seq_if_absent` precedent (ADR-0353) of resolving "upgraded, unknown" into
  "known" once, rather than treating every upgrade as a forced reset or re-checking a full-rebuild
  condition on every subsequent boot. Once a generation has been adopted for a stream/consumer
  pair, later changes to that stream's generation are still detected normally (Story 5, Scenario
  3).

### Key Entities

- **WAL stream**: The append-only sequence of mutations for one group (#378's per-group WAL
  directory). Identified by its directory and, after this issue, by a stable generation
  identifier.
- **Generation identifier**: A value minted when a stream's directory is first created (no
  existing content) and unchanged for the life of that stream. Opaque to consumers — compared for
  equality only, never interpreted or ordered.
- **Consumer position record**: The persisted `(generation, applied_seq)` pair a consumer (chiefly
  lcg's own `WalPosition`-equivalent row, per #353/#378) tracks per group, replacing the
  generation-blind `applied_seq`-only record that exists today.
- **Checkpoint**: A named, retained WAL position (#365), extended to also record the generation it
  was taken in.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A publisher resets group `A` and republishes a **longer** stream; a consumer at a
  prior position detects the reset rather than replaying incrementally, and does not end up
  holding two generations of `A`'s entities.
- **SC-002**: The same, for a **shorter** republished stream (the case a bare `applied_seq >
  max_seq` comparison would already catch) — both take the same code path.
- **SC-003**: Ordinary forward progress on an unchanged stream is unaffected: no spurious reset
  detection, and the existing cheap "am I caught up" no-op path (#353) still applies.
- **SC-004**: After a detected reset and full rehydrate, cross-group pointers into that group
  re-bind to the new generation, with no residual bindings to the old one.
- **SC-005**: A checkpoint taken before a reset reports unreachable afterwards rather than
  appearing restorable.
- **SC-006**: A pre-existing stream with no recorded generation behaves per FR-009 — in
  particular, it does not force a full rebuild on every boot.
- **SC-007**: Reporting a group's generation via `knowledge_status` does not add a full WAL
  directory scan beyond what the existing `applied_seq`/`max_seq` fields already cost.

## Assumptions

- The generation identifier is opaque: consumers compare it for equality only. Nothing in this
  spec requires it to be orderable, human-readable, or derived from content.
- Whoever creates a WAL stream's directory mints its generation — lcg's own writer for
  lcg-originated streams. Because the failure scenario this issue exists to fix (a publisher
  resetting and republishing) can originate outside lcg entirely (the orac/zen distributed,
  git-published WAL model), the on-disk generation record's format MUST be documented clearly
  enough that an external publisher creating a stream directly (not via lcg) can mint a compliant
  generation value too, without requiring lcg's involvement at creation time. The exact file
  format is a Research/Plan decision; this spec only requires that it be documented and
  publisher-writable.
- FR-005's self-heal-by-default resolution (rather than fail-loudly and wait for an operator) is
  made here based on ADR-0353's own precedent, which this issue's body already cites: a corpus
  reset is explicitly named as one of the cases a consumer "must self-heal" from. This spec
  requires the self-heal to be clearly reported as a reset-triggered event (not silently identical
  to an ordinary incremental replay's result), so the "clear diagnosis" concern behind the
  fail-loud alternative is addressed without requiring manual recovery.
- This mechanism is per-group, matching #378's per-group WAL directories — there is no
  instance-wide or cross-group generation concept.
- #383 and #385 are open, unimplemented issues at the time of this spec. They are named in the
  original issue as prerequisites "in practice" (without them, the position signal this issue
  extends is inert or misrouted for some write paths) but this spec does not require their
  completion to be written; downstream Research/Plan should confirm sequencing before Implement.

## Out of Scope

- The precise on-disk file format and minting mechanism for the generation identifier — a
  Research/Plan decision, constrained only by FR-001/FR-002/FR-008 and the Assumptions section
  above (persisted, publisher-writable, not derived from content/hashes/counts/timestamps).
- The concurrency mechanism that guarantees exactly one generation is recorded when two processes
  race to create the same stream's directory for the first time (see Edge Cases) — a
  Research/Plan decision.
- Changes to `knowledge_recover`/`knowledge_recover_full`'s whole-database recovery paths beyond
  what generation checks require them to also honor, per #378's already-documented multi-group
  recovery model.
- New recovery machinery for group purge or per-group replay — #361 and #378 FR-006 already
  supply it; this issue only supplies the trigger that invokes it (per the original issue's
  "Relationship to other work").
- A one-time migration pass that retroactively writes generation files for every pre-existing
  stream. FR-009 defines the runtime behavior for encountering a stream that has none; it does not
  require proactively creating one.
- Any consumer-side implementation outside lcg itself (e.g., orac's own internal bookkeeping of
  `(generation, applied_seq)`). This issue defines what lcg persists and publishes; external
  consumers building on it are out of scope.

## Source References

- Issue #353 / ADR-0353 — `applied_seq`, and the "must self-heal from a corpus reset" precedent
  this spec's FR-005 resolution relies on.
- Issue #365 / ADR-0365 — checkpoint directory-per-name store; this issue extends its record shape
  with a generation field (FR-007) and its FR-013 precedent for dumped-directory checkpoints
  (referenced by FR-006).
- Issue #375 / ADR-0375 — the `max_seq`/`min_seq` bounds manifest whose cost discipline FR-003/
  SC-007 extend to the generation field.
- Issue #378 / ADR-0378 — per-group WAL directories; this issue's generation is scoped per-group
  to match.
- Issue #361 — group-scoped purge, the recovery primitive FR-005's self-heal path invokes.
- Issue #369 / ADR-0369 — resolvable cross-group pointers, the re-bind mechanism Story 3 depends
  on.
- Issue #383 (open) — `applied_seq` not advancing for `wal_flush_ungrouped` writes.
- Issue #385 (open) — cross-stream mutation routing (delete_by_group/rebind writing to the wrong
  group's stream).
- ADR-0046 — native write path emits `CREATE` rather than `MERGE`, which is why an undetected
  reset produces co-residing generations instead of reconciling.
