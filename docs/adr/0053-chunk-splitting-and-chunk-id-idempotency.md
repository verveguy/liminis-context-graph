# ADR-0053: Chunk-Splitting and Chunk-ID Idempotency for `knowledge_process_chunk`

**Status**: Accepted
**Date**: 2026-07-30
**Issues**: #284 (subsumes #282's undelivered threshold/warning/telemetry baseline)

## Context

`knowledge_process_chunk` (`crates/core/src/handlers.rs::handle_knowledge_process_chunk`) is the
graph engine's documented ingestion entry point. #282 established that extraction quality
degrades sublinearly as `chunk_text` grows — a single oversized call silently succeeds at a small
fraction of achievable extraction yield — and specified an advisory threshold plus a warning
response. #282 was never implemented on `main` or on its own branch; #284 subsumes that baseline
(threshold, warning, `ChunkTextOversized` telemetry) as part of delivering its own scope: a
bounded degradation path for oversized `chunk_text`, and idempotency semantics for resubmitting a
`chunk_id`.

Two design questions had to be resolved:

1. **Split or reject?** #282's FR-004 requires default behavior to remain accept-and-warn, never
   reject. A reject path would need to be opt-in for no benefit over splitting.
2. **What happens when a `chunk_id` is resubmitted?** Before this issue, `episode::add_episode`
   always `CREATE`s a new `Episodic` node (ADR-0046: the write path is CREATE-only, never MERGE,
   for `Entity`/`Episodic`/`RelatesToNode_`), so resubmitting a `chunk_id` silently produced a
   second, unrelated episode with no relationship to the first
   (`test_knowledge_process_chunk_duplicate_chunk_id`, pre-#284, asserted exactly this).
   `remove_episodes_by_chunk_id` already tolerates N episodes per `chunk_id` on the delete path,
   but nothing on the write path gave that fact a defined meaning.

`EpisodicRow` (`crates/core/src/types.rs`) has no spare field to persist a content-hash or a
unit index — adding one would carry the schema-parity-with-`kuzu_driver.py` migration cost this
project's `CLAUDE.md` calls out for any `schema.rs` change. Any solution had to fit inside the
existing field set.

## Decision

**Split, don't reject.** `chunk_text` above `LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS` (default 8,000
chars, `chars().count()`, never byte length) is split by `chunk_split::split_into_units` into
threshold-sized units, each ingested as its own `add_episode` call, all sharing the caller's
`chunk_id`. The splitter prefers the nearest whitespace boundary at or before each `max_chars`
window, falling back to a hard cut only when a unit contains no whitespace (the unbreakable-token
edge case). Two invariants make the design below sound and are asserted directly in
`chunk_split.rs`'s unit tests: every unit's `chars().count() <= max_chars`, and
`units.concat() == original_text` exactly (lossless, order-preserving).

**Unit index lives only in `source_description`, never in `Episodic.name`.**
`remove_episodes_by_chunk_id` matches `ep.name` by exact string equality against `chunk_id`
(`crates/core/src/db.rs`) — any suffix there would silently break deletion for split units. So
`name` stays `chunk_id` verbatim in both the split and non-split cases, and `source_description`
carries the observability marker: `"{source_file}:{chunk_id}"` unchanged for a never-split chunk,
or `"{source_file}:{chunk_id}#{unit_1_based}/{unit_count}"` for each unit of a split chunk. This
also leaves `remove_episodes_by_source`'s `STARTS WITH "{source_file}:"` prefix match untouched.

