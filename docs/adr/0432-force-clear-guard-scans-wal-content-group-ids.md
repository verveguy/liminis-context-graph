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

Concretely: `wal::scan_wal_content_group_ids(wal_dir)` reads every `*.jsonl` file in the directory
and collects the distinct `params.group_id` string values found across every line (tolerating a
malformed line or unreadable file by skipping it, mirroring `first_seq_in_file`'s tolerance
model). This set, unioned with the request's own `group_id`, becomes the group of candidates the
emptiness check runs against (`count_entities_by_group_ids`/`count_episodics_by_group_ids`/
`count_relates_to_by_group_ids`, unchanged — already `&[&str]`-shaped, called once per candidate
instead of once total). The three existing response branches (dry-run refusal, no-`force_clear`
refusal, `force_clear`-triggered clear) key off *which* groups in that union are non-empty, and
their error messages enumerate every colliding group rather than naming only the request's own.

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
