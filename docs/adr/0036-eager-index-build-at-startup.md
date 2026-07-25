# ADR-0036: Eager HNSW/FTS Index Build at Startup + Dedup-Path Auto-Heal

**Status**: Accepted
**Date**: 2026-07-24
**Issues**: #208 (this fix); reported in #203, field context in discussion #207; extends ADR-0025;
closes ADR-0034 §5's deferred gap

## Context

HNSW vector indices (`entity_name_embedding_idx` on `Entity`, plus the `Episodic` and
`RelatesToNode_` equivalents) were built **lazily**: only by the search handlers' missing-index
auto-heal path (`handlers.rs`, ADR-0025) or an explicit `knowledge_build_indices` call. Startup
(`main.rs`) ran `init_schema` only, which intentionally does not build HNSW indices.

Ingest's Phase B dedup (`episode.rs`) selects between two strategies by entity count in the
group: `brute_force_similar_entity` below the hybrid-dedup threshold (1,000 by default), and
`hybrid_dedup_similar_entity` at or above it. The hybrid path issues a
`QUERY_VECTOR_INDEX(entity_name_embedding_idx)` query directly and propagated any error with a
bare `?` — unlike the search handlers, it had no missing-index detection or auto-heal.

On a workspace where nothing had ever called a search handler or `knowledge_build_indices`, the
index was never built. As soon as any `group_id` crossed the 1,000-entity threshold, every
subsequent ingested chunk failed its dedup query with `Binder exception: Table Entity doesn't
have an index with name entity_name_embedding_idx`, and `knowledge_status` correctly reported
`indices_built: false` — the index genuinely never existed. A single sustained writer often
escaped this by accident (an earlier search call triggered the lazy build first), which is why
the failure surfaced specifically under multi-writer field usage (#207: 5 parallel writers,
213/269 chunks failed) rather than in typical single-writer testing. Phase A (extraction +
embedding, real LLM spend) runs before the failing Phase B query, so every chunk that failed this
way was billed for discarded work.

Separately, ADR-0034 §5 flagged that `recovery.rs`'s WAL-corruption self-recovery path
(`run_full_recovery_sequence`) already calls `build_indices_and_constraints()` on success but
never set `AppState.indices_built = true` to reflect it — `knowledge_status` under-reported
readiness immediately after a successful auto-recovery, "flagged here as a candidate follow-up,
not fixed in this change." This issue is that follow-up.

## Decisions

### 1. Indices are built eagerly at startup, on both the direct-open and post-recovery paths

`bootstrap_app_state` (`main.rs`) now calls `conn.build_indices_and_constraints()` immediately
after `conn.init_schema(embedding_dim)` on the direct-open path, before the socket accept loop
starts — the socket is bound before `bootstrap_app_state` runs, but no request can be processed
until it returns, so this satisfies "before the socket accepts requests" without a separate
readiness flag. The call runs inline (not `spawn_blocking`), matching the existing un-wrapped
`init_schema` call immediately above it — there is no concurrent request to block at this point
in startup.

