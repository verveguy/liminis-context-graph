# ADR-0365: WAL Checkpoints — Directory-Per-Name, Generation-Numbered Exclusive-Create Store

**Status**: Accepted
**Date**: 2026-08-09
**Issue**: #365 (supersedes #363, closed unimplemented)
**Relates to**: ADR-0353 (`applied_seq` / `WalPosition`), ADR-0026 (episode-cursor WAL resume), ADR-0028 (`knowledge_dump_wal`)

## Context

[#362](https://github.com/verveguy/liminis-context-graph/issues/362) made it possible to replay
the WAL to an arbitrary earlier position (`to_seq`), but gave no way to answer "which position was
this graph last known-good at?" other than external bookkeeping. A **checkpoint** — a named,
retained WAL position — is the missing piece: `knowledge_wal_mark_create {name}` records the
database's current `applied_seq` under that name; `knowledge_wal_mark_list` reports every active
checkpoint and whether it is still reachable; `knowledge_wal_mark_delete {name}` removes one.

Two decisions were made before this ADR and are not reopened here:

- **Storage must live outside the database**, in the WAL directory, because a checkpoint's whole
  purpose is surviving the database loss/corruption it exists to recover from. This is why #363
  (the original attempt, database-resident) was closed unimplemented and superseded by this issue.
- **Placement is `<wal_dir>/.checkpoints/`**, not a `.jsonl` file in the WAL directory root — four
  non-recursive, extension-filtered WAL-file scan sites (`wal.rs`'s `count_jsonl_files`/
  `scan_max_seq`, `replay.rs`'s file listing, `recovery.rs`'s `collect_wal_files`, and
  `handlers.rs`'s `count_jsonl_files_in_dir`) would otherwise replay or count a checkpoint file as
  a WAL mutation. A subdirectory is invisible to all of them with no change to any scan.

What remained open going into Plan was the **on-disk record shape** — and it turned out to be the
one design choice this issue could not defer, because two requirements the spec places on it
conflict under the most obvious implementation.

## Decision

Store one **directory per checkpoint name**, containing **generation-numbered files**:

```text
<wal_dir>/.checkpoints/<name>/
    g1.create.json   # {"name","seq","created_at_ms"} — first checkpoint under this name
    g1.delete.json   # {"name","deleted_at_ms"} — tombstone for generation 1 (if deleted)
    g2.create.json   # a later create() reusing the name — new generation, g1 untouched
```

- `create(name, seq)` finds the highest generation `G` present for `name`. If none exists, or the
  highest generation has a `delete.json` tombstone (the name is currently inactive), it targets
  generation `G+1` (or `1`). If the highest generation has **no** tombstone (the name is currently
  active), it deliberately targets the *same* generation `G`, so the write below collides.
- Either way, the create record is written via `OpenOptions::new().write(true).create_new(true)`
  — POSIX `O_EXCL` semantics, atomic across processes on a local filesystem. `AlreadyExists`
  always maps to the same user-facing duplicate-name error, whether the collision came from an
  already-active name or a genuine concurrent race — there is no retry.
- `delete(name)` finds the highest generation; `G == 0` or an existing tombstone at `G` map to the
  "no such active checkpoint" error. Otherwise it writes `gG.delete.json`, also via `create_new` —
  a concurrent double-delete race gets the same "already deleted" error for free.
- `list()` walks each `<name>/` directory; a name is active iff its highest generation has no
  tombstone, in which case that generation's `create.json` is read for `{name, seq}`.
- A generation's `create.json` is **never** modified or removed by a delete — only a sibling
  `delete.json` is added.

`checkpoint.rs` takes an already-resolved `seq: Option<u64>` rather than a `Conn`; it has no
lbug/database dependency at all. `list`/`delete` are therefore pure filesystem operations that
work even when the database is degraded (they were added to `handlers.rs`'s `exempt_in_degraded`
list); only the handler wrapping `create` touches the database, to resolve `applied_seq`.

## Why not what the spec's Assumptions section suggested

The spec's Assumptions section described "one file per checkpoint name... deletion recorded as a
tombstone marker" — which reads, on first pass, like a flat `<name>.json` plus `<name>.deleted`.
That shape cannot satisfy two requirements simultaneously:

- **FR-007 / FR-006**: a create record must never be rewritten in place, and a name must become
  reusable after its checkpoint is deleted.
- **FR-002 / FR-011 / FR-012**: creation must be atomic and exclusive — `O_EXCL` — so concurrent
  `create` calls for the same name from two processes cannot both succeed.

A flat two-file scheme forces a choice: either reusing a name after deletion means overwriting (or
`O_EXCL`-colliding permanently with) the original `<name>.json`, breaking "never rewritten in
place," or reuse is disallowed, breaking FR-006's explicit reuse-after-delete requirement. The
spec-review comment during Specify (2026-08-08) flagged this directly and required a resolution
before Plan, since it changes the on-disk format and is not something Implement can safely
improvise.

The **generation axis** resolves it: reuse advances to a new generation number rather than
touching the old one, so "never rewrite" and "reusable after delete" are both true at once, and
`O_EXCL` still delivers exactly-once-wins because the *current* generation's create file is the
only thing a racing `create` call ever targets.

### Alternatives considered

- **Single shared append-only JSONL log** (the spec's original FR-002 wording, before revision).
  Rejected: a concurrent check-then-append across two processes reading "is this name already
  active?" then appending a create record is not atomic without separate locking of its own — the
  exact conflict with FR-011 the Specify-stage review identified. This codebase has no existing
  advisory-locking dependency (no `fs2`/`fcntl` in `Cargo.toml`), and one being added just for this
  interacts poorly with a WAL directory shared over a network mount (see NFS caveat below).
- **Flat one-file-per-name with in-place overwrite on reuse.** Rejected: violates FR-007's
  never-rewritten-in-place guarantee directly, and loses the deleted record's own history (when
  was `"pre-migration"` last deleted, and what did it point to before that).
- **Database-resident storage.** Rejected already, at the spec level (#363's superseded design) —
  a checkpoint stored in the database is destroyed by exactly the event it exists to recover from.

## Consequences

- **No new locking, no new crate dependency, no new `AppState` field, no schema/table change.**
  Filesystem exclusive-create delivers the cross-process guarantee for free on a local filesystem.
- **NFS-mounted WAL directories are a known, documented limitation, not a solved case.**
  `create_new`'s exclusivity is a local-filesystem (and most network-filesystem, but historically
  unreliable on NFS specifically) guarantee. This is called out in the tool descriptions and
  operations docs rather than worked around.
- **`.checkpoints/<name>/` is git-friendly by construction** — small, human-readable JSON files
  under a predictable path, satisfying Story 5 (checkpoints travel with the WAL through git) with
  no additional distribution mechanism.
- **Reachability (FR-009)** is a cheap `wal_min_seq`/`wal_max_seq` bounds check, not full coverage
  detection — `wal_min_seq` was added to `wal.rs` as the structural mirror of the pre-existing
  `wal_max_seq` (same sorted-file-list-and-read-one-line cost profile, already accepted at the
  ADR-0026 ~43,820-file production scale). A checkpoint whose seq falls inside `[min, max]` but
  behind a mid-range gap in on-disk WAL files reports `reachable: true` and can still fail at
  restore time — this is a deliberate, documented cost/precision tradeoff, not an oversight; the
  `ToolSpec` description for `knowledge_wal_mark_list` states it explicitly.
- **`#360`** (multi-source hydration with per-source applied positions) can reuse this exact
  `{name, seq}` record shape and store layout for its per-source variant, rather than inventing a
  parallel naming scheme — the generation mechanism generalizes to any "named, mutable-with-history,
  exclusive-create" record, not just single-WAL-directory checkpoints.
