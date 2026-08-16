# ADR-0387: WAL Stream Generation Identity

**Status**: Accepted
**Date**: 2026-08-13
**Issue**: #387
**Relates to**: ADR-0353 (`applied_seq`), ADR-0361 (group-scoped purge), ADR-0365 (WAL
checkpoints), ADR-0369 (resolvable cross-group pointers), ADR-0375 (`wal_max_seq` bounds
manifest), ADR-0378 (multi-stream WAL per group directory), ADR-0046 (native `CREATE` not
`MERGE`)

## Context

A WAL stream today is identified only by its directory (one per `group_id`, ADR-0378) and its
`seq` numbering, which starts at `0` and counts up. `seq` identifies a position *within* a
stream — it says nothing about *which* stream a position belongs to. A publisher can legitimately
reset a group's stream: re-extract its corpus and republish from `seq: 0` with entirely different
content and — because entity UUIDs are minted fresh (`Uuid::new_v4()`) at insert — entirely
different entity identities. A consumer that only tracks `applied_seq` (ADR-0353) cannot tell
this apart from ordinary forward progress:

- A reset that produces a **shorter** stream is already detectable today: `applied_seq >
  max_seq` is a contradiction that can only mean the stream restarted.
- A reset that produces a **longer** stream is not detectable by any signal lcg previously
  exposed. A consumer at `applied_seq(A) = 24` who later observes `max_seq(A) = 30` reads that as
  "six new lines," replays from `25`, and applies new-generation mutations on top of
  old-generation data. Because the native write path emits `CREATE` rather than `MERGE` for
  `Entity`/`Episodic`/`RelatesToNode_` (ADR-0046), the two generations do not reconcile — they
  co-reside. The graph ends up holding two disjoint copies of the same knowledge, the older one
  unreachable from the publisher's perspective but fully visible to queries.

The corruption compounds across groups: cross-group pointers (ADR-0369) re-resolve by name
against whatever the source group currently holds. Two co-residing generations of the same named
entity make resolution either `ambiguous` or bind to whichever generation wins the name-index
tie-break — silently mis-binding every layer-graph pointer into that group, while the re-bind
pass itself reports success.

ADR-0353 named "a corpus reset" explicitly, in its own opening context, as one of the divergent
cases a consumer "must self-heal" from, and rejected the workaround consumers had been using in
its place: hashing WAL file contents to detect change is unreliable, since lcg writes compact
`serde_json` but a distributed copy is re-serialized by a Python publisher with spaced formatting
— the bytes differ while the semantics are identical, forcing a full rebuild on every boot even
when nothing changed. `applied_seq` answers "has this stream moved?"; it does not answer "is this
the same stream?" One existing consumer (orac) answers that question today by inspecting WAL
files directly — exactly the kind of downstream inference ADR-0353 already argued should not be
left to consumers.

This is the fourth leg of one continuous change: ADR-0378 gave every group its own directory and
per-stream cursor; #383/#385 (open at the time of this issue) fix `applied_seq` not advancing for
some write paths and cross-stream mutation misrouting. The moment a consumer adopts per-stream
incremental hydration — the purpose ADR-0378 exists to enable — it drops the file-set-signature
over-approximation that was accidentally covering resets, and its own single-stream reset tell
("a cursor ahead of `max_seq`") only catches a shorter republish. This issue is what makes
per-stream incremental hydration safe, not an optional follow-on to it.

## Decision

### On-disk shape: a third sidecar file, `.wal-generation.json`

A group's WAL directory already carries two sidecar artifacts alongside its `*.jsonl` files:
`.checkpoints/<name>/` (ADR-0365, directory-per-name, `O_EXCL`-atomic) and `.wal-bounds.json`
(ADR-0375, a single-file cache). Generation identity is a third, following the same placement
convention:

```text
<wal_root>/<group_id>/
  *.jsonl
  .checkpoints/
  .wal-bounds.json
  .wal-generation.json     # {"generation": "<uuid>"}
```

