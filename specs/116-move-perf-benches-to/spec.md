# Feature Specification: Move Performance Benches Off Per-Push CI to On-Demand Invocation

**Feature Branch**: `fabrik/issue-116`
**Created**: 2026-05-28
**Status**: Draft
**Input**: 2026-05-28 — the `bench compile (stub)` and `bench-dedup` jobs in `.github/workflows/ci.yml` run on every push and pull_request event today, each paying the full ~1h `LBUG_BUILD_FROM_SOURCE=1` cost. The PR-time perf benches under `bench-dedup` are gated to `main`-push-only (see lines 86-95), but the bench *binaries* themselves still compile on every PR (lines 65-66, 82-82), which is the cost driver. The user has chosen: keep correctness gates running automatically, move pure performance measurement to specific invocation.

## Background

The current `bench-dedup` job has a useful PR-time gate — `dedup_overlap_check` at line 82 — which is a *correctness* assertion that R-003 (95% decision overlap) still holds. That stays on PRs. Everything else in the bench surface is *measurement*, not *gating*:

- `bench compile (stub)` (lines 53-66) just runs `cargo bench --no-run` to ensure benches compile. This is a build-the-bench-binaries check, not a measurement. It costs a full lbug C++ build per PR.
- `dedup bench 1k / 10k / 50k` (lines 85-95) only run on main pushes today, NOT on PRs. So PRs already don't measure perf — but they still pay the full bench-binary compile via `bench compile (stub)`.

The structural waste: every PR spins up `bench compile (stub)` and `bench-dedup` (correctness gate) on separate ubuntu-latest runners, each rebuilding lbug from C++ source. Three parallel lbug builds per PR. The user's framing — "specific invocation rather than every run" — points at the right reorganisation:

- Keep the correctness gate (dedup overlap check) on PRs, folded into the existing `test` job so it shares lbug build cost.
- Move bench-binary-compile and perf measurement to a separate workflow that runs on `workflow_dispatch` (manual trigger via the Actions UI or `gh workflow run`) and optionally on a nightly `schedule`.
- The combined effect with the sibling lbug-cache issue: PR-time CI shrinks from three slow jobs to one, AND that one finishes fast.

This issue is independently valuable: even without the lbug cache landing, eliminating the duplicate bench-time lbug builds removes two-thirds of the runner-minutes per PR and reduces cache-eviction pressure on the 10 GB per-repo cache cap (which is itself part of why warm-cache hits underperform today).

## User Scenarios & Testing *(mandatory)*

### User Story 1 — PRs No Longer Pay For Bench-Binary Compilation (Priority: P1)

When an engineer opens a PR that doesn't touch bench code, CI does not compile bench binaries. The bench compile step runs only when explicitly invoked (or on a nightly schedule, if configured).

**Why this priority**: this is the simplest waste-elimination available. Bench compilation is not a PR-gating concern today — it's measurement infrastructure that happens to be wired into every PR. Removing it from PRs is pure savings with no loss of safety.

**Independent Test**: Open a no-op PR. Inspect the running workflows. Assert that no job is compiling `cargo bench --no-run`. The `dedup_overlap_check` (correctness gate) MUST still run.

**Acceptance Scenarios**:

1. **Given** a PR is opened, **When** the CI workflow triggers, **Then** `cargo bench --no-run` is NOT executed on any job.
2. **Given** a PR is opened, **When** the CI workflow triggers, **Then** the `dedup_overlap_check` correctness gate still runs (somewhere — in `test` or in a dedicated lightweight job) and still blocks merge on failure.

---

### User Story 2 — Perf Benches Run On Explicit Invocation (Priority: P1)

An engineer (or Fabrik, or a scheduled cron) can trigger the full perf-bench suite without opening a PR. The trigger is a single `gh workflow run` command or a click in the Actions UI.

**Why this priority**: replaces today's "run automatically on main push" with "run when someone actually wants the number." This is the user's stated preference. Perf trends still measurable, just no longer on every commit.

