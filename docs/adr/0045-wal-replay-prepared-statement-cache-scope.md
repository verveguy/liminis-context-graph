# ADR-0045: WAL Replay Prepared-Statement Cache — LRU-1 Scope and Deferred Connection Recycling

**Status**: Accepted
**Date**: 2026-07-26
**Issue**: #238

## Context

`WalReplayer::replay_opts` (`crates/core/src/replay.rs`) replays an entire WAL directory over a
single `Conn` (`handlers.rs`'s `handle_rebuild_from_wal`, `recovery.rs`'s WAL-tail-resume path
each create exactly one connection for the whole run). Before this change, `flush_batch` called
`conn.prepare(&batch.template)` unconditionally on **every** flush — including a flush triggered
only by `batch_size` being reached with the same template as the previous flush, since
`ReplayBatch::clear()` unconditionally resets `template`.

`Conn::prepare` (`db.rs`) reaches lbug's `ClientContext::prepare`, which registers the statement
in `CachedPreparedStatementManager` (lbug internal, C++). That manager has **no eviction policy
and no removal API** — its `statementMap` only grows. Dropping the Rust `PreparedStatement`
frees the FFI handle but not the cached parsed statement and logical plan held by the connection.
For a homogeneous WAL (a small number of recurring mutation templates — the common case), this
meant roughly one cache entry accumulated per **batch**, not per distinct template: a 5M-line WAL
at the default `batch_size` of 64 could accumulate ~78k entries, growing RSS monotonically over
the course of a rebuild and risking an OOM kill mid-replay — the worst possible interruption
point, since `handle_rebuild_from_wal` drops the FTS/HNSW indexes before replay starts.

A **template-interleaved** WAL degrades batching to one row per flush (batching is
adjacency-based — a template change flushes the current batch), so this defect was more acute
there: one cache entry per line, not per batch.

## Decision

### A single-entry ("LRU-1") prepared-statement cache, not a multi-template cache

`flush_batch` now takes a `cache: &mut Option<PreparedCache>` parameter, threaded through
`replay_opts` and persisted across all `flush_batch` calls within one run. `PreparedCache` holds
the last-used `(template, use_probe, statement)`. On entry, if the batch's template matches the
cached one, the cached statement is reused directly — no `conn.prepare()` call — and the cache is
otherwise refreshed with the batch's own template after the batch's row loop settles (see below).

This bounds `prepare()` calls to the distinct-template count **only for templates that recur
across consecutive flushes** (FR-002) — exactly User Story 1's homogeneous-WAL case. It does
**not** help User Story 2's pathological, highly-interleaved WAL, where every flush's template
differs from the last: a cache keyed by every distinct template ever seen, instead of just the
most recent, would itself grow without bound whenever the distinct-template count is unbounded
relative to run length — reintroducing the same failure mode this issue exists to fix, just in a
new cache instead of lbug's.

**Rejected alternative: bounded multi-entry LRU (e.g. N=8).** Considered and rejected as
unnecessary complexity for a case the spec doesn't require solving: templates recurring in
separated, non-adjacent runs (as opposed to one contiguous run) are not the shape either
User Story targets, and an LRU-N reintroduces a tuning knob (what's N?) with no clear answer
from the available evidence. LRU-1 is the minimal mechanism that satisfies FR-001/FR-002 exactly
as scoped.

### Settled state, not the initial attempt, is what gets cached

A `MATCH`-prefixed batch may prepare with a `RETURN count(*)` probe appended
(`with_match_count_probe`), then fall back mid-batch to the unprobed template if the probed
statement fails at *execute* time (pre-existing logic, unrelated to this issue). The cache is
populated once, after the row loop completes, with whichever `(template, use_probe, statement)`
the batch ended on — so a flush that started probed but fell back unprobed caches the unprobed
statement, and the next flush of that same template correctly skips straight to unprobed
execution instead of re-attempting a probe already known to fail for this WAL's shape.

### `ReplayStats::prepare_calls` as the observable bound

A new `prepare_calls: u64` field on `ReplayStats`, incremented only at real `conn.prepare()`
call sites (cache misses), makes FR-002's bound directly assertable by a test — see
`prepare_calls_bounded_for_homogeneous_wal` and
`prepare_calls_proportional_to_distinct_templates` in `replay.rs`'s test module. This follows the
existing convention of internal-only diagnostic counters on `ReplayStats` (`seq_regressions`,
`match_prefixed_no_op`) that are not surfaced over IPC/JSON.

### FR-004: periodic connection recycling is evaluated and deferred

For the pathological interleaved case (User Story 2), the LRU-1 cache provides no bound — lbug's
underlying `CachedPreparedStatementManager` can still grow without bound over a sufficiently long
replay of such a WAL. The mitigation this issue's spec asks us to *evaluate* is periodically
recycling the replay connection (closing and reopening it every N flushes or N distinct
templates), which would force lbug to drop its accumulated statement cache along with the old
connection.

**Decision: defer, do not implement.** Reasons:

1. **API shape change required.** `WalReplayer::replay`/`replay_opts` take a caller-owned `&Conn`,
   not a `&Db` it could reconnect from. Adding recycling would require either (a) changing the
   signature to accept `&Db` and thread reconnect logic through both call sites
   (`handlers.rs::handle_rebuild_from_wal`, `recovery.rs`'s WAL-tail-resume path), or (b) some
   other cross-cutting restructuring — a larger change than this issue's scope.
2. **Interacts with future transaction-boundary work.** Transaction boundaries around replay
   batches are explicitly a separate issue in this same four-issue series (see this issue's Out of
   Scope). Recycling the connection mid-replay has implications for any future transaction wrapping
   that work will introduce; deciding the recycling mechanism now, before that work lands, risks
   designing it twice.
3. **Atypical WAL shape.** Per the spec (User Story 2's priority is P2, explicitly "residual
   risk", not the primary defect), real workspace WALs are dominated by a small number of
   recurring mutation templates — the homogeneous case this issue's LRU-1 cache already fully
   addresses. A WAL whose distinct-template count grows without bound over a long replay is not
   a shape observed in production WALs to date.

This decision should be revisited if either (a) transaction-boundary work lands and connection
lifecycle is being touched anyway, or (b) a real-world interleaved WAL is observed to cause
memory growth in practice — at which point periodic recycling (or an alternative such as an
explicit lbug-side cache-clear API, should one become available upstream) should be reconsidered
against the API-shape cost noted above.

## Consequences

- Homogeneous and mostly-homogeneous WALs (the common case) no longer accumulate one
  prepared-statement cache entry per batch; the LRU-1 cache bounds this to one entry per distinct
  template *encountered since the last template change*, which in practice bounds lbug's internal
  cache growth to the distinct-template count for the whole run.
- Pathological, highly-interleaved WALs retain the unbounded-growth risk this ADR documents; no
  code change in this issue mitigates that case. It remains a known, accepted limitation until
  revisited per the trigger conditions above.
- No observable behavioral change to replay results (FR-003): `ReplayStats` counters other than
  the new `prepare_calls` are unaffected, and the set of mutations applied is identical.

## References

- Issue #238 (this ADR)
- Issue #237 (prior issue in the same WAL-replay-audit series; ADR-0043)
- `crates/core/src/replay.rs`: `PreparedCache`, `flush_batch`, `ReplayStats::prepare_calls`
- `crates/core/src/db.rs`: `Conn::prepare`
