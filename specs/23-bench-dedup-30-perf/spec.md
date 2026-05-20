# Feature Specification: Remove Incorrect 30% Perf-Ratio Assertion from bench_dedup_hybrid_10k

**Feature Branch**: `fabrik/issue-23`
**Created**: 2026-05-20
**Status**: Draft
**Input**: User description: "bench-dedup job fails at bench_dedup_hybrid_10k: hybrid dedup 10k: 59183010ns > 30% of Rust brute-force 18969075ns — the 30% ratio assertion is wrong at ≤10k scale and was already removed from the 1k bench in PR #18"

## Background

`bench_dedup_hybrid_10k` in `liminis-graph-core/benches/search.rs` contains a panicking assertion requiring hybrid dedup wall time to be ≤ 30% of brute-force cosine wall time at 10k entities. Measured ratios show this condition never holds at small N:

| Scale | brute_force | hybrid | ratio |
|---|---|---|---|
| 1k  | 2.30 ms | 12.08 ms | 5.25× brute |
| 10k | 17.92 ms | 59.18 ms | 3.30× brute |

Hybrid (HNSW + BM25 + RRF) carries fixed per-query overhead absent from brute-force's single vectorized cosine pass. Brute-force is O(N) with a tight inner loop; at 10k entities it runs in ~18ms. The hybrid path only wins asymptotically — somewhere north of ~100k entities the O(log N) HNSW path overtakes the O(N) cosine sweep. The ≤ 30% gate is a constitution-level requirement scoped to the 50k-entity scale (`bench_dedup_hybrid_50k`), not to 10k.

PR #18 (commit `194f938`) already removed the same assertion from `bench_dedup_hybrid_1k` and added an explanatory comment. The `_10k` variant was missed in that PR, leaving it as a broken CI gate.

The R-007 95% overlap gate (`dedup_overlap_check`) now passes — that fix from issue #16 is complete. This issue is solely about removing a misplaced perf assertion that was never valid at 10k scale.

## User Scenarios & Testing

### User Story 1 - `bench-dedup` CI job passes without removing the perf observation (Priority: P1)

A developer running `cargo bench --bench search` (or CI on `ubuntu-latest`) sees `bench_dedup_hybrid_10k` complete without a panic, and the bench output still prints the measured hybrid and brute-force timings for visibility.

**Why this priority**: The assertion causes a CI hard failure on every push, blocking all merges. The underlying claim (hybrid ≤ 30% of brute at 10k) is architecturally unsound; asserting it provides no correctness guarantee and actively mislabels a healthy system as broken.

**Independent Test**: Run `cargo bench --bench search -- bench_dedup_hybrid_10k`. Should complete without panic. Timing output should still appear in the bench report.

**Acceptance Scenarios**:

1. **Given** the fix is applied, **When** `cargo bench --bench search -- bench_dedup_hybrid_10k` runs, **Then** the bench completes without a panic, regardless of the hybrid/brute-force ratio.
2. **Given** the fix, **When** the bench output is inspected, **Then** hybrid and brute-force timings are still printed (perf observation retained), matching the `_1k` bench's behavior.
3. **Given** the fix, **When** `cargo bench --bench search -- dedup_overlap_check` runs, **Then** the R-007 95% overlap gate still passes (no regression to the issue-16 fix).

---

### Edge Cases

- The `bench_dedup_hybrid_50k` bench retains its `≤ 30% of brute-force` assertion — that gate is architecturally correct at 50k and must not be touched.
- If CI times vary across runners, removing an assertion must not silently suppress the timing output needed to detect future regressions.

## Requirements

### Functional Requirements

- **FR-001**: The panicking assertion at `liminis-graph-core/benches/search.rs` inside `bench_dedup_hybrid_10k` MUST be removed.
- **FR-002**: The `bench_dedup_hybrid_10k` bench MUST still measure and report hybrid dedup timing (the perf observation is retained; only the assertion gate is removed).
- **FR-003**: An explanatory comment MUST be added to `bench_dedup_hybrid_10k` matching the style of the `_1k` comment (commit `194f938`), stating that the perf gate applies at 50k, not at 10k.
- **FR-004**: `bench_dedup_hybrid_50k` MUST retain its existing `≤ 30% of brute-force` assertion unchanged.
- **FR-005**: `dedup_overlap_check` MUST continue to pass (no changes to the R-007 gate).

## Success Criteria

### Measurable Outcomes

- **SC-001**: `cargo bench --bench search -- bench_dedup_hybrid_10k` exits 0 on `ubuntu-latest`.
- **SC-002**: `cargo bench --bench search -- dedup_overlap_check` continues to exit 0 with reported overlap ≥ 95%.
- **SC-003**: `cargo bench --bench search -- bench_dedup_hybrid_50k` continues to exit 0 (50k perf gate intact).
- **SC-004**: The diff is confined to `liminis-graph-core/benches/search.rs` — no changes to `src/`, `Cargo.toml`, or any other file.

## Assumptions

- The `_10k` bench is not relied upon as an algorithmic correctness gate anywhere in CI beyond the assertion being removed; its only role after the fix is observation/reporting.
- No corpus or baseline JSON changes are needed — this is a bench-harness-only change.
- The `bench_dedup_hybrid_50k` perf gate holds on current main and does not need adjustment.

## Out of Scope

- Changing the `bench_dedup_hybrid_50k` assertion or any 50k-scale bench.
- Adding a new `bench_dedup_hybrid_100k` bench or any new corpus.
- Replacing the ratio gate with an absolute budget bench (that is a separate follow-up issue).
- Any changes to `src/db.rs`, `src/search.rs`, `Cargo.toml`, or CI workflow files.
- Amending or revisiting the constitution's perf budgets.

## Source References

- `liminis-graph-core/benches/search.rs:148–175` — `bench_dedup_hybrid_10k` (assertion to remove)
- `liminis-graph-core/benches/search.rs:103–125` — `bench_dedup_hybrid_1k` (reference treatment after PR #18)
- `liminis-graph-core/benches/search.rs:199–228` — `bench_dedup_hybrid_50k` (perf gate to preserve)
- Commit `194f938` — removed the same assertion from `_1k`