**Independent Test**: From a local checkout, run `gh workflow run bench.yml` (or whatever the new workflow is named). Confirm the workflow runs, builds the bench binaries, executes `dedup bench 1k`, `10k`, and `50k`, and reports results visibly (in the workflow run summary or as an uploaded artefact).

**Acceptance Scenarios**:

1. **Given** a developer wants to measure perf, **When** they run `gh workflow run <bench-workflow-name>` from a local checkout, **Then** the perf bench workflow runs and reports `1k`, `10k`, and `50k` numbers.
2. **Given** the workflow has finished, **When** the developer opens the run in the GitHub UI, **Then** the bench output is readable — either as job log output, a summary step, or an uploaded artefact (text/markdown).
3. **Given** a developer wants to compare against a baseline, **When** they trigger the workflow with an input parameter (e.g., a branch or commit SHA), **Then** the workflow runs against that ref. This is a stretch goal; the minimum is "runs on the workflow ref."

---

### User Story 3 — Optional Nightly Scheduled Perf Run (Priority: P3)

The project optionally runs the perf bench nightly via `schedule:` cron so a perf-regression trend is still visible without daily manual invocation.

**Why this priority**: nice-to-have. A nightly run captures drift over time; without it, perf trends depend on someone remembering to run benches. But the user's framing emphasised "specific invocation," so this is opt-in, not the default.

**Acceptance Scenarios**:

1. **Given** the nightly schedule is enabled (off by default, on by uncommenting the `schedule:` block), **When** the cron fires, **Then** the bench workflow runs and the results are accessible in the Actions tab.
2. **Given** a perf regression has been introduced on main, **When** the next nightly run completes, **Then** the regression is visible in the workflow output. (No automatic alerting required — visibility is enough for this priority level.)

## Requirements *(mandatory)*

- **FR-001.** The current `bench compile (stub)` job MUST be deleted from the per-push/per-PR workflow. Bench compilation MUST happen only when a perf workflow is explicitly triggered.
- **FR-002.** The current `bench-dedup` job's perf measurements (`dedup bench 1k`, `10k`, `50k`) MUST be deleted from the per-push workflow and moved to a separate workflow file (e.g., `.github/workflows/bench.yml`) triggered by `workflow_dispatch` (and optionally `schedule`).
- **FR-003.** The current `bench-dedup` job's correctness gate (`dedup_overlap_check`, today at line 82) MUST be preserved on per-PR runs. Implementation options (Research stage chooses):
  - Option A: keep a slimmed `bench-dedup` job in the main workflow that runs only the overlap check.
  - Option B: fold the overlap check into the `test` job, eliminating the separate `bench-dedup` job entirely. Saves a runner. Preferred if it doesn't substantially lengthen the `test` job's wall-clock.
- **FR-004.** The new `bench.yml` workflow MUST be discoverable via `gh workflow list` and runnable via `gh workflow run bench.yml`. Inputs (if any) MUST be documented inline in the workflow file's `on.workflow_dispatch.inputs` block.
- **FR-005.** The new `bench.yml` workflow MUST upload its bench output as a workflow artefact (markdown or text). At minimum, the criterion-style output of the runs MUST be retrievable from the Actions UI for 30 days (GHA default retention).
- **FR-006.** `.github/workflows/ci.yml` MUST continue to enforce all current correctness gates: `cargo test --release`, `dedup_overlap_check` (R-003), `cargo clippy -- -D warnings`, `cargo fmt --check`, and the "no ML runtime deps" cargo-tree check. No gates are lost.
- **FR-007.** Documentation: a short note in `CLAUDE.md` or a new `docs/BENCHMARKING.md` MUST explain how to trigger the perf workflow and where to read results.
- **FR-008.** The reduction in per-PR runner consumption MUST be measurable. After deployment, total runner-minutes per PR (sum of all jobs that previously ran) MUST drop by at least 50%. Today: ~3h cumulative (3× ~1h). Post-fix target: ≤ ~1.5h cumulative.

## Edge Cases

