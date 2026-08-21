# ADR-0432: `force_clear` Rebuild Guard Scans WAL Content for Referenced `group_id`s

**Status**: Accepted
**Date**: 2026-08-21
**Issue**: #432
**Relates to**: ADR-0353 (origin of the `force_clear` guard), ADR-0378 (multi-stream WAL per
group directory, which scoped the guard per group_id)

## Context

`knowledge_rebuild_from_wal`'s `force_clear: true` pre-clear guard (issue #353, FR-005) exists so
a `from_seq: 0` full rebuild against a group that already has data fails fast, or clears that
data first, instead of re-issuing a native `CREATE` for every already-existing row and producing
a duplicate-primary-key failure per node. Issue #378 scoped this guard per `group_id`, so
rebuilding one group's data doesn't collide with, or get blocked by, another group's data.

Both the emptiness check and the clear it triggers have always operated on **the request's own
`group_id`** — the group that owns the WAL directory being replayed, defaulting to
`DEFAULT_GROUP_ID` (`"liminis"`) when the caller omits it. This is correct as long as a WAL
directory's owning group_id always matches the `group_id` value(s) embedded in that directory's
own content (the `group_id` param on each row's Cypher parameters). For any WAL directory written
directly by this codebase since #378, that invariant holds by construction — every write goes
through the per-group directory the mutation's own `group_id` selects.

It does not hold for a **migrated legacy WAL stream**. Before #378, WAL content was written flat,
with no directory-level group scoping. `wal_group::migrate_wal_root_if_needed` relocates any such
pre-#378 flat/legacy stream into the directory for `DEFAULT_GROUP_ID`, purely because that is the
only directory that existed for it to land in — regardless of what `group_id` value(s) the
stream's own rows actually carry in their params. A legacy stream whose rows were written for a
different group (e.g. `"apollo_program"`) ends up living inside `DEFAULT_GROUP_ID`'s directory,
and a rebuild request against it (which naturally defaults to `DEFAULT_GROUP_ID`, since that's the
directory's owning group) triggers an emptiness check against `DEFAULT_GROUP_ID` — which may well
be empty — while replay itself issues mutations against the real, already-populated
`"apollo_program"` group embedded in the content. The guard passes, no clear happens, and replay
proceeds directly against populated data it was never checked against.

This was discovered while investigating issue #429's pre-existing test failures:
`mcp_real_corpus_admin_data_e2e.rs`'s `force_clear: true` full-rebuild block against the
real-corpus fixture — itself exactly this scenario, a pre-#378 flat stream migrated into
`wal_root/liminis/` whose content carries `"group_id":"apollo_program"` — produced
`mutations_replayed: 8356, failed_lines: 3131` out of ~11,487 WAL lines, all silently logged as
`[WAL WARN]` output rather than surfaced as a request-level failure. Any real deployment that
predates #378, whose flat stream contains rows for a non-default group_id, hits the identical
collision on its first post-upgrade `force_clear: true` full rebuild.

## Decision

The guard now determines emptiness — and, if `force_clear: true`, performs the clear — across
**every `group_id` actually referenced by the WAL directory's own content**, not only the
request's own `group_id`.

Concretely: `wal::scan_wal_content_group_ids(wal_dir, to_seq)` reads every `*.jsonl` file in the
directory and collects the distinct `params.group_id` string values found across every line
(tolerating a malformed line or unreadable file by skipping it, mirroring `first_seq_in_file`'s
tolerance model). The scan is bounded by the request's own `to_seq` when set: a line with
`seq > to_seq` is excluded, mirroring `ReplayOptions::to_seq`'s own filter in `replay.rs`. This
matters for the documented bounded-full-rebuild call shape (`{from_seq: 0, to_seq: n}`, see
`docs/operations.md`'s "Bounded rebuild" section) — without the bound, the scan could discover a
group_id whose only rows live past `to_seq`, and `force_clear: true` would then purge that
group's real data even though the bounded replay that follows will never recreate it, a genuine
data-loss gap the bound closes. This set, unioned with the request's own `group_id`, becomes the
group of candidates the emptiness check runs against (`count_entities_by_group_ids`/
`count_episodics_by_group_ids`/`count_relates_to_by_group_ids`, unchanged — already
`&[&str]`-shaped, called once per candidate instead of once total). The three existing response
branches (dry-run refusal, no-`force_clear` refusal, `force_clear`-triggered clear) key off
*which* groups in that union are non-empty, and their error messages enumerate every colliding
group rather than naming only the request's own.

`clear_group_for_rebuild` is generalized to `clear_groups_for_rebuild`, purging the full
referenced set via the existing `group_purge::purge_groups` (already `&[&str]`-shaped — issue
#361 had already built it to accept multiple groups in one atomic call for unrelated reasons).
Every referenced group's own purge-mutation bucket is dropped rather than flushed to its own WAL
stream — not only the request group's, as before — because every referenced group's data, not
just the request group's, is about to be recreated by the one imminent replay of this WAL
directory's content. Only genuinely foreign buckets (a group outside the referenced set,
encountered via a forced pointer-rebind side effect) are still flushed to their own streams.
`WalPosition` is reset to 0 only for the request's own `group_id` — a `WalPosition` is tied to a
*physical* WAL directory, and a foreign-but-referenced group discovered only inside the request
group's migrated legacy content has no WAL directory of its own driving this replay, so there is
nothing meaningful to reset for it.

### Rejected alternatives

**Restructure `migrate_wal_root_if_needed` to split a legacy stream into per-embedded-group_id
directories at migration time**, so a later rebuild never needs to rediscover the embedded
group_id(s) by scanning content. Rejected: this only affects *future* migrations. Migration is
idempotent and only acts on loose top-level files — once a legacy stream is already inside a
group directory (as any deployment that migrated under a pre-#432 binary already has), it is
never re-inspected or re-split. This cannot fix deployments already in the wild, which is exactly
the scenario this issue exists to close.

**Stamp a migration-time marker** (a sentinel recorded when a flat stream is relocated) and gate
the content scan on its presence, paying the scan cost only for directories known to need it.
Rejected for the identical reason: an already-migrated directory from before this fix ships has
no marker, so a marker-gated scan would skip exactly the directories that need it.

A content-based scan performed at rebuild-guard time is the only mechanism that is correct for
both new and pre-existing migrated deployments — the only one that actually protects the
deployments the issue describes.

**Have the emptiness check call `group_purge::purge_groups(..., dry_run: true)`** instead of its
own per-group counting loop, to reuse one code path for both the check and the clear. Rejected:
`purge_groups` unconditionally also computes `unbound_impacts` (cross-group pointer analysis),
which the guard's "is there data I'd collide with?" question doesn't need and today's
single-group guard never paid for. Keeping the emptiness check as an iterated `count_*_by_group_ids`
loop avoids adding that unrelated cost to every full-rebuild guard check. The real clear path
already pays for `unbound_impacts` unconditionally (it always has), so extending its `group_ids`
slice to the full referenced set adds no *new* category of cost there.

## Consequences

- **Closes the silent-data-loss window for migrated legacy WAL streams.** A `force_clear: true`
  full rebuild against such a stream now clears the group(s) its content actually targets before
  replay, eliminating the duplicate-primary-key failures that previously went unnoticed as
  `[WAL WARN]` log lines rather than a request-level error (SC-001).
- **A full-content pre-scan adds I/O to every `from_seq: 0` full rebuild**, even the common,
  unaffected case where the WAL directory's content group_id already matches its owning group_id
  — the scan reads and JSON-parses every line's envelope before replay reads and parses (and
  executes) the same lines again. This is an accepted trade-off: no mechanism correct for
  already-migrated deployments can avoid inspecting content, and the added cost is bounded by
  "read + parse, no Cypher execution or write" versus replay's "read + parse + execute + write" —
  a small fraction of total replay wall-clock time even at the ~43,820-file scale ADR-0026
  documents. It does not change the check's *outcome* for the common case (FR-007/SC-003):
  behavior, error shape, and success shape are unchanged whenever the referenced-group union is
  exactly `{group_id}`.
- **Error messages naming a non-empty group now enumerate every colliding group_id**, not only
  the request's own — a caller or operator parsing these messages for a single group_id must
  handle a list.
- **This does not retroactively repair data already silently dropped** by a prior `force_clear:
  true` rebuild that hit this defect before this fix shipped — out of scope, per the issue.
- **Does not change** the `from_seq > 0` incremental-resume path (this guard never runs there,
  unaffected by this issue) or any other WAL/group-scoped operation that doesn't share this
  guard's request-group-vs-content-group assumption (`knowledge_delete_by_group`, `group_purge`
  itself, etc. were not found to share this defect during Research).

## Amendment (issue #462): clearing a referenced group in full is not always correct

This ADR's original decision clears **every** referenced group_id's data in full via
`group_purge::purge_groups`, with no regard for where else that group's data lives. That is
correct only when the referenced group's *entire* footprint lives inside the directory being
replayed — the migrated-legacy scenario this ADR was written for. It is not correct when the
referenced group also has rows in a **separate, independent WAL stream directory** that this
replay does not touch: a post-#378 per-group directory the group has been written to normally
since migration, or another migrated legacy location entirely. A whole-group purge in that case
destroys the independent stream's rows — this replay can never recreate them, because it never
reads that stream. This was caught by
`mcp_real_corpus_mutation_e2e::mcp_write_path_over_real_corpus_fixture` regressing between #432's
and #442's merges: the real-corpus fixture is exactly this ADR's scenario (a pre-#378 flat stream
migrated into `liminis`, embedding `group_id: "apollo_program"`), and a chunk written afterward
directly into `apollo_program`'s own post-#378 stream was silently dropped by a subsequent
`force_clear: true` rebuild of `liminis` — entity count 1507 → 1506.

The underlying invariant this ADR's guard exists to serve was always "clear exactly what the
imminent replay is about to recreate" — this amendment makes the clear match that invariant
exactly, rather than approximating it at group granularity.

### Amended decision

For each group_id referenced by the WAL directory's content (other than the request's own —
which, by construction of `resolve_group_wal_dir`, never has data split across directories and is
always cleared in full exactly as before):