The file holds one JSON object with a single `generation` string field — self-describing and
trivially writable by an external, non-lcg publisher (`json.dump({"generation": ...}, f)` from
Python), per the spec's requirement that the format be documented well enough for the orac/zen
distributed-WAL model to mint one without lcg's involvement. A bare-token file was considered and
rejected: a JSON object costs nothing extra to write or parse and leaves room for the format to
add fields later without a breaking change, matching this repo's existing sidecar convention
rather than inventing a bespoke one. The value itself is a full `Uuid::new_v4()` string (not
truncated the way `WalWriter::session_id` is) — global uniqueness matters here, brevity doesn't.
It is invisible to every existing non-recursive `*.jsonl` scan, the same as the other two
sidecars, and was added to `wal_group.rs`'s `is_legacy_top_level_wal_artifact` recognition list
for forward consistency with them (no pre-378 install could actually have one, since this
feature postdates ADR-0378).

**Unlike `.wal-bounds.json`, this file is load-bearing, not a cache.** ADR-0375's manifest is
explicitly rebuildable from a full directory scan whenever it's missing or stale — deleting it
never loses information. A generation record is the opposite: minted once, at first creation, and
never recomputed from content (FR-008 forbids deriving it from bytes, hashes, counts, or
timestamps in the first place) — losing it loses the identity itself, which is why a missing or
corrupt record degrades to "unknown," not to "recompute."

### Minting: only inside `WalWriter::new`'s already-existing "no content" branch

