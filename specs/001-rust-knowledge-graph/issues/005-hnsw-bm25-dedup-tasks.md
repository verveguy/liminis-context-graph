# Tasks: Issue #5 — HNSW + BM25 Hybrid Dedup at Scale

**Input**: `specs/001-rust-knowledge-graph/issues/005-hnsw-bm25-dedup-plan.md`  
**Spec**: `specs/001-rust-knowledge-graph/spec.md#user-story-4`  
**Branch**: `fabrik/issue-5`

Tags: `[HOT]` = dedup hot path, bench REQUIRED before merge; `[LDB]` = LadybugDB driver layer.

---

## Phase 1: DB Layer — New `Conn` Helpers

**Purpose**: Add the three synchronous `Conn` methods needed by the hybrid dedup path.
These are pure additions to `db.rs`; no existing behaviour is changed.

- [x] T001 [US4] [LDB] Add `entity_count_in_group(group_id: &str) -> Result<usize, Error>` to `Conn` in `liminis-graph-core/src/db.rs`.  
  Query: `MATCH (e:Entity) WHERE e.group_id = '...' RETURN count(e)`.  
  Returns `0` on empty result (no entities, threshold check should use brute-force).

- [x] T002 [US4] [LDB] Add `get_entity_embeddings_by_uuids(uuids: &[String]) -> Result<Vec<(String, Vec<f32>)>, Error>` to `Conn` in `liminis-graph-core/src/db.rs`.  
  Query: `MATCH (e:Entity) WHERE e.uuid IN [...] RETURN e.uuid, e.name_embedding`.  
  Returns `(uuid, embedding)` pairs; entities with empty embeddings are excluded.

- [x] T003 [US4] [HOT] [LDB] Add `hybrid_dedup_similar_entity(name_embedding: &[f32], entity_name: &str, group_id: &str, threshold: f32) -> Result<Option<EntityRow>, Error>` to `Conn` in `liminis-graph-core/src/db.rs`.  
  Steps inside this method:  
  1. Call `vector_search_entities(name_embedding, &[group_id], 10)` — HNSW candidates.  
  2. Call `fts_search_entities(entity_name, &[group_id], 10)` — BM25 candidates.  
  3. Call `crate::search::rrf_fuse(&bm25, &vector)` — fused UUID list.  
  4. Call `get_entity_embeddings_by_uuids(&top_uuids)` for the top fused candidates.  
  5. Apply `cosine_similarity` vs. `threshold` (same as brute-force), return best match.  
  Add a module comment documenting that `ef` uses the lbug default (not configurable).

- [x] T004 [US4] [LDB] Add unit test `hybrid_dedup_returns_none_when_below_threshold` in `liminis-graph-core/src/db.rs` (in-file `#[cfg(test)]` block or `tests/db_dedup.rs`).  
  Seeds 3 entities, queries with a dissimilar embedding, asserts `None`.

- [x] T005 [US4] [LDB] Add unit test `hybrid_dedup_returns_best_match_above_threshold` in the same test location.  
  Seeds 3 entities with known embeddings, queries with embedding identical to entity 2, asserts returned UUID matches entity 2.

**Checkpoint**: `cargo test` passes; `hybrid_dedup_similar_entity` is callable on `Conn`.

---

## Phase 2: Bench Harness (HOT gate — must pass before `episode.rs` change lands)

**Purpose**: Establish the performance and decision-overlap baseline before wiring hybrid
dedup into the hot path. Writing the bench first lets the gate be measured on the
implementation directly.

- [x] T006 [US4] [HOT] Create `liminis-graph-core/benches/python_baseline_ns.json` with pre-measured Python brute-force wall-time nanoseconds at 1k, 10k, 50k entities.  
  Format: `{"1k": <ns>, "10k": <ns>, "50k": <ns>}`.  
  These are measured offline from `graphiti_service.py` against a deterministic synthetic corpus. Commit the file with a comment in `benches/search.rs` explaining the measurement machine and date.

- [x] T007 [US4] [HOT] Extend `liminis-graph-core/benches/search.rs` with a `setup_bench_db_n(n, dim)` helper that seeds exactly `n` entities with deterministic embeddings and builds both HNSW and FTS indexes. Replace the existing hard-coded 100-entity setup.

- [x] T008 [P] [US4] [HOT] Add `bench_dedup_brute_force_1k`, `bench_dedup_brute_force_10k` criterion bench functions in `liminis-graph-core/benches/search.rs`.  
  Each calls `conn.brute_force_similar_entity(query_emb, "bench", 0.85)` and iterates with Criterion.  
  Include `bench_dedup_brute_force_50k` (separate criterion group named `"dedup_50k"`).

- [x] T009 [P] [US4] [HOT] Add `bench_dedup_hybrid_1k`, `bench_dedup_hybrid_10k`, `bench_dedup_hybrid_50k` criterion bench functions alongside T008.  
  Each calls `conn.hybrid_dedup_similar_entity(query_emb, "entity query", "bench", 0.85)`.