**Idempotency: content-comparison-gated no-op/replace, using `content` directly instead of a
stored hash.** Because splitting is lossless and order-preserving, a chunk_id's prior
`chunk_text` can be reconstructed by reading back its existing `Episodic` row(s)' `content` and
concatenating them in unit order — no hash, no new field, no schema change. This is what unifies
the never-split and split cases into one mechanism (the spec's FR-007): reconstruction is just
"concatenate however many rows there are, in unit order."

Every `knowledge_process_chunk` call — including a `chunk_id` never seen before — runs this
lookup first, under only a *read* acquisition of `write_lock` (escalated to a write acquisition
only if a replace-delete actually needs to happen — see Consequences):

1. Query existing `Episodic` rows for `(chunk_id, group_id)`
   (`Db::get_episodes_by_chunk_id`, the read-only counterpart to `remove_episodes_by_chunk_id`).
2. Reconstruct (`handlers.rs::reconstruct_prior_chunk_text` → `PriorState`), counting only rows
   whose `source_description` ends with `":{chunk_id}"` — exactly, for the never-split shape, or
   that ending immediately before a parseable `#i/N` suffix for a split unit — regardless of
   what `source_file` precedes it. `Episodic.name` is not exclusive to this handler
   (`knowledge_add_episode` lets a caller set an arbitrary `name`), so a row that merely shares
   the same `name` but doesn't match this ending convention is a different endpoint's data and
   is excluded entirely: never reconstructed into, never deleted (see Consequences). Matching is
   deliberately *not* anchored to the current call's `source_file`: a caller may legitimately
   resubmit a `chunk_id` under a renamed/moved `source_file`, and FR-006/FR-007 idempotency must
   still recognize that as the same lineage.
   - No matching rows → `PriorState::None` (first-time ingest for this lineage, unaffected —
     any foreign row sharing the `name` is left alone).
   - One matching row with no `#i/N` suffix → its `content` **is** the prior `chunk_text`.
   - N matching rows whose suffixes form a complete `1..=N` sequence with consistent `N` → sort
     by `i`, concatenate `content`.
   - Anything that doesn't parse cleanly (missing indices, inconsistent `N`, a mix of split and
     non-split shapes in the same lineage) → `PriorState::Anomalous`, carrying exactly the UUIDs
     that belong to this lineage. Deliberately conservative: an ambiguous prior state is always
     treated as a mismatch, never guessed into a no-op.
3. Compare the reconstructed text to the incoming `chunk_text`:
   - **Equal → no-op.** Skip extraction/embedding entirely and return the existing episode
     UUID(s) with `idempotent: true`. This is the reason the comparison happens *before* any LLM
     call, not after: extraction is nondeterministic, so re-running it on a byte-identical retry
     is not guaranteed to reproduce the same entities/edges, and `remove_episodes_by_chunk_id`'s
     own doc comment already warns that orphaned entities from a deleted episode are not cleaned
     up — a naive replace-always design would risk both.
   - **Different (including `Anomalous`) → replace.** Proceed with a fresh ingest of the incoming
     `chunk_text` (single episode or split, depending on its size) *first*, and only once that
     ingest fully succeeds, delete exactly the UUIDs already known to belong to this lineage via
     `Db::remove_episodes_by_uuids` (never a name-based delete, so a foreign row is never
     touched), flushing that delete's WAL the same way `handle_delete_chunk_episode` does
     (`wal_exec::wal_flush_ungrouped`). Deleting *after* the new ingest succeeds, rather than
     before, means a failed ingest (e.g. an extraction error) never leaves the `chunk_id` with
     zero episodes — the prior lineage stays in place until its replacement actually exists.

