# ADR-0363: Named WAL Checkpoints Stored Outside the Database

**Status**: Accepted
**Date**: 2026-08-08
**Issue**: #363
**Relates to**: ADR-0353 (`applied_seq`/`WalPosition`), ADR-0026 (episode-cursor WAL resume), ADR-0028 (`knowledge_dump_wal` compaction), ADR-0035 (MCP-over-stdio transport, `ToolSpec` registry)

## Context

Bounded WAL replay (#362) lets `knowledge_rebuild_from_wal` stop at an inclusive `to_seq`, making
it possible to restore a graph to an earlier position. It does not answer the harder question: *which*
seq was the graph last known-good at? An operator recovering from a bad mutation otherwise has to
correlate timestamps, read WAL lines by hand, or bisect with repeated bounded rebuilds. At the
scale ADR-0026 documents (43,821 WAL files, seq 4,641,989), none of those are realistic.

`applied_seq` (ADR-0353) already persists a WAL position, but it answers a different question. It
is *derived, not chosen* — it advances automatically after every commit. It is *singleton* — a
`MERGE ... SET` overwrite means there is no history, only "where the database is right now." And
it is *positional, not semantic* — it records where the database is, not that any particular
position was ever declared known-good. A checkpoint is not a new kind of measurement; it is an
operator-chosen label attached to a position `applied_seq` already produces.

### Storage must live outside the database

The obvious first instinct — a second table alongside `WalPosition`, extending ADR-0353's already-
documented schema-parity divergence — was proposed early and explicitly withdrawn. A checkpoint
stored in the database is destroyed by exactly the event it exists to recover from: after a
database wipe or corruption, the operator holds the WAL and nothing else, and the checkpoint list —
the one artifact that names which position to restore to — would be gone with the database. This
also contradicts this project's own model of the database as a derived cache: nothing required to
rebuild that cache can live inside it.

This is the load-bearing distinction from `applied_seq`. `applied_seq` describes *this database's*
position and is meaningless without the database. A checkpoint describes *the WAL stream* and must
outlive any database built from it.

### Placement within the WAL directory must not collide with WAL scanning

Given checkpoint storage must live in the WAL directory (there is nowhere else it could live and
still survive a database wipe), the specific placement is constrained by existing code. Three
call sites discover WAL files by a non-recursive `fs::read_dir` filtered to a `.jsonl` extension:

- `wal.rs`'s `count_jsonl_files` (WAL admin file counting)
- `wal.rs`'s `scan_max_seq` (the source of `WalWriter`'s `global_seq` derivation)
- `replay.rs`'s file collection inside `WalReplayer::replay`

A checkpoint store placed directly in `wal_dir` with a `.jsonl` extension — the obvious first
attempt, since that's the existing WAL file convention — would be silently replayed as mutations
by the third and folded into sequence allocation by the second. This is a "silent until it
happens" corruption class with no compiler or test-suite guard against it. Because all three scans
are non-recursive, a subdirectory is invisible to every one of them with zero change to their
logic — so the fix is purely a placement decision, not a code change to the existing scanners.

## Decision

### One JSON file per checkpoint under `<wal_dir>/.checkpoints/`

Each checkpoint is `<wal_dir>/.checkpoints/<name>.json`, containing `{name, seq, created_at, note}`.
`checkpoint::checkpoints_dir()` is the single source of truth for this path — never `.jsonl`-suffixed,
never `wal_dir` itself.

**One file per checkpoint, not a single append-only log with tombstone deletes** (the shape
originally suggested when this storage decision was first proposed). The per-file layout gets two
correctness properties for free from the filesystem, without inventing a new locking primitive
this codebase has no precedent for (`WalWriter` assumes a single writer per process; a checkpoint
store may be touched by both a producer process and a local operator/reader sharing the same WAL
directory):

- **Duplicate-name detection (FR-002)**: `OpenOptions::new().write(true).create_new(true)` is an
  atomic, kernel-enforced "fail if exists." A log-based design would need to replay the whole log
  (or maintain a separate index) just to check whether a name is already taken.
- **Delete-of-missing-name detection (FR-006)**: `fs::remove_file` naturally returns `NotFound` —
  mapped directly to the required error.

An ever-growing single log file would also sit awkwardly against FR-010's unbounded-retention
requirement (no automatic eviction) — N small files age independently; a log grows without bound
and would eventually need its own compaction story nobody has asked for.

### Reachability is a bounds check, not a full existence scan

`knowledge_checkpoint_list` (FR-007) reports whether each checkpoint's `seq` is still reachable —
whether the WAL content currently available for replay still covers that position — via a new
`wal_min_seq` (symmetric to the existing `wal_max_seq`, built on the previously-private
`replay::first_seq_in_file`) giving `[min, max]`; a checkpoint is `reachable` iff
`min <= seq <= max`.

