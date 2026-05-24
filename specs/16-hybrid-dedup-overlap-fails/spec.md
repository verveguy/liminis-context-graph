# Feature Specification: Fix Hybrid Dedup Overlap Failure (R-003)

**Feature Branch**: `fabrik/issue-16`
**Created**: 2026-05-19
**Status**: Draft
**Input**: User description: "The dedup_overlap_check bench asserts hybrid dedup agrees with brute-force on ≥95% of dedup decisions over 100 probes against a 1k-entity corpus. Current observed overlap is 64%, so the assertion fires. Hybrid dedup silently misses ~36% of matches that brute-force cosine would find."

## Background

The `hybrid_dedup_similar_entity` function (HNSW + BM25 + RRF) was implemented in issue #5 to replace the O(N) brute-force cosine path for large workspaces. The constitution requires ≥ 95% decision overlap with brute-force and ≤ 30% wall time of the Python baseline at 50k entities (Performance & Resource Budgets, v1.0.0).

With the `-rdynamic` build flag and `INSTALL`/`LOAD` infrastructure bugs fixed (issues preceding #16), the `dedup_overlap_check` bench can now exercise the hybrid path end-to-end for the first time. It reveals the algorithmic problem: 64% overlap vs. the 95% required. In production this means ~36% of entity merges that brute-force would perform are silently skipped, creating duplicate entities and regressing quality relative to the Python graphiti-core baseline.

The bench at `liminis-graph-core/benches/search.rs:281` is deterministic and serves as the acceptance gate for this fix.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Hybrid dedup matches brute-force at ≥ 95% on 1k synthetic corpus (Priority: P1)

An operator running the bench harness against a 1k-entity workspace with axis-aligned embeddings sees `dedup_overlap_check` pass, confirming that the hybrid path's recall is sufficient to make the same dedup decisions as brute-force cosine.

**Why this priority**: This is R-003, a constitution-level gate. The 64% overlap means roughly one in three dedup decisions is wrong in production-sized workspaces, directly causing duplicate entity proliferation that degrades all downstream search quality.

**Independent Test**: Run `cargo bench --bench search -- dedup_overlap_check`. The bench constructs a deterministic 1k-entity corpus, runs 100 probe queries through both `hybrid_dedup_similar_entity` and `brute_force_similar_entity`, and asserts decision overlap ≥ 95%.

**Acceptance Scenarios**:

1. **Given** the bench's 1k corpus with axis-aligned unit embeddings, **When** `dedup_overlap_check` runs 100 probes, **Then** decision overlap is ≥ 95% with no assertion panic.
2. **Given** the same fix, **When** `bench_dedup_brute_force_50k` and `bench_hybrid_entity_search_50k` run, **Then** hybrid dedup wall time remains ≤ 30% of the Python brute-force baseline and no performance regression is introduced.

---

### User Story 2 - Fix stays within LadybugDB-only constraint (Priority: P1)

A contributor reviewing the fix sees that no external ANN library or shim was introduced — the improvement uses only LadybugDB's native HNSW and FTS knobs (e.g., increasing `CANDIDATE_K`, adjusting RRF parameters, or improving BM25 query construction).

**Why this priority**: Principle III (LadybugDB Only) is NON-NEGOTIABLE. Any shim or external index would violate it and require a constitution amendment, which is out of scope.

**Independent Test**: `cargo tree` shows no new dependencies. The fix is confined to changes in `db.rs`, `search.rs`, and/or `benches/search.rs`.

**Acceptance Scenarios**:

1. **Given** the final PR diff, **When** `cargo tree` is inspected, **Then** no new crate dependencies appear in `Cargo.toml` or `Cargo.lock`.
2. **Given** the fix, **When** it is applied, **Then** all five constitution principles are satisfied as confirmed by the Plan stage's Constitution Check.

---

### Edge Cases

- Synthetic axis-aligned embeddings (unit vectors along each dimension axis, ~125 entities per axis at dim=8) create high local density; the fix must handle this distribution, not just sparse/random corpora.
- BM25 tokenization of numeric entity names ("Entity 0", "Entity 127") may produce zero or low-recall FTS results; the fix must either improve tokenization or compensate via other means without breaking non-numeric entity names.
- `ef` is not configurable in lbug 0.16.1; any fix that depends on raising HNSW `ef` is not viable under the current dependency.
- Raising `CANDIDATE_K` improves recall but increases post-RRF cosine recheck cost; the fix must not push 50k dedup wall time above 30% of the Python baseline.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `hybrid_dedup_similar_entity` MUST produce dedup decisions that agree with `brute_force_similar_entity` on ≥ 95% of probe queries over the bench's deterministic 1k-entity corpus.
- **FR-002**: The fix MUST use only LadybugDB-native APIs (HNSW index, FTS index, Cypher) — no external ANN libraries, no shims, no new Cargo dependencies.
- **FR-003**: The fix MUST not regress the 50k-entity dedup wall time above 30% of the Python brute-force baseline.
- **FR-004**: The existing `bench_hybrid_entity_search_50k` and `bench_dedup_brute_force_50k` benches MUST continue to pass.
- **FR-005**: The Research stage MUST triage the four suspected causes (HNSW recall, RRF weighting, CANDIDATE_K, BM25 tokenization) and identify which are contributing, before the Plan stage selects a fix strategy.

### Key Entities

- **`hybrid_dedup_similar_entity`**: The two-stage retrieval function in `db.rs` that combines HNSW vector search and BM25 FTS, fuses candidates via RRF, and applies a cosine threshold — currently missing ~36% of true matches.
- **`CANDIDATE_K`**: The per-source candidate limit (currently 10) passed to HNSW and BM25 retrieval; a primary tuning lever for recall.
- **`rrf_fuse`**: The RRF rank fusion function in `search.rs` that merges vector and FTS candidate lists.
- **`dedup_overlap_check`**: The benchmark function in `benches/search.rs` that is the authoritative acceptance gate for this fix.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `cargo bench --bench search -- dedup_overlap_check` exits 0 with reported overlap ≥ 95% over 100 probes on the 1k deterministic corpus.
- **SC-002**: No new Cargo dependencies are introduced.
- **SC-003**: `bench_hybrid_entity_search_50k` and `bench_dedup_brute_force_50k` continue to pass their respective wall-time gates (≤ 30% of Python baseline for hybrid dedup at 50k).
- **SC-004**: The fix is confined to `liminis-graph-core/src/db.rs`, `liminis-graph-core/src/search.rs`, and/or `liminis-graph-core/benches/search.rs` — no IPC surface changes, no WAL changes.

## Assumptions

- lbug 0.16.1 is the pinned version; `ef` is not configurable in this version and remains at the library default.
- The bench's synthetic corpus (1k entities, axis-aligned unit embeddings at dim=8, ~125 entities per axis) is the authoritative test distribution; fixing it on this distribution is sufficient for the acceptance gate.
- The Python baseline JSON (`benches/python_baseline_ns.json`) committed from issue #5 remains valid; no re-measurement is required unless the fix changes the hot path materially.
- The fix is algorithmic (parameter tuning, query construction, RRF weighting) rather than architectural; no new Conn methods are required beyond what issue #5 already introduced.

## Out of Scope

- Replacing LadybugDB's HNSW with an external ANN library (violates Principle III).
- Removing or weakening the 95% overlap requirement (weakens R-003).
- Changing the bench's synthetic corpus distribution or probe count.
- Any IPC surface changes or WAL format changes.
- Raising the HNSW `ef` parameter (not configurable in lbug 0.16.1).

## Source References

- `liminis-graph-core/src/db.rs` — `hybrid_dedup_similar_entity` (~line 533), `vector_search_entities`, `fts_search_entities`
- `liminis-graph-core/src/search.rs` — `rrf_fuse`
- `liminis-graph-core/benches/search.rs` — `bench_dedup_overlap_check` (~line 248), assertion at line 281
- `specs/001-rust-knowledge-graph/issues/005-hnsw-bm25-dedup-plan.md` — AD-2 (candidate_k), AD-3 (cosine recheck)
- `.specify/memory/constitution.md` — Principle III (LadybugDB Only), Performance & Resource Budgets