`WalWriter::new` mints a generation (via `wal_generation::ensure_generation`, an atomic
write-temp-then-`hard_link` publish — see below) only when no generation record exists yet **and**
the directory has no prior `.jsonl`
content (`scan_max_seq(&wal_dir)? == 0`) — the same "no existing content" condition that already
governs first-time directory creation there. A pre-existing, populated directory with no
generation record (a pre-#387 stream — Story 5) is **never** retroactively minted; it stays
`None` until something else writes one, matching the spec's explicit exclusion of a proactive
migration pass.

This placement has a load-bearing side effect: `knowledge_dump_wal`'s output directory is always
freshly created and empty before its `WalWriter` is constructed against it (an FR-004-of-#365
precedent already enforces this), so FR-006 ("a dumped stream always carries a new generation,
never the source's") falls out automatically with **no `dump`-specific code** — the same minting
branch that handles ordinary first-time stream creation handles it.

### First-creation race: the loser adopts, it does not error

Two processes racing to create the same group's stream directory for the first time both call
`ensure_generation`. Exactly one wins; the loser does not error the way `checkpoint.rs`'s
duplicate-name case does — it reads the winner's file and adopts that value. Concurrent process
startup contending for a group's directory is an expected race, not a user error — the
correctness requirement is that exactly one generation ends up recorded, not that exactly one
caller succeeds.

**Publish via write-temp-then-`hard_link`, not `create_new`-then-write-in-place.** The first
implementation used `OpenOptions::create_new` (the same `O_EXCL` pattern `checkpoint.rs` uses for
its duplicate-name check) directly against the target path, then wrote the content into the
just-created file. That shape has a real window between the file becoming visible to other
callers and its content actually landing — a process crashing inside that window (between
`create_new` succeeding and `write_all`/`sync_all` completing) leaves a permanently unreadable
file at the target, which every future caller (including a fresh process on the next boot) would
keep hitting. A first fix bounded a loser's read-retry with a fixed budget and, on exhausting it,
self-healed by deleting the target and re-minting — but review (PR #391) correctly identified that
this cannot distinguish a winner that genuinely crashed mid-write from one that is merely slow
(e.g. fsync contention when several groups' streams mint concurrently at process startup): the
self-healer could unlink a still-writing winner's file out from under it, and the winner's own
`write_all`/`sync_all` would then succeed against the now-unlinked inode while the self-healer
mints and publishes a second, different generation at the same path — two callers each holding a
different "the" generation for one stream, a false-positive reset triggered by this function's own
internals rather than an actual publisher reset.

The fix removes the window instead of trying to time around it: each caller writes its complete
content to its own uniquely-named temp file in the same directory and durably `sync_all`s it
*before* touching the shared target path at all, then publishes with `fs::hard_link(tmp, target)`.
`hard_link` is atomic and fails with `AlreadyExists` for exactly one side when two callers race;
critically, the target can never become observable in a partially-written state, because nothing
ever links a temp file whose content isn't already complete and fsynced. A loser's `read_generation`
immediately after its failed `hard_link` is therefore guaranteed to see the winner's finished
content with no retry loop needed at all. Self-healing is retained, but is now safe to apply
unconditionally the moment the target exists yet reads as `None`: since this function's own writes
can no longer produce that state, observing it here can only mean genuinely pre-existing wreckage
unrelated to a live racer (e.g. external corruption, or a leftover from a binary built before this
fix) — never a live writer that self-heal might unlink out from under.

### `WalPosition` schema extension, not a new table or a sidecar file

FR-004 requires the persisted consumer-side position to be scoped to a generation: a recorded
`applied_seq` from a different generation must not be treated as valid in the current one. This
extends the existing `WalPosition` table (`id STRING PRIMARY KEY, applied_seq INT64`) with a
nullable `generation STRING` column, rather than either alternative:

- **A separate table**, keyed the same way, was rejected because `applied_seq` and its generation
  must be evaluated together for every FR-004 mismatch check, and every write that advances one
  must advance both — splitting them across two tables reintroduces exactly the two-read/write
  coordination problem ADR-0353's single-row design was built to avoid in the first place. A
  torn write (one table updated, the other not, on a crash mid-write) would silently desynchronize
  a value that is only ever meaningful as a pair.
- **A sidecar file outside the database**, as ADR-0365 chose for checkpoints, was rejected because
  the two have opposite lifetime requirements. ADR-0365's own reasoning is that a checkpoint
  *describes the WAL stream* and must outlive the database it happened to be recorded from — it is
  meaningful even against a completely different, freshly rebuilt DB. `applied_seq` and its
  generation describe *this database's own recorded position*; neither is meaningful without the
  database they're paired with, and both should vanish together with it (e.g. on
  `knowledge_clear_all`). That's the same distinction ADR-0365 itself draws to justify staying
  file-resident while ADR-0353 stays DB-resident — this issue's extension inherits that reasoning
  rather than re-deriving it.

ADR-0353 already recorded `WalPosition` as a **deliberate, one-time divergence** from this
repo's graphiti/`kuzu_driver.py` schema-parity rule (graphiti has no equivalent table, since it
never tracks an applied WAL position itself). This issue's extension is an explicit, acknowledged
**continuation** of that existing divergence, not a second, independent one — the same table, the
same rationale, one more nullable column.

`Conn::get_applied_seq`/`set_applied_seq` become `get_wal_position` (returns a
`WalPositionRecord { applied_seq: Option<u64>, generation: Option<String> }`) and
`set_wal_position(group_id, seq, generation: Option<&str>)` — a **required signature change**
across roughly a dozen call sites, not a pair of new, loosely-coupled methods. Every call site
that persists `applied_seq` already has a natural "generation observed right now" value in hand
at that moment (the on-disk value the same code path just read, or the writer's own cached
value), so requiring it costs nothing and gives compiler-enforced completeness: a partial rollout
— some call sites tracking generation, others silently not — would be worse than not having the
feature at all, since a consumer would trust a field that isn't consistently written. This choice
also makes FR-009's "adopt on first encounter" fall out for free, with no separate branch:
adoption is simply "the write always carries whatever generation is currently on disk," including
carrying `None` forward when the source itself has none yet.

### Detection: one insertion point in `knowledge_rebuild_from_wal`, row-existence-aware

> **Amended by [ADR-0414](0414-wal-generation-unknown-refuses-replay.md).** The "current on-disk
> generation is unreadable → never a mismatch" rule below was, as originally decided, permanent
> and indefinite: a stream that never had a generation was designed to keep booting and replaying
> normally forever. Issue #414 found this made ADR-0387's own detection inert end-to-end in
> real-world hydrated channels (every real hydrated stream reported `generation: null`, silently,
> with reset detection never once having a value to compare) and reversed that tolerance for one
> specific case: once a position has already been recorded for a group, a subsequent
> `knowledge_rebuild_from_wal` call against an unknown current generation now refuses outright
> instead of silently proceeding. The rule below is unchanged for a group's *first* encounter
> (`applied_seq: None`), which still adopts an unknown generation exactly as designed here.

Detection lives in `handle_rebuild_from_wal` only — not in ordinary live ingest, and not in
`knowledge_status` (which only *reports* generation, FR-003) — inserted once, before the existing
`from_seq == 0` non-empty-group guard, so it covers all three downstream replay paths (streaming,
non-streaming dry-run, non-streaming background job) from a single point rather than tripling the
check. This matches the orac/zen deployment model (ADR-0353's context): a consumer calls
`knowledge_rebuild_from_wal` to catch up, and that is the one call site with both "what I last
recorded" and "what's on disk now" in hand simultaneously.

The comparison is **not** a simple "both sides `Some` and different" equality check. That rule is
correct for a checkpoint's *immutable* recorded generation (`wal_generation::generation_mismatch`,
used by FR-007's reachability check — a checkpoint's generation is set once, at creation, and
never revisited, so a `None` recorded value stays permanently generation-blind by design) but
wrong for the *live, self-updating* `WalPosition` row. Every successful completion write adopts
whatever generation is currently on disk, including adopting `None` when the stream has none yet
— which means a **row that already exists with a `None` generation is a real recorded value, not
an unset placeholder**, and a later check where the stream's generation has become `Some` (the
stream was genuinely reset and republished, now carrying a generation for the first time) must
still be caught. FR-009's own Story 5 Scenario 3 requires exactly this. The correct rule
(`wal_generation::position_reset_detected`) is therefore gated on whether a row exists at all
(`applied_seq.is_some()`), not on whether the recorded generation itself is `Some`:

- No row yet (`applied_seq: None`) → never a mismatch; nothing to compare against, and the
  replay this check gates performs the adoption itself.
- The current on-disk generation is unreadable (`None` — missing or corrupt sidecar) → never a
  mismatch, symmetrically with the checkpoint-side rule; a damaged-but-harmless artifact must
  never fake a reset (Edge Cases).
- A row exists and the current generation is known → compare `recorded != current` for real,
  including a recorded `None` against a current `Some` — this is what makes Story 5 Scenario 3
  work.

On a genuine mismatch the caller's `from_seq`/`to_seq`/`force_clear` are overridden entirely —
self-heal always means "purge the group (#361), full replay from `seq: 0` (ADR-0378 FR-006),
re-bind cross-group pointers (FR-010, below)" — consistent with ADR-0353's own precedent that a
corpus reset is a case a consumer must self-heal from, not fail loudly and wait for an operator.
`dry_run: true` against a mismatched group reports the same `reset_detected`/
`previous_generation`/`generation` fields but purges and replays nothing, matching every other
dry-run path in this codebase (`group_purge`, `checkpoint`, this same handler's pre-existing
non-empty-group guard).

### FR-010: the post-replay re-bind is part of the same operation, not a follow-up call

`clear_group_for_rebuild`'s purge (via `group_purge::purge_groups`) already force-unbinds every
cross-group pointer into the group being cleared, correctly, *before* replay runs. But that alone
satisfies FR-005 while leaving SC-004 unmet: a purge-and-replay with no further action leaves the
layer graph still bound to nothing (or, at best, requiring a separate, easy-to-forget
`knowledge_rebind_pointers` call) even though the group now holds real, new-generation content.
The self-heal in `handle_rebuild_from_wal` therefore also calls the plain, staleness-gated
`cross_group::rebind_pointers` (not the `_forced` variant `group_purge` already used pre-replay)
once, immediately after the post-replay `set_wal_position` write succeeds, inside the same
`spawn_blocking` closure and lock scope as the replay itself — so a caller observing
`reset_detected: true` in the response has already gotten a fully-recovered layer graph in the
same call, with `cross_group_rebind` (a `RebindCounts`) surfaced for auditability. No
`unbound_impacts`-driven explicit list is needed: the ordinary staleness gate correctly re-checks
every affected pointer on its own once `applied_seq` has advanced past what was recorded
pre-purge, which is exactly what makes the spec's "no new discovery mechanism" claim true.

### `knowledge_status` reports the on-disk (source-side) generation, not lcg's own recorded value

`knowledge_status`'s `wal.generation`/`wal_groups[*].generation` mirror `wal_max_seq`'s existing
role: "what's actually on disk right now," read via `wal_generation::read_generation` alongside
the same manifest-backed machinery `wal_max_seq` already pays for (SC-007 — no new full-directory
scan). This is deliberately the on-disk value, not lcg's own DB-recorded consumer-side position
(the one `WalPosition`/`get_wal_position` holds and `knowledge_rebuild_from_wal`'s detection
actually compares against): an external consumer comparing "is this the same stream I was
tracking?" needs the source-side truth, the same way it already needs `wal_max_seq` rather than
its own last-applied cursor to answer "has this stream moved?". lcg's internal recorded
generation has no separate consumer-facing use and is not exposed by `knowledge_status`.

## Consequences

- **Wide, but bounded, blast radius.** `get_wal_position`/`set_wal_position`'s signature change
  touches every prior `get_applied_seq`/`set_applied_seq` call site — `db.rs`, `handlers.rs`
  (`knowledge_status`, `knowledge_wal_mark_create`, all three `knowledge_rebuild_from_wal` replay
  paths, `clear_group_for_rebuild`), `recovery.rs` (`backfill_applied_seq_if_absent`,
  `run_full_recovery_sequence`, and `recover_rebuild_from_workspace_wal`), `episode.rs`,
  `wal_exec.rs`, `cross_group.rs`, and every test file exercising any of them. Each is a
  mechanical thread-through, not a design decision, but the count is real: this is not a
  single-function change.
- **New public API surface, additive only.** `knowledge_status`'s `wal`/`wal_groups` objects gain
  `generation`; `knowledge_wal_mark_create`/`knowledge_wal_mark_list`'s checkpoint records gain
  `generation`; `knowledge_rebuild_from_wal`'s result gains `reset_detected`,
  `previous_generation`, `generation`, and `cross_group_rebind`. Every existing field is
  unchanged, and no new `knowledge_*` dispatch method was needed — five `ToolSpec` descriptions
  got prose updates, no registry count or scope-bucket change.
- **`WalPosition` gains a second documented graphiti-parity divergence column**, on top of the
  `applied_seq` divergence ADR-0353 already recorded — see the schema-extension rationale above.
- **No new `AppState`/`Db` struct field.** The generation lives on disk (the sidecar) and in the
  DB row (`WalPosition.generation`), read fresh on every check — the same "must survive restart,
  not memoisation" constraint ADR-0353 established, avoiding the constructor-sweep risk a new
  field would add across every hand-built `AppState` test fixture in this codebase (CLAUDE.md
  #46/#58).
- **The live-ingest hot path (`episode.rs`, once per WAL chunk) reads the writer's own cached
  `WalWriter::generation()`, never a fresh disk read**, so this feature adds zero filesystem I/O
  to the highest-frequency write path in the system.
- **`recover_rebuild_from_workspace_wal`** (the `knowledge_recover_full`/whole-database recovery
  path, out of scope for behavioral changes per the spec) still needed its
  `get_applied_seq`/`set_applied_seq` call site updated to the new signature for the codebase to
  compile, and now persists the on-disk generation it observes per group — a mechanical
  consequence of the signature change, not a new detection or self-heal behavior for that path.
- **`wal_flush_ungrouped` remains generation-blind**, exactly as it remains `applied_seq`-blind
  today (the #383 gap, explicitly named out of scope by this issue's own Assumptions section) —
  there is nothing for this issue to make generation-aware there until #383 lands.
- **Cross-repo**: orac/zen and any other non-lcg publisher are downstream consumers/producers of
  `.wal-generation.json` and the `knowledge_status`/`knowledge_rebuild_from_wal` response
  fields — out of scope for this repo's change, but the entire motivation for it (see
  `docs/operations.md`'s publisher-writable format documentation).