1. **Not split** — the referenced group has no separate, non-empty WAL directory elsewhere
   (`wal_group::group_wal_dir(wal_root, gid)` differs from the directory being replayed, and
   either doesn't exist or has no `.jsonl` content). Cleared in full via `purge_groups`, unchanged
   from this ADR's original decision — this remains the common migrated-legacy case, and its
   existing regression coverage (`handlers_wal_admin.rs`'s
   `test_rebuild_from_wal_migrated_legacy_stream_force_clear_clears_embedded_group`) continues to
   pass unmodified.
2. **Split, and every line in the replayed directory referencing that group is a bare `CREATE`**
   (never `SET`/`DELETE`/`DETACH`/`REMOVE`) — cleared via a new row-scoped purge,
   `group_purge::purge_group_rows`, which deletes only the exact `uuid`s a bare `CREATE` line
   creates for that group (`wal::scan_wal_content_by_group`'s `GroupWalContent::create_uuids`,
   itself bounded by `to_seq` exactly as this ADR's original `scan_wal_content_group_ids` was).
   This clears precisely the replay restore set for that group: no more (the independent stream's
   rows are never named, so they survive), and no less (the rows this replay *will* recreate are
   still cleared first, so #432's duplicate-primary-key collision does not return). `MATCH ...
   CREATE` relationship-hop lines (the two-hop/direct-rel shape every edge insert also emits) are
   not required to be bare `CREATE` — they only link existing nodes by uuid, never mutate or
   remove a row this replay doesn't own, so their presence doesn't change a group's classification
   here.
3. **Split, and some line referencing that group is a mutating non-`CREATE`
   (`SET`/`DELETE`/`DETACH`/`REMOVE`)** — such a line implies this replay might reach into a row
   it doesn't fully own (e.g. a legacy `SET` on a row actually still maintained by the group's
   independent stream). Clearing either the whole group (risking the independent stream's data)
   or just the row-scoped set (risking a duplicate-primary-key collision on replay, since the
   `SET` line's own history wouldn't have been cleared) is unsafe in a way this guard cannot
   resolve silently in either direction. The rebuild is refused outright — an `Error::Ipc` naming
   the group(s), raised before any clear happens — the same fail-fast pattern this guard already
   uses for its sibling refusals (dry-run, no-`force_clear`), and observable from the rebuild
   call's own error response with no new response shape needed.
4. **Split, otherwise row-scope-clearable (case 2 above), but the DB currently has a live
   two-hop `RELATES_TO` connection from one of that group's `create_uuids` into a
   `RelatesToNode_` row that is itself *not* in `create_uuids`** — added after review found that
   case 2's row-scoped `DETACH DELETE` can silently sever that connection. An `Entity`'s
   `DETACH DELETE` removes every incident relationship, including a hop into a surviving
   `RelatesToNode_` this replay does not also recreate (its own `CREATE` — and therefore its
   connecting hop `CREATE` — lives in the group's other, un-replayed stream, the same split
   condition that makes case 2 exist at all). `purge_group_rows`'s forced-rebind pass does not
   repair this: it only re-resolves `RelatesToNode_` rows carrying a genuine cross-group pointer
   (`crate::pointer::CrossGroupPointer`), not an ordinary same-group edge created directly by
   `db::Conn::insert_relates_to_edge`. Detected via
   `db::Conn::find_relates_to_dangling_after_uuid_purge`, checked both in the initial (unlocked)
   classification and again in the locked freshness re-check immediately before the purge; a hit
   in either refuses the rebuild with the same fail-fast `Error::Ipc` pattern as case 3, just
   naming the severed-connection cause rather than a mutating-Cypher-line one.

`cross_group::rebind_pointers_forced`'s forced-rebind pass, reused unchanged by
`purge_group_rows`, needed no logic change for the partial-purge case: its actual mechanism
(`resolve_endpoint`, a real per-pointer existence re-check) is correct regardless of whether the
source group was emptied in full or only partially — only the function's doc comment, written for
the whole-group case, needed a wording correction. That mechanism is unrelated to case 4 above,
though: pointers and ordinary two-hop edges are two different mechanisms, and only case 4's
caller-side check protects the latter.

This amendment does not reopen anything this ADR's original "Rejected alternatives" section
already settled — it narrows *how much* of a referenced group's data gets cleared, not *which*
groups get discovered or *why* the content scan is necessary in the first place.

See `docs/operations.md`'s "Bounded rebuild" section for the operator-facing description of the
resulting behavior, including the new refusal case and its remedy.
