# Implementation Plan: Issue #5 — HNSW + BM25 Hybrid Dedup at Scale (US4)

**Branch**: `fabrik/issue-5` | **Date**: 2026-05-19 | **Spec**: `specs/001-rust-knowledge-graph/spec.md#user-story-4`

## Summary

Replace the unconditional `brute_force_similar_entity` call in `episode.rs:65` with a
threshold-gated strategy: when workspace entity count ≥ `LIMINIS_DEDUP_HYBRID_THRESHOLD`
(default 1 000), use HNSW + BM25 candidate retrieval fused by RRF, then cosine-recheck
the top candidates. Below the threshold, retain brute-force exactly as-is. A bench harness
at 1k/10k/50k entities reports wall-time vs. a pre-committed Python baseline and decision
overlap vs. brute-force.

All building blocks (HNSW index, FTS index, `rrf_fuse`, `vector_search_entities`,
`fts_search_entities`) are already in the codebase from issue #2; this issue wires them
into the dedup hot path.

## Technical Context

**Language/Version**: Rust stable 2021  
**Primary Dependencies**: `lbug = "=0.16.1"` (HNSW + BM25 already confirmed working), `criterion` (bench), `std::sync::OnceLock` (threshold config)  
**Storage**: LadybugDB — Entity HNSW index `entity_name_embedding_idx` (cosine) and FTS index `entity_name_fts` created by `build_indices_and_constraints` (already in place)  
**Testing**: `cargo test` unit tests on `rrf_fuse` (already passing); new unit tests for hybrid dedup decision path; bench harness in `benches/search.rs`  
**Target Platform**: Linux (ubuntu-latest) + macOS (macos-latest)  
**Project Type**: Library crate (`liminis-graph-core`) — no IPC or binary changes  
**Performance Goals**: Dedup wall time ≤ 30% of Python brute-force baseline at 50k entities; decision overlap ≥ 95%  
**Constraints**: No IPC surface changes; no new Cargo deps; `ef` parameter is not configurable in lbug 0.16.1 (accept default)  
**Scale/Scope**: US4 only — touches `db.rs`, `episode.rs`, `benches/search.rs`, CI workflow

## Architecture Decisions

### AD-1: `hybrid_dedup_similar_entity` is a synchronous `Conn` method in `db.rs`

The dedup call is inside a `spawn_blocking` closure in `episode.rs:57`. `Conn` has a `'db`
lifetime that makes it `!Send`, so it cannot be moved across async task boundaries. The new
method must be synchronous and called directly on the `Conn` obtained in the closure,
matching the existing `brute_force_similar_entity` call pattern exactly.

### AD-2: candidate_k = 10 per retrieval path before RRF

For a 1:1 "is this entity a duplicate?" query, retrieving 10 candidates per path (vector +
BM25) before RRF gives sufficient recall with minimal overhead. The user-facing
`hybrid_entity_search` uses `limit * 3` but dedup only needs the single best match above
threshold. If overlap testing shows < 95% on synthetic corpora, candidate_k is the first
tuning lever.

### AD-3: Post-RRF cosine recheck via batch embedding fetch

After `rrf_fuse` returns the fused UUID list, the top-N (≤ `candidate_k`) entities need
their `name_embedding` to compute cosine similarity against the new entity's embedding.
`get_entities_by_uuids` (db.rs:471) deliberately omits `name_embedding` for performance.
A new `get_entity_embeddings_by_uuids` helper fetches only `(uuid, name_embedding)` for
the fused candidates. Cosine is then computed in Rust using the existing
`cosine_similarity` helper (db.rs:676). The best match ≥ `DEDUP_THRESHOLD` is returned,
preserving identical decision semantics to brute-force.

### AD-4: Configurable threshold via `LIMINIS_DEDUP_HYBRID_THRESHOLD` env var

A `static HYBRID_THRESHOLD: OnceLock<usize>` in `episode.rs` reads from the env var once
on first access, defaulting to 1 000. Documented in the module comment. The threshold check
calls a new `entity_count_in_group()` method on `Conn` (a single `count(e)` Cypher query).
At 50k entities the count query adds negligible overhead relative to the dedup query itself.