**Mid-split partial failure has no explicit rollback — it self-heals via step 2/3 above.** Each
unit of a split is its own `add_episode` call with its own `write_lock` acquisition and WAL
flush (Research's Constraint 5); if unit *i* of *N* fails, units `0..i-1` are already committed
and the error propagates to the caller. For a fresh ingest (no prior lineage), a retry of the
same call reconstructs only the partial units, finds `PriorState::Anomalous` (the index sequence
is incomplete), and takes the replace path — deleting the partial leftovers and re-splitting from
scratch. For a *replace* whose new ingest fails partway, the prior lineage was never deleted (see
above), so the retry's reconstruction sees the old lineage's rows plus the new partial units
together — inconsistent shapes/counts, so still `PriorState::Anomalous` — and the replace path
deletes that whole combined set before re-splitting from scratch. Either way, no separate
partial-failure-detection code was needed.

**Response shape changes only where behavior is new.** The below-threshold,
never-seen-`chunk_id` path is byte-identical to pre-#284 (`episode_uuid`, `nodes_extracted`,
`edges_extracted`, `duration_seconds` — no new keys), satisfying FR-001. A split response
replaces `episode_uuid` with `episode_uuids` (array, unit order) + `unit_count`, sums
`nodes_extracted`/`edges_extracted` across units, and attaches a `warning` object. A no-op adds
`idempotent: true` and omits `nodes_extracted`/`edges_extracted` (nothing was extracted —
reporting `0` would misleadingly imply an empty extraction ran). A replace adds `replaced_uuids`
naming what was deleted, on top of whatever shape the fresh ingest of the new text produces.

## Consequences

- **A `chunk_id` now maps to multiple episodes by design, not just as a delete-path curiosity.**
  `remove_episodes_by_chunk_id`/`handle_delete_chunk_episode` needed no code change — the
  multi-episode-per-`chunk_id` tolerance they already had is precisely what splitting relies on.
- **Every `knowledge_process_chunk` call now runs one extra read query** (the chunk_id lookup),
  even for a `chunk_id` never seen before — an unindexed exact-match scan on `Episodic.name`, the
  same query shape the delete path already runs. Small, but real; not free. This lookup takes
  only `write_lock.read()`, not `.write()`: the common case (a new chunk_id, or a no-op
  resubmission) is a pure read, and only the replace branch escalates to `write_lock.write()`
  for its delete. An earlier revision of this design took the write lock for the whole lookup,
  which would have serialized *all* concurrent chunk ingestion — including unrelated chunk_ids —
  behind an exclusive lock up front rather than only at Phase C's brief commit, as review caught.
