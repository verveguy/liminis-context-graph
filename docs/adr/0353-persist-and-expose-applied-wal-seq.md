# ADR-0353: Persist and Expose an Applied WAL Sequence in `knowledge_status`

**Status**: Accepted
**Date**: 2026-08-05
**Issue**: #353 (community report #351)
**Relates to**: ADR-0026 (episode-cursor WAL resume), ADR-0009 (degraded-mode startup & recovery), ADR-0025 (auto-heal index build)

## Context

The orac/zen deployment model (#351) treats a distributed, git-published JSONL WAL as
authoritative: every node — read-only zen consumers and the ingesting master node alike —
rebuilds its local LadybugDB from that WAL on boot. On startup a node needs to answer cheaply
and reliably: *does my local DB already reflect this exact WAL, or do I need to replay?* The
common case (a container restart with no WAL change) must be a cheap no-op; the divergent cases
(failover advanced the WAL, a backup restored behind it, a corpus reset) must self-heal.

`knowledge_status`'s `wal` object exposed only `exists`/`file_count`/`byte_size` — nothing tying
DB content to a WAL position. `knowledge_rebuild_from_wal` computes exactly the needed value
(`ReplayStats::last_committed_seq`) but discards it once the call returns. The downstream
workaround — hashing WAL file contents to detect change — is unreliable: lcg writes compact
`serde_json`, but the distributed copy is re-serialized by a Python publisher with spaced
formatting, so the bytes differ while the semantics are identical, forcing a full rebuild on
every boot even when nothing changed.

This work was blocked on #352 (the `global_seq` re-derivation fix): the whole design rests on
WAL `seq` being unique and monotonic, and a WAL populated after process start could otherwise
receive duplicate seqs, making both `applied_seq` and `max_seq` ambiguous. That fix landed first.

### Relationship to ADR-0026 — a deliberate divergence, not a reinvention

ADR-0026 already considered persisting a last-applied seq in a metadata row, for crash recovery,
and rejected it in favor of the episode cursor (read the last `Episodic` node, find its uuid in
the WAL, take that line's seq) — because the cursor is crash-proof and **retroactive**: it works
on databases that predate any cursor mechanism.

This issue diverges deliberately, because it is solving a different problem:

- **ADR-0026** is crash recovery — a one-off WAL scan on the recovery path is acceptable, and
  happens rarely.
- **This issue** is an **O(1) boot check** that must not scan the WAL on every `knowledge_status`
  call — ADR-0026 itself documents a production WAL directory with **~43,820 files**, where a
  scan on every status poll would be slower than the byte-hashing heuristic this feature
  replaces.

The two mechanisms are complementary, not competing: **persist a cursor for the fast path** (this
ADR), and **derive one via ADR-0026's episode-cursor mechanism when the persisted record is
absent** (the backfill case below, reusing the mechanism rather than inventing a second one).

### The gap #351's own proposal didn't cover: upgrade semantics

#351 specified `applied_seq` as "integer, or `null` when the graph is empty / nothing applied."
That conflates two different states:

| State | Naive `applied_seq` | `entity_count` | Correct action |
|---|---|---|---|
| Fresh/cleared DB | `null` | `0` | nothing to do (or full rebuild if the WAL is non-empty) |
| Upgraded pre-existing DB | `null` | `> 0` | **unknown position — full rebuild required** |

A DB populated before this feature existed has content but no recorded position. Reporting
`null` for that is indistinguishable from "empty," which every existing deployment would hit
exactly once, on upgrade. A sentinel value for "unknown position on a populated DB" was
considered and rejected — it pushes a correctness obligation onto every consumer, forever, to
handle a state the service can resolve for itself once. Instead: **derive the position from
graph content on first open**, using ADR-0026's already-validated retroactive mechanism,
collapsing "upgraded, unknown" into "known position."

### Schema-parity constraint

`schema.rs` tracks parity with graphiti's `kuzu_driver.py` (the canonical source of truth for
node/rel tables), and had no metadata/singleton table before this feature — only `Entity`,
`Episodic`, `RelatesToNode_`, `Community`, `Saga`, and the rel tables. graphiti has no equivalent
of an applied-WAL-position record (it doesn't itself track one), so the new `WalPosition` table
is a **deliberate, recorded divergence** from that parity rule, not an oversight.

## Decision

### Storage: a singleton `WalPosition` node table, row-absence means "unknown"

```cypher
CREATE NODE TABLE IF NOT EXISTS WalPosition (id STRING PRIMARY KEY, applied_seq INT64)
```

created idempotently in `schema.rs` alongside the existing tables (every normal startup calls
`init_schema`, not just a fresh DB, so table creation must be `IF NOT EXISTS`-safe). A single row
`id: 'singleton'` holds the current position.

**Row-absence, not a nullable column on an always-present row, represents "unknown."** This
collapses two states that would otherwise need separate representations — "never written" and
"backfill failed" — into one, and makes the reset case (below) trivial: an explicit write of `0`,
not a delete.

Reads/writes go through two new `Conn` methods, `get_applied_seq()`/`set_applied_seq(seq)`, that
call `Connection::query` directly rather than `raw_query`/`exec_params` — the same
non-recording bypass `count_nodes`/`exec_transaction_control` already use. This is load-bearing:
`raw_query`/`exec_params` record into `executed_mutations`, which WAL-flush helpers drain and
log. Using either for the metadata write would make `applied_seq` immediately stale by the write
that just recorded it — a self-referential regress where recording the position invalidates the
position just recorded.

### Advancing `applied_seq`: write-after-commit, not atomic-with-commit (FR-003)

**Crash safety requires `applied_seq` never exceed what is actually committed.** The ideal
mechanism would update it atomically with the chunk's graph commit. That mechanism isn't
available here: `episode::add_episode`'s Phase C mutations (`insert_entity`, `insert_episodic`,
`insert_mentions_edge`, `insert_relates_to_edge`) auto-commit individually against lbug — there
is no explicit `BEGIN TRANSACTION` wrapping them. This codebase reserves explicit transactions
for WAL replay's `flush_batch` only (`db.rs`'s `lbug_transaction_semantics_pinning_tests`
documents that lbug rolls back the *entire* transaction on any statement exception, which is why
replay uses one and live ingest doesn't — a mid-batch failure during live ingest should not roll
back entities/edges that already committed successfully).