- [x] T010 [US4] [HOT] Add a `decision_overlap_check` function (called from a bench, not an assertion in bench timing) that:  
  1. Runs brute-force on 100 probe queries against a 1k-entity DB, collects `Option<uuid>` decisions.  
  2. Runs hybrid on the same probes, collects decisions.  
  3. Computes `overlap = matching_decisions / total_probes`.  
  4. Panics if `overlap < 0.95`.  
  Wire this as a one-shot non-timed bench group named `"dedup_overlap_check"`.

- [x] T011 [US4] [HOT] Update `criterion_group!` in `benches/search.rs` to include the new dedup bench groups.  
  Ensure existing `bench_hybrid_entity_search` and `bench_hybrid_edge_search` still compile.

**Checkpoint**: `cargo bench --bench search --no-run` passes. `cargo bench --bench search -- dedup_overlap_check` completes without panic.

---

## Phase 3: Hot-Path Integration in `episode.rs`

**Purpose**: Wire the threshold gate and hybrid dedup into `add_episode`. Blocked on Phase 1 and Phase 2 completing so the HOT gate is satisfied before the hot path changes.

- [x] T012 [US4] [HOT] Add `static HYBRID_THRESHOLD: OnceLock<usize>` to `liminis-graph-core/src/episode.rs`.  
  Read from `LIMINIS_DEDUP_HYBRID_THRESHOLD` env var on first access; default `1_000`.  
  Add a module-level doc comment: `/// Default fallback threshold: 1000 entities. Override with LIMINIS_DEDUP_HYBRID_THRESHOLD env var.`

- [x] T013 [US4] [HOT] In `add_episode` (`episode.rs:57` `spawn_blocking` closure), before the entity dedup loop:  
  1. Call `conn.entity_count_in_group(&gid_owned)` once (outside the per-entity loop).  
  2. Store result as `let use_hybrid = count >= *HYBRID_THRESHOLD.get_or_init(...)`.  
  Inside the loop, replace the `brute_force_similar_entity` call with:  
  ```rust
  let existing = if use_hybrid {
      conn.hybrid_dedup_similar_entity(name_emb, &extracted.name, &gid_owned, DEDUP_THRESHOLD)?
  } else {
      conn.brute_force_similar_entity(name_emb, &gid_owned, DEDUP_THRESHOLD)?
  };
  ```

- [x] T014 [US4] Add integration test `dedup_falls_back_to_brute_force_below_threshold` in `liminis-graph-core/tests/` (new file `tests/dedup_integration.rs`).  
  Seeds 10 entities (below default 1 000 threshold), calls `add_episode`, asserts the second identical episode deduplicates to the same entity UUID. Sets `LIMINIS_DEDUP_HYBRID_THRESHOLD=50000` to force brute-force path in a second sub-test.

- [x] T015 [US4] Add integration test `dedup_uses_hybrid_above_threshold` in the same file.  
  Seeds 1 001 entities (above threshold), calls `add_episode` with a name matching entity 500, asserts dedup returns entity 500's UUID.  
  (Uses `LIMINIS_DEDUP_HYBRID_THRESHOLD=1000` explicitly.)

**Checkpoint**: `cargo test` passes including new integration tests. `cargo clippy -- -D warnings` clean.

---

## Phase 4: CI Bench Job

**Purpose**: Satisfy R-007 (bench MUST run in CI).

- [x] T016 [US4] Add `bench-dedup` job to `.github/workflows/ci.yml`.  
  Job runs on `ubuntu-latest`. Steps:  
  1. `cargo bench --bench search -- dedup_overlap_check` — always runs; panics on < 95% overlap.  
  2. `cargo bench --bench search -- bench_dedup_hybrid_1k bench_dedup_brute_force_1k bench_dedup_hybrid_10k bench_dedup_brute_force_10k --measurement-time 30` — runs on every PR.  
  3. `cargo bench --bench search -- dedup_50k` — runs only on `push` to `main` (job condition: `github.ref == 'refs/heads/main'`).  
  Retain existing `bench-stub` job (compile-check only, unchanged).

**Checkpoint**: CI `bench-dedup` job passes on PR. 50k bench runs on merge to `main`.

---

## Dependencies & Execution Order

- **Phase 1** (T001–T005): No dependencies. Start immediately.
- **Phase 2** (T006–T011): Depends on T003 (`hybrid_dedup_similar_entity` must exist for T009). T006–T008 can start in parallel with Phase 1.
- **Phase 3** (T012–T015): Depends on Phase 1 complete (T001–T003) AND Phase 2 HOT gate (T010 overlap check passing).
- **Phase 4** (T016): Depends on Phase 2 and Phase 3 complete.

### Parallel opportunities

- T001 and T002 can be written in parallel (different methods, same file section).
- T006 and T007 can start while Phase 1 is in progress.
- T008 and T009 can be written in parallel (different bench functions, same file).
- T014 and T015 can be written in parallel (different test functions).

---

## Acceptance Scenario Coverage

| Scenario | Task(s) |
|----------|---------|
| 50k-entity workspace, dedup ≤ 30% Python baseline wall time | T008, T009, T016 (50k bench) |
| 50k-entity workspace, ≥ 95% decision overlap vs. brute-force | T010 (overlap check) |
| 100-entity workspace (below threshold), brute-force fallback, exact match | T013 (branch), T014 (test) |
