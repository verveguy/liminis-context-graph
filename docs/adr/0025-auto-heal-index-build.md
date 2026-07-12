# ADR-0025: Auto-Heal Index Build and Bulk-Load Reload Pattern

**Status**: Accepted
**Date**: 2026-06-17
**Issues**: #146 (bulk-load reload pattern); #58 (original auto-heal)

## Context

liminis-context-graph uses lbug (Kuzu) as its graph database. lbug requires separate DDL calls to create
FTS (BM25) and HNSW vector indexes after table creation — they are not automatic. Two problems
arose:

1. **Missing vector index on fresh open**: `schema::init` created tables and FTS indexes but not
   HNSW vector indexes, so the first search after startup failed with a binder exception.
2. **FTS throughput degradation during WAL reload**: FTS indexes created by `init_schema` were
   maintained inline as each mutation was replayed, causing per-mutation cost to grow with the
   FTS index size. On a 12k Entity-create workload, throughput degraded ~1.85× between the first
   and second 6k halves.

## Decisions

### Auto-heal search path (from #58)

`handle_find_entities`, `handle_find_relationships`, and `handle_search_facts` catch lbug binder
exceptions on index lookups and, if `indices_built` is false, call `build_indices_once` before
retrying. This repairs a missing-index state transparently on the first search.

**`is_missing_index_error`**: Matches lbug 0.17 binder exceptions for missing indexes. The error
text for both HNSW and FTS missing indexes has the same shape (confirmed empirically in #146):
```
Binder exception: Table <T> doesn't have an index with name <index_name>.
```
The matcher checks `s.contains("Binder exception:") && s.contains("doesn't have an index with name")`.
This matches both:
- HNSW: `"Binder exception: Table Entity doesn't have an index with name entity_name_embedding_idx"`
- FTS: `"Binder exception: Table Entity doesn't have an index with name node_name_and_summary"`

No extension to `is_missing_index_error` was required in #146.

**`build_indices_once`**: Holds the write lock to prevent concurrent builds, double-checks
`indices_built` inside the lock (DCLP pattern), calls `Conn::build_indices_and_constraints`
(creates HNSW + FTS), then sets `indices_built = true` while still holding the lock.

### Bulk-load reload pattern (from #146)

`handle_rebuild_from_wal` now owns the index DDL lifecycle for WAL reload:

1. **Drop FTS before replay** — `schema::drop_fts_indexes` drops all 3 FTS indexes before the
   `WalReplayer` runs. This eliminates inline FTS maintenance during replay, producing flat
   throughput regardless of graph size.
2. **Bulk replay** — `WalReplayer::replay_opts` runs with no FTS present. HNSW indexes are also
   absent during reload (they were never created by `init_schema`).
3. **Build all indexes once after replay** — `Conn::build_indices_and_constraints` rebuilds both
   FTS and HNSW over the fully-loaded dataset. This is more efficient than per-insert maintenance.
4. **Set `indices_built = true`** — after a successful build, the flag is set so subsequent
   searches skip the auto-heal path.

This applies to both the streaming path (progress_tx present) and the background job path.

**Build always runs after replay returns** (not skipped on cancel). The spec's "may be skipped
when cancelled" is permissive; building on a partially-loaded graph is better than leaving it
unindexed. The auto-heal path covers the gap if the build fails (non-fatal).

**Build failure is non-fatal** — a failure logs an `eprintln!` and leaves `indices_built = false`.
The auto-heal path rebuilds on the first search. Propagating would report a successful
2.5M-mutation replay as failed, which is wrong.

**Index DDL lifecycle lives in the handler, not `WalReplayer`** — `WalReplayer` remains a pure
mutation executor. Drop and build are orchestration concerns and belong in `handle_rebuild_from_wal`.

### Interrupted-reload self-heal (FR-005)

If the service is killed after the FTS drop but before the end-of-reload build, indexes are
absent on restart. The auto-heal path in the search handlers recovers transparently:

1. FTS missing → `is_missing_index_error` matches (same Binder exception shape as HNSW)
2. `indices_built` is false (reset on `clear_all`; not set by incomplete reload)
3. `build_indices_once` runs → rebuilds both FTS and HNSW
4. Flag set → subsequent searches skip auto-heal

No manual intervention required.

## Consequences

- Reload throughput is flat in graph size (measured ~1.02× half2/half1 vs ~1.85× without the
  drop-before pattern).
- Post-reload searches are immediately available — no lazy-build stall on the first query.
- FTS is temporarily absent during reload; search is unavailable mid-reload (acceptable).
- `schema::drop_fts_indexes` is idempotent — safe to call repeatedly or when indexes are absent.

## Related

- ADR-0024: bound-parameter DB access (supersedes the escaping layer, same era as #143 refactor
  that removed the prior drop-before-replay implementation).
- `auto_heal_index_integration.rs`: integration tests for the auto-heal path.
- `handlers_wal_admin.rs` tests `test_reload_builds_all_indexes` and
  `test_interrupted_reload_auto_heals`: integration tests for the bulk-load lifecycle.