So the write happens **strictly after** both the graph commit and the WAL flush that assigns the
chunk's seq: `wal_exec::wal_flush_chunk` now returns the max seq it assigned
(`Option<u64>`, `None` if nothing was actually written), and `episode.rs`'s call site writes it
via `conn.set_applied_seq(seq)` immediately after, non-fatally.

This gives exactly the safety direction FR-003 requires: a crash between the WAL flush and the
`set_applied_seq` write leaves `applied_seq` **trailing** the actually-committed position — a
resume redoes a little work, which is recoverable — rather than **leading** it, which would
silently skip committed-but-unrecorded mutations. Tested deterministically in
`wal_exec::tests::skipped_set_applied_seq_write_leaves_applied_seq_trailing_not_leading` by
committing a chunk, flushing it to WAL, and simply not calling `set_applied_seq` — the same state
a `kill -9` between those two steps would leave, without needing an actual process kill in CI.

The same write lands at every other point that commits a batch of mutations and knows its
precise resulting seq: `knowledge_rebuild_from_wal`'s two non-dry-run call sites (streaming and
background-job), `Db::open_or_rebuild`'s replay branch, and — a deliberate extension beyond
FR-004's literal text — `recovery::run_full_recovery_sequence`'s completion (autonomous
WAL-corruption self-heal produces an equally-precise `ReplayStats` via the same replay call;
skipping the extension would leave `applied_seq` at `null` immediately after every self-heal even
though a better value was just computed).

`wal_flush_ungrouped` (delete/corrections/`knowledge_query_cypher`) is deliberately **not**
wired up — no FR requires it, and the resulting lag is the same explicitly-safe "trailing" case.

### `max_seq`: unit-aligned with `applied_seq`, and cheap enough to call on every status request

`WalWriter`'s existing `scan_max_seq` returns `highest_seq_value + 1` — "next assignable seq,"
used only to seed `global_seq` at startup/resync. `applied_seq` is a literal seq value (e.g.
`41`). Wiring `wal.max_seq` straight to `scan_max_seq()`'s raw return value would put the two
fields permanently one apart, so "caught up" (`applied_seq == max_seq`) could never be true. A
new `wal::wal_max_seq(wal_dir)` wraps `scan_max_seq()` and subtracts 1 (`None` when the WAL is
empty), reporting the literal highest seq present — the same units as `applied_seq`.

`max_seq` is read **fresh on every `knowledge_status` call**, not cached — caching would
silently miss exactly the external-WAL-write case #351 exists to detect (another process
publishing new WAL content between status calls). At the ~43,820-file scale ADR-0026 documents,
a naive full-file read per file on every status poll would risk being slower than the
byte-hashing heuristic this feature replaces. `read_last_seq` (the per-file worker) now seeks
from EOF and reads a bounded 256 KiB tail window — generous even for a line carrying a large
embedding vector — falling back to a full read only when no complete line is found in that
window (an oversized single line, or a window landing entirely inside a truncated final write).
This preserves the existing "tolerates truncated final lines" guarantee, since truncation can
only affect the *end* of a file and both paths always read through to EOF.

