# Feature Specification: Eager HNSW Index Build + Dedup-Path Auto-Heal to Fix Missing-Index Ingest Failures

**Feature Branch**: `fabrik/issue-208`
**Created**: 2026-07-24
**Status**: Draft
**Input**: User description: "Under sustained ingest, once a group_id passes the 1000-entity hybrid-dedup threshold, every chunk starts failing with `Binder exception: Table Entity doesn't have an index with name entity_name_embedding_idx` and `knowledge_status` reports `indices_built: false`. Reported in #203; field context in discussion #207 (5 parallel writers → 213/269 chunks failed)."

## Background

liminis-context-graph builds HNSW vector indices (`entity_name_embedding_idx` on `Entity`, plus indices on `Episodic` and `RelatesToNode_`) **lazily**: today they are created only by a search handler's missing-index auto-heal path (`handlers.rs` — `is_missing_index_error` → `build_indices_once` → retry, used by `knowledge_search`/`knowledge_find_entities` and friends), or by an explicit `knowledge_build_indices` call. Startup (`main.rs`) runs `init_schema` only, which intentionally does not build HNSW indices.

Ingest has two dedup strategies in Phase B of `episode.rs`, selected by entity count in the group: `brute_force_similar_entity` below the threshold (currently 1000, see `hybrid_threshold()`), and `hybrid_dedup_similar_entity` at or above it. The hybrid path issues a `QUERY_VECTOR_INDEX(entity_name_embedding_idx)` query directly and propagates any error with a bare `?` — unlike the search handlers, it has no missing-index detection or auto-heal.

