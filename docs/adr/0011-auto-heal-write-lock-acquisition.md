# ADR-0011: Auto-Heal Write-Lock Acquisition from Search Handlers

**Status:** Accepted  
**Date:** 2026-05-24  
**Relates to:** ADR-0002 (reader/writer lock split), issue #58

## Context

ADR-0002 designates `handle_find_entities`, `handle_find_relationships`, and
`handle_search_passages` as **lock-free hot paths** — they invoke the search
functions without acquiring `write_lock` so that concurrent episode ingestion
never blocks reads.

When a LadybugDB database is opened fresh (either brand-new or after
`knowledge_clear_all`), HNSW vector indexes do not yet exist. The schema
initialisation (`init_schema`) creates tables and FTS indexes, but HNSW
creation is deferred because HNSW indexes block in-place writes. Under the
original design, the caller was expected to call `knowledge_build_indices` after
bulk ingestion.

In the steady-state incremental workflow, chunks trickle in continuously and
users query at any time. No caller reliably invokes `knowledge_build_indices`
before the first search. The result is a cryptic LadybugDB binder error:
`Binder exception: Table Entity doesn't have an index with name entity_name_embedding_idx`.

## Decision

On the first search call that encounters a missing-index binder error, the
affected search handler:

1. Detects the error by matching `"Binder exception:"` and `"doesn't have an
   index with name"` in the error string (no typed variant is exposed by lbug).
2. **Acquires `write_lock.write()` from within the handler** — a one-time
   exception to the lock-free contract stated in ADR-0002.
3. Calls `build_indices_and_constraints` inside `spawn_blocking`.
4. Releases the write lock.
5. Sets `AppState.indices_built = true` (an `Arc<AtomicBool>`).
6. Retries the original search and returns the result.

Subsequent searches check `indices_built` before entering the recovery path
(FR-003): once the flag is `true`, the handler skips the auto-build and
returns a human-readable error if a missing-index error somehow recurs.

The flag is reset to `false` in `handle_clear_all` after the database swap
(per ADR-0003) so that the first post-clear search self-heals again.

Any missing-index binder error that would surface to the caller is always
rewritten to `"Knowledge graph indices not yet built. Call
knowledge_build_indices to resolve."` — the raw `Binder exception:` trace is
never exposed.

## Rationale

**Why acquire the write lock from a read handler?**  
HNSW index creation is a write transaction in LadybugDB. It must be serialised
with concurrent episode ingestion to avoid corruption. Acquiring `write_lock`
is the correct mechanism for serialisation and is consistent with ADR-0002's
intent (writes use the lock; the ADR's "lock-free" designation is for the
normal search path, not an unconditional constraint).

**Why a per-session flag rather than a DB-level check?**  
A DB-level check would require a round-trip to LadybugDB on every search to
determine whether the index exists. The flag pays the cost at most once per
session per DB lifecycle event (startup or `clear_all`), which is acceptable
given NFR-001 (the one-time penalty is excluded from the p95 ≤ 500 ms budget).

**Why string matching rather than a typed error?**  
LadybugDB 0.16.1 exposes only `lbug::Error::FailedQuery(String)` for query
failures. No typed binder-error variant is available. The lbug version is
pinned at `=0.16.1`; a version bump must re-verify this string.

## Consequences

- The lock-free guarantee from ADR-0002 holds for all steady-state searches
  (after the first successful build). The one-time write-lock acquisition is
  bounded to a single event per DB lifecycle.
- `build_indices_and_constraints` must remain idempotent (already true: both
  `create_vector_indexes` and `create_fts_indexes` suppress index-already-exists
  errors).
- Future maintainers upgrading lbug must verify the binder error string format
  has not changed.
- The `indices_built` flag in `AppState` must be reset any time the database is
  replaced with a fresh empty copy (currently only `handle_clear_all`).