A further optimization (trusting file mtime/name ordering to scan only recent files, rather than
every file) is deferred to a future issue if the tail-read alone proves insufficient at more
extreme scale — not attempted here, since it touches an explicitly untrusted-ordering assumption
`scan_max_seq` documents.

### Backfill on first open (FR-007/FR-008)

`recovery::backfill_applied_seq_if_absent(conn, wal_dir)`:

1. No-op if a position is already recorded (every boot after the first).
2. If `Episodic` count is `0` → write `0` directly, **without touching the WAL directory at
   all**. This is what distinguishes "genuinely fresh" from "populated but unknown" — the
   ambiguity #351's original proposal didn't resolve.
3. Otherwise, call ADR-0026's `derive_episode_cursor`: `CursorReason::UuidMatch` → write
   `Some(seq)` (the conservative, `<=`-true-position value ADR-0026 already validated as safe
   to re-apply — MERGEs are no-ops, create-form statements collide harmlessly);
   `CursorReason::UuidNotFound` → leave the row absent (`null`). `CursorReason::NoEpisodes` is
   unreachable here, since step 2's guard already ran.

Called at every DB-open path that doesn't already know a precise position from a fresh replay:
`main.rs`'s startup sequence (after schema init and index build, before the socket accepts
requests) and `Db::open_or_rebuild`'s non-replay branch. Non-fatal everywhere — a missed backfill
just leaves `knowledge_status` reporting `null`, the same safe "unknown, full rebuild" signal a
genuine backfill failure produces.

### Reset on clear (FR-005)

`handle_clear_all` and `clear_db_for_rebuild` (the two paths that always operate on a
just-recreated, empty DB file) each call `conn.set_applied_seq(0)` immediately after
`init_schema`. `0` is correct here, not `null`: the position is *known* (nothing has been
applied to this fresh DB), which also skips an unnecessary backfill scan the next time the DB is
opened.

### The `null` / `0` / integer contract (FR-008)

Three values, three distinct meanings — not points on a single number line:

- **`null`** — unknown position. Reserved for genuine backfill failure only (no episodes, or the
  last episode's uuid not found in the WAL). The documented action is always a full rebuild, the
  same fallback ADR-0026 already defines for its own recovery path.
- **`0`** — known position: nothing applied yet.
- **positive integer** — known, applied WAL position.

This is documented explicitly in `docs/operations.md` rather than left to be inferred from type,
because the failure mode is language-specific: `null` breaks arithmetic in Rust and Python
(comparing `None`/`null` numerically is a type error), which tends to surface a client bug
immediately — but JavaScript coerces `null < 5` to `true`, so a naive port of the "if behind,
resume" comparison would silently take the *incremental resume* branch on an *unknown* position,
skipping the full rebuild that state actually calls for.

## Consequences

- **New public API surface, additive only**: `knowledge_status`'s `wal` object gains
  `applied_seq`/`max_seq`; every existing field is unchanged. No new `knowledge_*` dispatch
  method, so no `ToolSpec` registry change.
- **New persisted schema, and a documented graphiti-parity divergence.** `WalPosition` is the
  first metadata/singleton table in `schema.rs`; anyone diffing against `kuzu_driver.py` for
  parity should expect this one intentional gap.
- **No new `AppState`/`Db` struct fields.** All state lives in DB rows, read fresh on every
  `knowledge_status` call — satisfying "must survive restart, not memoisation" without the
  constructor-sweep risk a new field would otherwise carry across every hand-built `AppState`
  test fixture in this codebase.
- **`run_full_recovery_sequence` writes `applied_seq` beyond FR-004's literal text.** Flagged
  explicitly during Plan/Review as a deliberate, documented judgment call: the autonomous
  self-heal path produces an equally-precise `ReplayStats` via the same replay mechanism
  `knowledge_rebuild_from_wal` uses, so skipping it would leave `applied_seq` stale for no
  reason.
- **`wal_flush_ungrouped` paths (delete/corrections/raw-cypher) are not wired up.** `applied_seq`
  can lag further behind reality after those operations than after an ingest chunk. This is the
  explicitly-safe "trailing" direction FR-003 already accepts, not a gap — extending it is
  unnecessary surface area for this issue and can be added later if the lag proves to matter in
  practice.
- **Cross-repo**: the Python-side `service_protocol.py`/`graphiti_service.py` consumer (the
  liminis app) and the orac/zen deployment described in #351 are downstream consumers of this
  response shape — out of scope for this repo's change, but the entire motivation for it.