- **Reconstruction is anchored to this call's own `source_description` convention, not just
  `Episodic.name`.** `knowledge_process_chunk` and `knowledge_add_episode` both write to the same
  `Episodic.name` field with no shared namespace — a caller could coincidentally (or
  adversarially) `knowledge_add_episode` with `name` equal to a `chunk_id` used elsewhere. Prior
  to this anchoring, `handle_knowledge_process_chunk` would pick up such a foreign row as
  `PriorState`, almost certainly find its content didn't match the incoming `chunk_text`, and
  silently `DETACH DELETE` it as a side effect of a routine ingest call — data loss with no
  explicit delete action from the caller. Anchoring reconstruction (and the resulting delete) to
  rows whose `source_description` ends with `":{chunk_id}"` (exactly, or plus a parseable `#i/N`
  suffix) closes this: a foreign row is left untouched, and `chunk_id` resubmission proceeds as a
  fresh ingest instead. `handle_delete_chunk_episode` (an explicit, caller-invoked deletion) is
  unaffected by this change and still matches/deletes by `name` alone — that endpoint's contract
  has always been "delete everything with this name," which is a different, and here still an
  intentionally accepted, risk from routine ingestion silently destroying data. The
  `":{chunk_id}"`-suffix anchor narrows, but does not eliminate, the foreign-row risk: a
  `knowledge_add_episode` caller whose `name` equals a `chunk_id` *and* whose own
  `source_description` happens to end in `":{chunk_id}"` too (e.g. a caller mirroring this
  handler's own `"{source_file}:{chunk_id}"` convention, such as `"notes:chunk-42"`) still gets
  pulled into that chunk_id's reconstructed lineage and is a candidate for deletion on a
  subsequent replace. This residual collision is accepted for the same reason as the
  `source_file`-independence tradeoff below: `Episodic.name` has no namespace separating
  `knowledge_process_chunk` lineages from `knowledge_add_episode` callers, and closing it
  completely would require either a schema-level provenance marker (a schema change, per
  Research's Constraint 4) or a reserved delimiter convention enforced across both handlers —
  out of scope for this issue. Deliberately *not*
  anchored to the current call's `source_file`, since a caller may legitimately resubmit a
  `chunk_id` under a renamed/moved `source_file` and idempotency must still recognize that as the
  same lineage — an earlier revision of this design required an exact `"{source_file}:{chunk_id}"`
  match, which broke idempotency across a `source_file` rename (review caught this too). This
  `source_file`-independence has a wider blast radius than just the rename case: two *unrelated*
  documents from different callers that happen to reuse the same `chunk_id` under different
  `source_file`s are indistinguishable from a rename to this matching logic, and one submission
  will replace the other's episodes exactly as if it were a legitimate resubmission. This is an
  accepted consequence of `chunk_id` being the caller-supplied idempotency key with no
  `source_file` namespacing: callers must treat `chunk_id` as unique per logical document within
  a `group_id`, not merely unique per `source_file`. No enforcement of this uniqueness is done
  (or feasible) server-side — it is a documented caller contract, not a runtime check.
- **A replace's delete is deferred until after the new ingest succeeds, not performed up front.**
  An earlier revision deleted the prior lineage before starting the fresh ingest; if that ingest
  then failed (e.g. an extraction error partway through a split), the `chunk_id` was left with
  zero episodes — the caller's previously-good content gone, the new content never written, and
  no leftover to self-heal from (unlike the mid-split-partial-failure case, which always has a
  prior-lineage fallback). Performing the fresh ingest first and deleting the superseded UUIDs
  only once it fully succeeds (`Db::remove_episodes_by_uuids`, same as before) closes this
  window: the prior lineage stays intact until its replacement actually exists.
- **`source_description`'s `#{i}/{N}` suffix is now a load-bearing convention**, consumed by
  `reconstruct_prior_chunk_text`. A future change to how `source_description` is built for chunk
  ingests must preserve or explicitly migrate this format, or idempotency reconstruction silently
  starts treating every existing split chunk as `Anomalous` (safe — it replaces rather than
  corrupting — but surprising and expensive if unintended).
- **Existing tests that assumed one `chunk_id` call → one `episode_uuid`, or that duplicate
  `chunk_id` submission was non-idempotent, needed updating**, per the spec's FR-009/SC-005:
  `test_knowledge_process_chunk_duplicate_chunk_id` (`ipc_parity.rs`) flipped from `assert_ne!`
  to `assert_eq!`; `delete_chunk_episode_all_revisions` (`tier1c_deletion.rs`) — which relied on
  resubmission always "appending a revision" — was renamed to
  `delete_chunk_episode_after_idempotent_resubmission` and now asserts 1 episode, not 2.
- **A concurrent `knowledge_process_chunk` call for a *different* `chunk_id` can still interleave
  between units of one split chunk's ingest**, since each unit acquires and releases
  `write_lock` independently rather than holding it for the whole split (consistent with today's
  single-unit behavior; no correctness property depends on cross-unit atomicity).
- **Known limitation: two concurrent calls for the *same* `chunk_id` are not mutually exclusive
  across the lookup-through-insert span.** `write_lock` is held only for the lookup step (a read
  acquisition) and, if needed, the delete step (a separate write acquisition) in
  `handlers.rs::handle_knowledge_process_chunk`, both dropped before the fresh ingest runs;
  `add_episode` only reacquires the lock briefly, per unit, at its own Phase C commit —
  extraction in between runs unlocked. Two calls racing for one `chunk_id` can both observe the
  same prior state (including `PriorState::None`) and both proceed to insert, producing
  duplicate/divergent episodes for that `chunk_id` until a later resubmission self-heals via the
  mismatch/`Anomalous` path. This is not limited to the retry case (a client retry sent while the
  original is still blocked on LLM extraction, which is the scenario this feature is primarily
  meant to make safe) — it applies equally to two independent callers submitting the *same*
  `chunk_id` for the *first* time concurrently, since both observe `PriorState::None` and neither
  sees the other's in-flight write. The idempotency guarantee above therefore only holds for
  *serialized* (non-concurrent) calls for a given `chunk_id`, first-time or resubmission alike;
  the general fix is per-`(group_id, chunk_id)` locking held for the full request duration, which
  needs a new `AppState` field and is a larger change than fits this feature — tracked as a
  recommended follow-up rather than fixed here. Two further shapes of the same underlying gap,
  both review-caught and neither fixed for the same reason:
  - **The split loop itself is not atomic against a concurrent `knowledge_delete_chunk_episode`
    for the same `chunk_id`.** Each unit's `add_episode` call acquires and releases `write_lock`
    independently (no lock is held across the whole split), so an explicit delete landing between
    two units removes whatever is already committed while the loop keeps inserting the units
    still to come — leaving a transient mix of "some pre-delete units gone, some new units still
    arriving" under one `chunk_id` that reads/search can observe mid-flight. This resolves itself
    once the split loop finishes (the `chunk_id` then has exactly the surviving new units), but
    unlike the mid-split-extraction-failure case there is no reconstruction step re-run
    automatically afterward — the caller sees this only if it happens to read during the window.
  - **Two concurrent resubmissions with different `chunk_text` (both taking the Replace path)**
    can both reconstruct the same prior state, both run their own fresh ingest, and both then
    delete the same prior UUIDs via `Db::remove_episodes_by_uuids` — the loser's delete is a
    silent no-op against rows the winner already removed, but the loser's response still reports
    those UUIDs in `replaced_uuids`, naming rows that are already gone by the time the response
    is returned. Not a data-integrity problem (no unintended deletion), but a response-accuracy
    one: `replaced_uuids` can name UUIDs the caller can no longer look up.

## Related

- ADR-0015: WAL Drain-and-Flush Pattern — `wal_flush_chunk` (one atomic group per `add_episode`
  call, i.e. per split unit) vs. `wal_flush_ungrouped` (used here for the replace-path delete,
  same as `handle_delete_chunk_episode`).
- ADR-0038: In-Process `NameIndex` — unaffected; this feature never deletes `Entity` nodes
  (`remove_episodes_by_chunk_id` only ever `DETACH DELETE`s `Episodic`), so no `NameIndex`
  invalidation is needed here, consistent with `remove_episode`/`remove_episodes_by_source`.
- ADR-0046: WAL Replay — Deduplicated Failure Samples and Fail-Fast Rebuild Idempotency —
  establishes the CREATE-only write path this ADR's "replace via delete-then-recreate" design
  follows, rather than introducing an upsert.
- ADR-0047: WAL Replay Transaction Boundaries — confirms replay does not reconstruct
  writer-side chunk grouping, so a mid-split partial commit has no replay-side special case; only
  the handler-level self-healing behavior above was needed.
- `crates/core/src/chunk_split.rs`: the splitter and its concatenation/length-invariant tests.
- `crates/core/src/handlers.rs`: `chunk_text_advisory_max_chars`, `PriorState`,
  `reconstruct_prior_chunk_text`, `handle_knowledge_process_chunk`.
- `crates/core/src/db.rs::get_episodes_by_chunk_id`: the read-only counterpart to
  `remove_episodes_by_chunk_id` this design's reconstruction step depends on.
- `crates/core/src/db.rs::remove_episodes_by_uuids`: deletes exactly a given set of UUIDs (no
  `name`-based lookup), used by the replace path instead of `remove_episodes_by_chunk_id` so a
  foreign row sharing the `name` is never deleted.
- `docs/telemetry.md`: `chunk_text_oversized` event, emitted whenever `chunk_text` exceeds the
  threshold regardless of created/replaced/no-op outcome.