- **`dedup_overlap_check` requires the bench binary to be built.** Folding it into `test` (FR-003 option B) means `test` has to build the bench too. Verify during Research whether this materially lengthens `test`'s wall-clock; if so, prefer Option A (keep slim `bench-dedup` job).
- **A PR introduces a perf regression and merges without anyone running the bench workflow.** Possible. Mitigation: enable the optional nightly cron (US-3) so regressions surface within 24h.
- **The bench workflow is invoked against a non-main branch.** Should run cleanly. The bench numbers reflect that branch's code. No assumption that bench always runs against main.
- **A scheduled nightly run fails (transient flake).** No automatic remediation; the next night's run is the recovery path. Acceptable.
- **The bench workflow takes 1h+ to complete.** Same lbug build cost applies until the sibling cache issue lands. Acceptable; perf workflow runtime isn't gating any PR.

## Assumptions

- **A1.** The user's intent is to remove perf measurement from automatic runs, not to remove correctness gates. The `dedup_overlap_check` is correctness (R-003 spec requirement) and stays automatic; everything else moves to on-demand.
- **A2.** `gh workflow run` is an acceptable trigger mechanism. No fancy chatops or Slack integration needed.
- **A3.** GHA `workflow_dispatch` triggers are documented and stable. Engineers reading the workflow file can find the trigger.
- **A4.** Workflow artefact retention (30 days default) is sufficient for human review. Long-term perf history is out of scope.
- **A5.** Removing two of the three concurrent lbug builds per PR will, on its own, materially improve cache-hit rates by relieving GHA cache LRU pressure. This is a soft expectation, not a guarantee — the sibling lbug-cache issue is the deterministic fix.

## Success Criteria *(mandatory)*

- **SC-001.** After this change ships, a no-op PR triggers only the `test` job (and any other correctness-gating job, e.g., a slim `bench-dedup`). The `bench compile (stub)` job is gone; the perf bench job is gone from PR runs.
- **SC-002.** Total runner-minutes consumed per PR drops by ≥ 50% (today: ~3h cumulative; target: ≤ 1.5h cumulative).
- **SC-003.** A developer can run `gh workflow run bench.yml` from a local checkout and see bench results in the Actions UI within the workflow's wall-clock window.
- **SC-004.** All existing correctness gates (`cargo test --release`, `dedup_overlap_check`, `cargo clippy -- -D warnings`, `cargo fmt --check`, ML-runtime-deps check) continue to enforce on every PR.
- **SC-005.** Documentation update lands explaining how to trigger the perf workflow.
- **SC-006.** Fabrik PR pause-rate (current observation: near 100% — both #109 and #110 paused) drops measurably. With the lbug cache (sibling issue) also landed, the combined effect should put PR wall-clock under Fabrik's CI-wait window. This issue alone may not be sufficient — but it's a necessary precondition because cache eviction pressure compounds the slowness.

## Out of Scope

- Caching lbug across jobs (sibling Fabrik issue). The two issues compose well: this one removes two of the three concurrent lbug builds per PR; the sibling makes the remaining one fast.
- Setting up a perf-regression dashboard, alerting, or storage of historical bench numbers in a database. Workflow artefacts are sufficient for now.
- Changing the bench code itself (`benches/search.rs` etc.). Pure CI reorganisation.
- Adding macOS bench runs. Today's workflow is ubuntu-only; macOS perf is a developer-local concern.
- Replacing criterion or any bench library.

## Source References

- **`.github/workflows/ci.yml` lines 53-66** — current `bench compile (stub)` job (`bench-stub`). Pure build, no measurement, no gating value on PRs.
- **`.github/workflows/ci.yml` lines 68-95** — current `bench-dedup` job. Mixes correctness (`dedup_overlap_check`, line 82) with measurement (`dedup bench 1k/10k/50k`, lines 85-95, already gated to main pushes only).
- **specs/23-bench-dedup-30-perf/spec.md** — original bench infrastructure spec (R-003 95% decision-overlap requirement).
- **Sibling issue**: cache lbug build across CI runs — composes with this one. PR runtime improves dramatically when both land.
- **Direct motivation**: PRs #113 and #114, both paused by Fabrik because three concurrent ~1h jobs per PR exceed the CI-wait window.
