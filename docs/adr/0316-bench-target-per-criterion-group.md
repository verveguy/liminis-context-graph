# ADR-0316: One `[[bench]]` Target Per `criterion_group!`

**Status**: Accepted
**Date**: 2026-08-02
**Issue**: #316 (the 15–18 minute release test suite is the root cause behind the headless-stall
class); casualties #208, #190, #219, #212, #236, #283, #297

## Context

CI's PR-blocking `test` job measured 15–18 minutes even on a warm cache — long enough that an
agent stage worker deciding it needed the release test suite would background the command and
stall waiting for a notification that never arrives headlessly (the failure class this issue's
spec documents). The spec's own stated hypothesis was that `cargo test --release`'s ~50
integration-test binaries were the dominant cost. Measurement (Research stage, #316) disproved
this: `cargo test --release` is ~5 minutes, of which only ~1 minute is actual test execution
(compile+link is the rest). The dominant cost — ~10.5–11 minutes, ~58–60% of the job — was the
`dedup decision-overlap check (R-003)` step: `cargo bench --bench search -- dedup_overlap_check`.

`crates/core/benches/search.rs` registered four `criterion_group!`s (`benches`, `dedup`,
`dedup_50k`, `name_lookup`, eleven bench functions total) behind a single `criterion_main!`. Each
function's setup — `setup_bench_db_n(n, dim)`, which inserts `n` entities one row at a time and
then builds an HNSW vector index + BM25 FTS index via `build_indices_and_constraints()` — lived in
the function body, outside the `c.bench_function(...)` closure. Criterion's `-- <filter>` CLI
argument only gates whether the *measurement loop inside* `bench_function` runs; it has no effect
on ordinary Rust code that runs before that call. So `cargo bench --bench search --
dedup_overlap_check` unconditionally ran every other group's setup too — 100+100+1,000×3+10,000×2+
50,000×2+10,000×2 = 143,200 rows, most of it through the HNSW/FTS index build — before ever
reaching the ~1,000-row overlap check it was actually asked for. This is a well-documented Criterion
footgun (filters operate on registered bench IDs, not on arbitrary code paths reachable before
registration), not a bug in this repo's Criterion version.

The same file was also invoked three times by `.github/workflows/bench.yml` (`-- 1k`, `-- 10k`,
`-- dedup_50k`), each paying the same unconditional 143,200-row setup on top of whatever it was
actually trying to measure.

## Decision

Split `search.rs` into five separate bench source files, each with its own `[[bench]]` Cargo
target and its own `criterion_main!`. Three of the former groups map one-to-one onto a new file
(`benches` → `hybrid_search.rs`, `dedup_50k` → `dedup_50k.rs`, `name_lookup` → `name_lookup.rs`);
the fourth, `dedup`, splits into two: `dedup_overlap_check.rs` carves out
`bench_dedup_overlap_check` alone, since it's the one function that's actually PR-gating, and the
rest of that group's functions move to `dedup_search.rs`:

- `dedup_overlap_check.rs` — `bench_dedup_overlap_check` only (the R-003 gate)
- `dedup_search.rs` — the four 1k/10k dedup functions
- `dedup_50k.rs` — the two 50k dedup functions (including the R-007 ≤30%-ratio gate)
- `name_lookup.rs` — the two name-lookup functions
- `hybrid_search.rs` — the two general hybrid-search functions

Separate Cargo bench targets compile to separate binaries with separate `main()`s (via
`criterion_main!`), so invoking one target no longer executes any other target's setup code at
all — isolation becomes a property of *how the targets are laid out*, not of a filter string that
has to be remembered and gets silently defeated the moment a new function is added to a shared
group. `crates/core/benches/common/mod.rs` holds the two functions shared across all five files
(`setup_bench_db_n`, `measure_brute_force_ns`), included via `#[path = "common/mod.rs"] mod
bench_common;` in each bench file.

`ci.yml`'s R-003 step now runs `cargo bench --bench dedup_overlap_check` directly — no filter
needed, since the binary contains only that one function. Measured: ~1m19s (vs. ~10.5–11m before),
even without the surrounding job's warm build cache. `bench.yml`'s three steps retarget to
`dedup_search` (`-- 1k`, `-- 10k`) and `dedup_50k` (no filter).

### Why the file went in `benches/common/`, not `benches/bench_common.rs`

Cargo auto-discovers every `.rs` file directly under `benches/` as its own bench target unless
`autobenches = false` is set — including files never declared in an explicit `[[bench]]` entry.
A first attempt at this split placed the shared helpers in `benches/bench_common.rs`; `cargo
bench` then built and ran it as an eleventh, redundant target (using the default libtest harness,
since it has no `criterion_main!` and no explicit `harness = false`). Moving it to
`benches/common/mod.rs` avoids auto-discovery entirely — mirroring the existing
`crates/core/tests/common/` convention, which is excluded from Cargo's test-binary
auto-registration the same way.

## Consequences

### Positive

- The CI-blocking R-003 gate's cost drops from ~10.5–11 minutes to ~1–2 minutes, without moving
  it off the required PR check — FR-003 ("must not reduce what's verified before merge") is
  satisfied trivially, since nothing moved to post-merge.
- The isolation property is structural (target layout), not a hand-maintained filter-string
  convention that silently stops working the next time someone adds a function to a shared group
  — this is what FR-004 asked for.
- `bench.yml`'s three on-demand measurement runs each pay only their own relevant setup instead
  of all 143,200 rows every time.
- R-003 (95% decision-overlap) and R-007 (≤30% hybrid/brute-force ratio) assertion logic is
  unchanged — only moved, not rewritten; both were re-run after the split to confirm.

### Negative / Residual risks

- `dedup_search.rs` still bundles the four 1k/10k functions behind one `criterion_main!`, so a
  `bench.yml` `-- 1k` invocation still pays the 10k functions' setup too (and vice versa). This
  residual overlap is deliberately not split further: `bench.yml` is on-demand
  (`workflow_dispatch`), not budget-constrained, and splitting to four targets there would add
  binaries for no PR-path benefit.
- The `cargo test --release` integration-test side (~50 binaries, ~4m compile+link, ~1m
  execution) was measured and found not to be a material bottleneck — no restructuring was applied
  there. If that changes as the test count grows, `.github/workflows/real-corpus-e2e.yml`'s
  `#[ignore]` + naming-convention + separate-post-merge-workflow pattern is the established
  precedent to extend.
- Every `--bench search` call site had to be updated in the same change (`ci.yml`, `bench.yml`);
  a missed one fails the workflow immediately rather than silently, but this is still a
  multi-file-consistency requirement worth flagging for future changes to bench target names.
- Splitting into five separate compilation units means a PR that only ran
  `cargo bench --bench dedup_overlap_check` would no longer catch a compile break in the four
  sibling targets (`dedup_search`, `dedup_50k`, `name_lookup`, `hybrid_search`) — before the
  split, they shared one binary with `dedup_overlap_check`, so compiling it compiled all of
  them too. `ci.yml`'s `test` job now runs a follow-up `cargo check --release --benches -p
  lcg-core` step (type-check only, no codegen, ~10-30s reusing the job's already-built release
  deps) specifically to keep that coverage without paying `bench.yml`'s full measurement cost.

## Alternatives Considered

### Move R-003 off the PR-blocking path (post-merge gate, per #298's precedent)

Rejected: unnecessary once the actual fix (isolating the target) makes the check fast enough to
stay PR-blocking, and would have pulled in #298's post-merge failure-signalling mechanism as a
hard dependency for no benefit — #298 remains relevant only to the "post-merge gate" branch of
FR-003, which this decision doesn't exercise.

### Keep one `criterion_main!`, pass `--bench search --exact <name>` everywhere

Rejected: `--exact` narrows which bench *function* is measured, but does nothing about which
functions' *setup* code executes — the root cause is unaffected. This is the same footgun in a
slightly stricter filter string.

### Restructure `cargo test --release`'s ~50 integration-test binaries

Rejected for this issue: measured compile+link (~4m total) and execution (~1m total) don't
support it. The bench-harness fix alone closes the measured gap; further splitting the test
binaries would add `ld` invocations against `liblbug.a` without a demonstrated payoff.

## Related

- `crates/core/benches/dedup_overlap_check.rs`, `dedup_search.rs`, `dedup_50k.rs`,
  `name_lookup.rs`, `hybrid_search.rs`, `common/mod.rs` — the split implementation.
- `.github/workflows/ci.yml` — R-003 gate, now `cargo bench --bench dedup_overlap_check`.
- `.github/workflows/bench.yml` — on-demand perf measurement, retargeted to the split bins.
- `.github/workflows/real-corpus-e2e.yml`, `docs/adr` precedent (#217/#234–236) — the
  `#[ignore]` + naming-convention + separate-workflow pattern for the integration-test side,
  not extended here but the established option if that side ever needs it.
- #298 — post-merge failure signalling; a soft dependency only for the rejected "post-merge
  gate" alternative above, not exercised by this decision.
