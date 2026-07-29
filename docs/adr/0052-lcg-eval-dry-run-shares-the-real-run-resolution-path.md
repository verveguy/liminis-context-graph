# ADR-0052: `lcg-eval --dry-run` Shares the Real Run's Resolution Path

**Status**: Accepted
**Date**: 2026-07-29
**Issues**: #279

## Context

PR #278's `06-ontology-matrix.sh` needed a way to preview a multi-hour, real-money run
before committing to it: `DRY_RUN=1` printed the per-leg replay/live decision, the report
names, and any condition that would abort the real run. It was implemented as a second copy
of the real run's decision logic, hand-written in shell against `_common.sh`'s
`cassette_complete`/`cassette_key_check`/`sha256_of` helpers. During review it drifted from
the real path **twice**: once printing cassette names the real run would not actually use,
once omitting an abort condition (the identity guard) that the real invocation would still
hit. Both were caught by reviewers, not by any check — a preview that can silently disagree
with the thing it previews is worse than no preview, because it looks trustworthy.

#279 moves the underlying guards (corrupt/duplicate cassette detection, the identical-
cassette identity check, coverage-shortfall reporting) out of shell and into `lcg-eval`
itself, for exactly this class of defect — see the sibling ADRs and `crates/eval/src/
plan.rs`'s own module doc for the guard semantics. That move only pays off if `--dry-run`
cannot repeat the same mistake: a second, hand-maintained preview path inside `lcg-eval`
itself (e.g. a `main.rs` branch that reimplements what the real run's backend loop does)
would reintroduce precisely the drift this issue exists to eliminate, just one layer deeper.

## Decision

**`crates/eval/src/plan.rs::resolve(cli: &Args, chunks_len: usize) -> ResolvedPlan` is the
only place backend specs are resolved into a replay/live decision, a cassette record count,
and a guard verdict.** `main.rs::run()` calls it exactly once, immediately after the corpus
subset is selected and before anything else happens:

- `--dry-run`: prints `plan::render(&resolved)` and returns — no `CassetteWriter::open`, no
  backend loop, no judge client, exit 0.
- A real run: if `resolved.guard_violations` is non-empty, returns an `Err` joining them
  before any `build_extractor`/`run_backend` call; otherwise proceeds, first emitting an
  `eprintln!` for any FR-005 coverage-shortfall note (which never aborts — see `plan.rs`'s
  module doc for why a shortfall is a note, not a violation).

`resolve` itself never returns `Result` — guard violations are entries in a `Vec<String>`,
not an early return — specifically so both callers read the *same data* and differ only in
what they do with it. There is no second code path that decides replay-vs-live or evaluates
a guard independently; a `--dry-run`-only branch that reimplements any part of this
resolution is a bug, not a valid extension point.

**The constraint this ADR pins**: any new guard, any new backend kind's replay/live
resolution, or any new plan field belongs inside `plan::resolve` (and `plan::render` for its
`--dry-run` presentation), never duplicated separately in `main.rs`'s `--dry-run` branch or
its real-run branch. If a future change needs `--dry-run` to show something new, the change
goes into `ResolvedPlan`/`resolve`/`render`, and the real run picks it up automatically by
construction — not the other way around.

## Consequences

- `--dry-run`'s Acceptance Scenario 3 (the plan names a guard that would abort a real run,
  while still exiting 0) falls out of this design for free: `guard_violations` is populated
  identically either way, and only the caller's response to a non-empty list differs.
- A `--dry-run --record-cassette` invocation writes nothing, because the `--dry-run` branch
  returns before `CassetteWriter::open` is ever reached — not because of a separate check
  that the two flags conflict.
- The double file-read this implies (once in `plan::resolve` to validate/count, once more in
  the real backend loop's `build_extractor` → `ReplayingExtractor::load`) is an accepted
  tradeoff, not a defect: cassette files are local and small relative to network/judge
  latency, and avoiding it would mean threading loaded `ReplayingExtractor` instances back
  out of `plan::resolve` into the backend loop, which `plan.rs`'s Key Decisions in the #279
  plan explicitly scoped out as unnecessary restructuring of `backend::build_extractor`'s
  `Arc<dyn Extractor>` erasure.
- A reviewer auditing a future PR that touches `--dry-run` behavior has a direct test: if the
  diff adds logic to `main.rs`'s `if cli.dry_run { ... }` branch beyond calling
  `plan::render`, or adds a guard check to the real-run branch that isn't also visible in
  `ResolvedPlan`, that diff is reintroducing the exact defect class #278 review caught twice
  and #279 exists to close.

## Related

- Issue #279 spec (`specs/279-move-the-benchmark-guards/spec.md`): FR-001 (dry-run itself),
  FR-002/FR-003 (corrupt vs. duplicate cassette typing), FR-004 (identity guard), FR-005
  (coverage shortfall reporting).
- ADR-0044: the cassette record/replay seam (`ReplayingExtractor`, `CassetteRecord`) that
  `crates/core::cassette::load_records` — the function `plan::resolve` calls to validate and
  count a cassette backend — sits on top of.
- ADR-0048: the eval harness's overall module boundary (`cli.rs`/`backend.rs`/`runner.rs`/
  `report.rs`) that `plan.rs` extends; `cli.rs` remains documented as I/O-free, which is why
  `plan.rs` — not `cli.rs` — owns cassette loading and content hashing.
- `crates/eval/src/plan.rs`: `BackendPlan`, `ResolvedPlan`, `resolve`, `render`.
- `crates/eval/src/main.rs`: the two call sites (`--dry-run` short-circuit; guard-violation
  abort before the backend loop) that this ADR requires stay the only two.
- `crates/eval/scripts/06-ontology-matrix.sh`: its own `DRY_RUN=1` no longer previews
  guard-abort conditions (that duplicated logic was deleted per #279's FR-007) — it now
  covers only the shell-level replay/live resume decision; `lcg-eval --dry-run` is the
  canonical way to preview what a real invocation would do.