### AD-5: Python baseline committed as `benches/python_baseline_ns.json`

The upstream Python graphiti-core brute-force cosine path (`graphiti_service.py`) is measured offline against the
same fixed synthetic corpus (deterministic entity embeddings). Baseline wall-time nanoseconds
per scale (1k, 10k, 50k) are committed in a JSON file. The Criterion bench loads this file,
computes `rust_ns / baseline_ns`, and uses a custom `criterion` measurement (via
`black_box` + manual timing) to assert ≤ 0.30. This avoids subprocess execution in CI and
makes the baseline portable across CI environments.

### AD-6: Bench job runs actual iterations in CI, gated by scale

A new `bench-dedup` CI job runs `cargo bench --bench search -- dedup` with:
- `--measurement-time 30` for 1k and 10k corpora on every PR
- 50k corpus bench runs only on `push` to `main` or on a schedule, via a job condition

The existing `bench-stub` job (compile check only) is retained unchanged.

## Constitution Check

- **Principle I (IPC Parity)** — N/A. No IPC surface changes. No new wire methods, no param changes.
- **Principle II (Library and Binary Are Peers)** — PASS. `hybrid_dedup_similar_entity` is a public `Conn` method, callable from both the binary (via IPC `knowledge_add_episode`) and directly from library code.
- **Principle III (LadybugDB Only)** — PASS. Uses `QUERY_VECTOR_INDEX` and `db.index.fulltext.queryNodes` directly on `Conn`. No driver abstraction added.
- **Principle IV (WAL Is Authoritative)** — N/A. No WAL changes. Dedup is a read + conditional entity insert; WAL append for entity insert is already a TODO stub from issue #3 and is unchanged.
- **Principle V (LLM/Embedding Out-of-Process)** — PASS. No ML runtime added to Cargo.toml. Embeddings already in the DB; no embedding call needed during dedup retrieval.

### Performance budget gates

**Applies**: Dedup wall time ≤ 30% of Python brute-force baseline at 50k entities; decision overlap ≥ 95%.

Bench location: `liminis-graph-core/benches/search.rs` — `bench_dedup_brute_force_{1k,10k,50k}` and `bench_dedup_hybrid_{1k,10k,50k}` functions, plus `assert_decision_overlap`.

### Workflow gates

- Spec exists at `specs/001-rust-knowledge-graph/spec.md` ✓
- No IPC-touching changes: parity test not required ✓
- Hot-path (dedup) changes: bench REQUIRED before merge — planned in Phase 2 ✓
- No WAL code: TDD not required for this issue ✓
- No constitution deviations requiring ADR ✓

## Project Structure

```text
liminis-graph-core/
└── src/
    ├── db.rs          EXTENDED: entity_count_in_group(), get_entity_embeddings_by_uuids(),
    │                            hybrid_dedup_similar_entity()                 [HOT][LDB]
    └── episode.rs     EXTENDED: HYBRID_THRESHOLD OnceLock, entity-count branch [HOT]

liminis-graph-core/benches/
├── search.rs          EXTENDED: 1k/10k/50k corpora, brute-force + hybrid dedup [HOT]
│                               benches, decision-overlap assertion
└── python_baseline_ns.json   NEW: pre-measured Python wall-time ns per scale

.github/workflows/ci.yml  EXTENDED: bench-dedup job (actual run, scale-gated)
```

## Complexity Tracking

| Item | Notes |
|------|-------|
| candidate_k = 10 fixed | Not configurable for now; surfaced as a documented constant. If 50k overlap testing shows < 95%, increase to 20 and re-measure before merge. |
| `ef` not configurable in lbug 0.16.1 | Accepted. Document in `hybrid_dedup_similar_entity` that ef uses the lbug default. |
| Python baseline JSON committed | Baseline numbers are environment-dependent. They are generated on a single machine (Apple M2) and committed. CI assertions compare relative ratio (≤ 0.30), not absolute ns, which is environment-agnostic. |
| 50k bench in CI | 50k seeding (~50k INSERT + index build) is slow. Gated to `main` push only to avoid PR CI timeouts. Accepted trade-off per R-007. |