On the recovery path, `run_full_recovery_sequence` already performed this build (pre-existing,
ADR-0034 §5's flagged case); no new call was needed there.

Both paths now track whether the resulting DB is actually indexed (`indices_ready: bool`, true on
direct-open success and on recovery success, false on degraded/no-DB) and store it into
`AppState.indices_built` after construction — `state.indices_built.store(indices_ready, ...)` —
rather than the previous hardcoded `false`. This is a local-variable, "store after the fact"
change, not a new `AppState::from_env` parameter, keeping the blast radius to `main.rs` (the one
production call site; `mcp/server.rs`'s `from_env` call is test-only and unaffected).

The build is idempotent and cheap when indices already exist: `create_vector_indexes` already
swallows "already exists" errors (ADR-0034) and propagates any genuine failure. A genuine,
non-"already exists" failure during the eager startup build now fails startup — this is a **new**
failure mode for the direct-open path (previously `init_schema`-only startup could not fail this
way) and is this issue's explicit, accepted tradeoff: a real inability to build indices should not
be silently masked as a successful, but effectively broken, startup.

### 2. Ingest's hybrid dedup path gets the same missing-index auto-heal as the search handlers

Phase B's per-entity-name resolution loop (previously an inline `spawn_blocking` closure) is
factored into an async `resolve_phase_b` helper. `add_episode` calls it once; on a missing-index
error (`is_missing_index_error`), if `indices_built` is not already `true` it calls the shared
`build_indices_once`, reloads the DB (in case a concurrent `clear_all` swapped it), and retries
the whole batch once. A second consecutive missing-index error, or `indices_built` already being
`true` when the first error hits, maps to `MISSING_INDEX_USER_MSG` rather than retrying —
identical in shape to the search handlers' existing pattern (ADR-0025), including that same
"already true but still fails" quirk (no redundant rebuild attempt).

Because the whole batch runs inside one `spawn_blocking` closure with no `.await` inside, and
`build_indices_once` is async and lock-acquiring, the retry re-runs the **entire** Phase B batch
rather than resuming mid-batch. This redoes already-successful name-match/dedup lookups for
entities before the failing one, bounded by one chunk's entity count (typically small) — not
worth a partial-resume design for that cost.

Non-missing-index errors from the hybrid query propagate immediately via the existing `match`'s
fallthrough arm, untouched by the retry logic — auto-heal only ever fires for the specific
missing-index condition.

### 3. Shared auto-heal plumbing relocates out of `handlers.rs`

`load_db` and `build_indices_once` move from `handlers.rs` (where they were private) to
`app_state.rs` (both made `pub`), and `MISSING_INDEX_USER_MSG` moves to `error.rs` next to
`is_missing_index_error`. This lets `episode.rs` reuse the exact same primitives the search
handlers use — including the same DCLP-under-`write_lock` guarantee that prevents concurrent
writers from triggering redundant/conflicting rebuilds — without `episode.rs` and `handlers.rs`
reaching into each other's internals. This mirrors the precedent ADR-0034 set when
`is_missing_index_error`/`is_already_exists_error` moved to `error.rs` for the same reason (two
call sites needing shared logic). `handlers.rs`'s three existing auto-heal call sites
(`handle_find_entities`, `handle_find_relationships`, `handle_search_passages`) import the
relocated names with no behavior change.

### 4. Fixed a pre-existing, unrelated startup-recovery classification bug discovered while testing this change

Testing FR-002 end-to-end (spawning the real binary against a corrupted WAL, then checking
`indices_built` post-recovery) surfaced that `bootstrap_app_state`'s recoverable-error classifier
only matched lbug's `"Corrupted wal file"` message (raised for an invalid WAL record type,
`wal_record.cpp`), not lbug's other WAL-corruption message, `"Checksum verification failed, the
WAL file is corrupted."` (raised by the checksum check in `wal_replayer.cpp`) — the shape produced
by a torn/garbage WAL tail, which is the more common real-world corruption pattern (e.g. a crash
mid-write). Before this fix, that corruption shape was classified as fatal and skipped
self-recovery entirely, meaning ADR-0009's self-recovery path was effectively dead for this
corruption class in production. Both message shapes are now recognized as recoverable. This bug
was orthogonal to #208's own scope but blocked verifying FR-002 with a real test, so it's fixed
alongside.

## Consequences

- `knowledge_status.indices_built` is normally `true` immediately after startup (fresh DB or
  post-recovery), before any request — not just "eventually true after the first search or
  ingest." The README's description of `indices_built` is updated accordingly: eager
  build-at-startup is now the normal case; lazy auto-heal (search handlers, and now the dedup
  path) is the fallback-only mechanism for post-`clear_all`/`rebuild_from_wal` staleness.
- A genuine index-build failure now fails startup on the direct-open path — a new failure surface
  that did not exist before this change. Operators who see startup fail this way should treat it
  the same as any other fatal startup error (check logs, `lbug` extension state, disk space).
- Sustained ingest past the hybrid-dedup threshold — with one writer or many concurrent writers —
  no longer fails chunks with the missing-index Binder exception, on a fresh workspace or
  immediately after `knowledge_clear_all`/`knowledge_rebuild_from_wal`.
- The hybrid-dedup threshold (1,000 entities per `group_id` by default,
  `LIMINIS_DEDUP_HYBRID_THRESHOLD`-overridable) and the brute-force/hybrid selection logic are
  unchanged — only the reliability of the index the hybrid path depends on changes.
- This closes ADR-0034 §5's explicitly deferred gap: `recovery.rs`'s successful index build is now
  reflected in `AppState.indices_built` rather than under-reported until the next auto-heal event.

## Related

- ADR-0025: Auto-Heal Index Build and Bulk-Load Reload Pattern — the precedent pattern this issue
  extends to the ingest dedup path.
- ADR-0034: Observable Index-Build Outcome — made index-build functions genuinely fallible and
  `indices_built` reflect real outcomes; §5 flagged the `recovery.rs` gap this issue closes.
- ADR-0009: Degraded-Mode Startup and In-Process Recovery — governs why `maybe_db` can be `None`
  (degraded mode); the eager build only applies once a DB is open, unchanged by this issue.
- `crates/core/tests/dedup_auto_heal_integration.rs`: concurrent ingest past the hybrid threshold
  on a never-indexed DB (SC-001/SC-004), immediate post-`clear_all` resume (SC-002), and the
  stale-`indices_built`-flag quirk (FR-006).
- `crates/core/src/episode.rs` `#[cfg(test)] mod tests`: `resolve_phase_b`'s genuine-error
  classification (FR-007), unit-tested directly since the helper is private.
- `crates/service/tests/eager_index_build.rs`: fresh-startup and post-recovery-startup
  `indices_built: true` on the first `knowledge_status` call (SC-003).