This is deliberately an approximation, not a guarantee. `wal_max_seq`'s own doc comment already
establishes "safe to call on every `knowledge_status` request" as the cost bar for any per-call WAL
directory scan at this codebase's ~43,820-file production scale; a full existence scan (walking
every file to confirm the exact seq is present, not merely within the observed range) would break
that discipline. The accepted blind spot: a *gap* inside `[min, max]` — a specific file manually
removed while its neighbors remain — is misreported as reachable. A checkpoint genuinely outside
the WAL's current range (the common real-world case: a partial or manual WAL directory swap) is
always correctly reported unreachable.

### `create` is a pure filesystem write plus one DB read; `list`/`delete` never touch the DB

`knowledge_checkpoint_create` reads `Conn::get_applied_seq()` once (already gated by the existing
degraded-mode guard, and required to fail per FR-008 when `applied_seq` is `null`), then writes the
checkpoint file. It does not take `state.write_lock` and does not touch `state.wal_writer` — it
mutates neither the graph nor the live WAL, so the filesystem's own atomicity is sufficient and
taking the write lock would only slow down unrelated ingest for no correctness gain.

`knowledge_checkpoint_list`/`knowledge_checkpoint_delete` touch only `wal_dir` and are added to
`handlers.rs`'s `exempt_in_degraded` list, alongside `health_check`/`knowledge_status`/
`knowledge_recover*`/`knowledge_close`. This lets an operator find the right recovery seq *while
the database itself is degraded* — precisely the scenario this feature exists for. `create`
correctly stays gated: without a live `applied_seq` there is nothing meaningful to label.

### Name safety

`name` is used directly as a filename component (`<wal_dir>/.checkpoints/<name>.json`), so it is
validated against `^[A-Za-z0-9._-]{1,255}$` (also rejecting literal `.`/`..`) before touching the
filesystem. Without this, a name containing `/` or `..` could escape `.checkpoints/` entirely.
This isn't called out explicitly in the spec but is a load-bearing correctness/security property
of the chosen storage shape, not an optional hardening pass.

## Consequences

- **New public API surface**: `knowledge_checkpoint_create`/`knowledge_checkpoint_list`/
  `knowledge_checkpoint_delete`, all `admin`-scope, plus corresponding `ToolSpec` entries in
  `crates/service/src/mcp/tools.rs` (registry 34→37, admin bucket 7→10).
- **No new DB schema.** Unlike ADR-0353's `WalPosition` table, this feature adds zero columns and
  zero tables — the entire storage surface is `.checkpoints/*.json` files. Anyone looking for a
  checkpoint's data in the database will not find it there; that absence is the point.
- **Git-friendly by construction.** `.lcg/wal/` is already documented as safe to check into git
  alongside the WAL itself; a checkpoint file is small, human-readable JSON with the same
  properties. Whether checkpoint files should actually be published/distributed via the
  orac/zen model is explicitly out of scope for this issue (see the spec's Out of Scope section)
  — this ADR only records that the local storage choice does not foreclose it.
- **No new locking primitive.** The per-file/`create_new`/`remove_file` design was chosen
  specifically to avoid needing one. A future design that consolidates checkpoints into a single
  file (e.g. for a distribution format) would need to solve concurrent-writer safety that this
  design gets from the filesystem for free.
- **Restoring to a checkpoint is not made faster by this feature.** A checkpoint only supplies the
  `to_seq` value; the rebuild it feeds is the same full bounded replay `knowledge_rebuild_from_wal`
  already performs (ADR-0026 measures ~7h for a full production replay). Making that replay fast
  (e.g. via a true point-in-time snapshot mechanism) is a distinct, not-yet-tracked concern.
- **Reachability's bounds-check blind spot is accepted, not silently swept under the rug.** An
  internal gap (one file removed, neighbors intact) can be misreported as reachable. This is
  documented on `checkpoint::list`'s doc comment and here; closing it would require a full
  existence scan, which would violate the O(files) cost discipline this feature otherwise shares
  with `wal_max_seq`.
- **A crash between `create_new()` succeeding and the write completing** can leave a
  truncated/empty checkpoint file. `list`'s parse-skip behavior (matching the existing
  corruption-tolerance precedent in `wal::read_last_seq`/`replay::first_seq_in_file`) means such a
  file is simply absent from `list` rather than corrupting the call — an accepted, documented risk
  with the same profile as other single-shot file writes in this codebase.
- **`knowledge_dump_wal` compaction is compatible but not checkpoint-aware.** Dump/compaction
  writes to a separate target directory and never touches the live `wal_dir` (ADR-0028), so running
  it does not itself invalidate a checkpoint. But a checkpoint's `seq` is only meaningful against
  the WAL numbering it was created under — adopting a dumped (renumbered) WAL as the new live
  directory silently strands any checkpoints created against the old numbering. This feature does
  not attempt to detect or migrate that case.
- **Cross-repo**: as with prior `knowledge_*` additions (see ADR-0028's precedent), the Electron
  app's `service_protocol.py` will need a corresponding update to call these three new methods —
  out of scope for this repo, tracked in the PR description for a human to follow up on.