The consequence: on a workspace where nothing has ever called a search handler or `knowledge_build_indices`, the vector indices are never built. As soon as any `group_id` crosses the 1000-entity threshold, every subsequent ingested chunk fails its dedup query with the reported Binder exception, and `knowledge_status` correctly reports `indices_built: false` because the index genuinely was never created — this is not a race that drops an existing index. A single sustained writer often escapes this by accident (a search call earlier in the session triggers the lazy build first), which is why the failure mode surfaced specifically under multi-writer field usage (#207: 5 parallel writers, 213/269 chunks failed) rather than in more typical single-writer testing.

This also has a cost dimension: Phase A (extraction + embedding generation, real LLM spend) runs *before* the failing Phase B dedup query, so every chunk that fails this way is billed for work whose result is discarded.

**Relationship to #202**: this issue and #202 both touch `crates/core/src/episode.rs`. Per the issue's stated dependency ordering, this issue lands first and #202 rebases onto it.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Ingest succeeds past the 1000-entity threshold on a fresh workspace (Priority: P1)

An operator starts liminis-context-graph against a brand-new (or freshly cleared) workspace and begins sustained ingestion — with one writer, or with several concurrent writers — of content that will push one or more `group_id`s past the 1000-entity dedup threshold. No chunk should fail with the missing-index Binder exception, regardless of writer concurrency.

**Why this priority**: this is the reported production failure (#203, #207) — the core bug this issue exists to fix.

**Independent Test**: Start the service against an empty workspace. Without issuing any search calls, ingest enough episodes into a single `group_id` to push its entity count past the hybrid-dedup threshold, using several concurrent writers. Assert zero chunks fail with the `entity_name_embedding_idx` Binder exception and all chunks that should succeed do succeed.

**Acceptance Scenarios**:

1. **Given** a freshly created workspace and a service that has just started, **When** the operator ingests episodes into a `group_id` with multiple concurrent writers, crossing the 1000-entity threshold partway through, **Then** every chunk is processed without a missing-index error, whether it hit the brute-force path (below threshold) or the hybrid path (at/above threshold).
2. **Given** the same scenario, **When** ingestion completes, **Then** `knowledge_status` reports `indices_built: true`.

---

### User Story 2 - First ingest after `knowledge_clear_all` or `rebuild_from_wal` succeeds without a prior search call (Priority: P1)

After the graph is cleared (`knowledge_clear_all`) or rebuilt from the WAL (`knowledge_rebuild_from_wal`), the vector indices may need to be (re)established before the next ingest can safely use the hybrid dedup path. The very next ingest — even if it is the first operation of any kind after the clear/rebuild, with no intervening search call — must succeed.

**Why this priority**: defense-in-depth for the state right after a destructive/administrative operation, where the eager startup build already happened once but the DB content (and therefore index validity) has since changed. This is the second, independent mechanism the issue asks for (the dedup-path auto-heal), not just a restatement of User Story 1.

**Independent Test**: Ingest past the hybrid threshold once (indices now built and populated). Call `knowledge_clear_all`. Immediately (no search call in between) resume ingestion past the hybrid threshold again. Assert no chunk fails with the missing-index error.

**Acceptance Scenarios**:

1. **Given** a workspace where indices were built and then `knowledge_clear_all` was called, **When** ingestion resumes and a `group_id` reaches the hybrid-dedup threshold, **Then** the dedup query auto-heals (detects the missing/stale index, rebuilds via the same mechanism the search handlers use, and retries) rather than failing the chunk.
2. **Given** the same scenario using `knowledge_rebuild_from_wal` instead of `knowledge_clear_all`, **When** ingestion resumes post-rebuild, **Then** the same auto-heal behavior applies and no chunk fails.

---

### User Story 3 - `indices_built` accurately reflects reality at all times (Priority: P2)

Operators and tooling rely on `knowledge_status`'s `indices_built` field to know whether vector search/dedup is ready. This field must never report `true` when the index is actually missing, and must never linger at `false` after a build has actually happened (whether via startup, recovery, auto-heal, or explicit `knowledge_build_indices`).

**Why this priority**: P2 because it's an observability correctness requirement that supports Stories 1 and 2 rather than being independently the source of ingest failures, but it is explicitly named in the issue's requirements and is how operators would currently detect this whole class of bug.

**Acceptance Scenarios**:

1. **Given** a fresh workspace, **When** the service finishes startup, **Then** `knowledge_status` reports `indices_built: true` before the socket accepts any ingest or search request.
2. **Given** a degraded-mode startup where recovery itself fails and no DB is opened, **When** `knowledge_status` is queried, **Then** `indices_built` reports `false` (indices cannot exist without an open DB) and this is consistent with existing degraded-mode reporting.
3. **Given** recovery succeeds (e.g. WAL-corruption auto-heal per ADR-0009), **When** startup completes, **Then** `indices_built: true` is reported, matching the fresh-startup case.

---

### Edge Cases

- **Large pre-existing DB at startup.** `create_vector_indexes` is already idempotent (an "already exists" error is swallowed; a genuine build failure still propagates). Building eagerly at startup on a DB that already has valid indices must be a cheap no-op, not a full rebuild.
- **Startup fails before a DB is opened (degraded mode).** Eager index build only applies once a DB is open (either the direct-open path or the post-recovery path). Degraded-mode startup with no DB continues to report `indices_built: false`, consistent with today.
- **Below-threshold ingest never needs the hybrid path.** A workspace whose group(s) never reach 1000 entities never exercises `hybrid_dedup_similar_entity`, but still benefits from indices being built eagerly (search handlers, and any future crossing of the threshold, are covered from the start rather than depending on lazy first-use).
- **Concurrent writers racing the same auto-heal.** Multiple concurrent ingest tasks that all hit a missing index at once must not each attempt a redundant/conflicting index build; the existing `build_indices_once` mechanism (guarded by `indices_built` + a lock, per its current use in the search handlers) is the precedent to reuse or mirror for the dedup path.
- **A genuine (non-missing-index) DB error on the dedup query.** Only the specific missing-index condition should trigger auto-heal-and-retry; other errors from the hybrid dedup query must still propagate as failures, not be silently swallowed or retried indefinitely.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The service MUST build the HNSW vector indices (at minimum `entity_name_embedding_idx`, `episodic_content_embedding_idx`, `edge_fact_embedding_idx`) immediately after `init_schema` succeeds during normal startup, before the IPC socket begins accepting ingest or search requests.
- **FR-002**: The service MUST also build the HNSW vector indices after a successful startup self-recovery (e.g. the WAL-corruption auto-heal path per ADR-0009) completes and before the socket begins accepting requests — recovery-then-serve must not skip the eager build.
- **FR-003**: The eager index build MUST be idempotent and cheap when indices already exist (reuses/relies on `create_vector_indexes`'s existing "already exists" swallowing behavior) so it does not materially slow startup on a large, already-indexed DB.
- **FR-004**: A genuine index-build failure during the eager startup/recovery build (anything other than "already exists") MUST propagate as a startup error rather than being silently ignored, so a real failure to build indices is not masked as success.
- **FR-005**: The ingest Phase B hybrid dedup vector query (`hybrid_dedup_similar_entity`, used once a `group_id`'s entity count reaches the hybrid-dedup threshold) MUST be wrapped in the same missing-index detection used by the search handlers: on a missing-index error, trigger the existing build-indices-and-retry mechanism, then retry the dedup query once.
- **FR-006**: If the retried dedup query in FR-005 still fails with a missing-index error (genuine build failure, not just a stale/missing index that got fixed), the error MUST propagate to the caller as a chunk failure — it must not retry indefinitely or silently fall back to a different dedup strategy.
- **FR-007**: Non-missing-index errors from the hybrid dedup query MUST propagate immediately without triggering the auto-heal/retry path.
- **FR-008**: `knowledge_status`'s `indices_built` field MUST report `true` after the eager startup/recovery build (FR-001/FR-002) completes successfully, and MUST report `true` after any auto-heal build triggered by the dedup path (FR-005) completes successfully — consistent with how it already reflects builds triggered by the search-handler auto-heal path and by explicit `knowledge_build_indices`.
- **FR-009**: The fix MUST NOT change the hybrid-dedup threshold (currently 1000 entities per `group_id`) or the brute-force/hybrid selection logic itself — only the reliability of the index the hybrid path depends on.

### Key Entities

- **HNSW vector index**: A named vector index over an embedding column (`entity_name_embedding_idx` on `Entity.name_embedding`, plus the `Episodic` and `RelatesToNode_` equivalents), created via `CREATE_VECTOR_INDEX` and required by both vector search and the hybrid dedup query path.
- **`indices_built` flag**: Service-wide state (surfaced via `knowledge_status`) indicating whether the HNSW indices are currently known to exist and be usable.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Sustained ingest of more than 1000 entities into a `group_id` on a fresh DB, driven by multiple concurrent writers, completes with zero chunks failing due to the `entity_name_embedding_idx` (or other HNSW index) missing-index Binder exception.
- **SC-002**: After `knowledge_clear_all` or `knowledge_rebuild_from_wal`, the first subsequent ingest that crosses the hybrid-dedup threshold succeeds without a preceding search call having built the index first.
- **SC-003**: `knowledge_status.indices_built` is `true` immediately after any successful startup (fresh DB or post-recovery) and before any ingest/search traffic is processed, and remains an accurate reflection of build state through auto-heal events.
- **SC-004**: A new integration test reproducing the reported >1000-entity concurrent-ingest scenario fails against the pre-fix code path and passes after the fix.
- **SC-005**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` all pass.
- **SC-006**: Startup time on a large, already-indexed existing DB is not materially affected by the eager build (idempotent no-op per FR-003).

## Assumptions

- **A1**: The existing `build_indices_once` / `is_missing_index_error` mechanism used by the search handlers (`handlers.rs`) is the correct pattern to reuse or mirror for the ingest dedup path — this issue is about closing the gap where that pattern isn't applied yet, not designing a new mechanism.
- **A2**: "Before the socket begins serving ingest" means before the IPC listener starts accepting/processing requests, not merely before some internal readiness flag flips — i.e., no request can reach the dedup path while indices are being eagerly built at startup.
- **A3**: Degraded-mode startup (no DB opened) is out of scope for the eager build — `indices_built` correctly stays `false` in that case, matching current documented behavior (ADR-0009).
- **A4**: The 1000-entity hybrid-dedup threshold and the choice between brute-force and hybrid dedup strategies are settled prior design (per FR-009) and are not reconsidered here.

## Out of Scope

- Broader ingest-concurrency redesign beyond fixing the missing-index failure.
- Changing the 1000-entity brute-force → hybrid dedup threshold.
- Surfacing per-chunk billing/cost information for chunks that fail (tracked separately if wanted, per the original issue).
- Any changes to `crates/core/src/episode.rs` unrelated to the dedup path's index auto-heal (this issue is scoped narrowly given #202 also touches this file and rebases onto this issue's changes).

## Source References

- **liminis-context-graph#203**: original bug report — ingest failures with `indices_built: false` past the 1000-entity threshold.
- **liminis-context-graph#207 (discussion)**: field reproduction — 5 parallel writers, 213/269 chunks failed, establishing that concurrent writers make the un-built-index window easy to hit.
- **liminis-context-graph#202**: separate in-flight work also touching `crates/core/src/episode.rs`; this issue is a blocker for #202 (native `blockedBy` set on #202) and should land first.
- Key existing code sites named in the root-cause analysis: `db.rs` (`create_vector_indexes` / index create-drop, ~lines 345-380), `episode.rs` Phase B dedup call site (~line 283, currently a bare `?`), `main.rs` startup sequence (~lines 270-290, `init_schema` only today), `handlers.rs` (`build_indices_once` ~line 35, and its use in the search auto-heal path ~lines 460-500).
- **ADR-0009**: degraded-mode startup & recovery — governs the recovery path this issue's FR-002 hooks into.
- **ADR-0025**: auto-heal index build — the precedent pattern this issue extends to the ingest dedup path.
- **ADR-0026**: episode-cursor WAL resume — relevant to the post-recovery serving sequence this issue's eager build must precede.
