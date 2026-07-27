# Feature Specification: Remove broken `LBUG_BUILD_FROM_SOURCE` flag from `bench.yml`

**Feature Branch**: `fabrik/issue-259`
**Created**: 2026-07-26
**Status**: Draft
**Input**: User description: "bench.yml sets LBUG_BUILD_FROM_SOURCE=1, which was removed elsewhere as broken — the workflow has never run"

## Background

`.github/workflows/bench.yml` (the on-demand "Perf Benchmarks" workflow) sets `LBUG_BUILD_FROM_SOURCE: "1"` in its `bench` job. That flag forces lbug's `build.rs` to take the cmake source-build path instead of downloading the prebuilt bundle. Since lbug 0.17.0, the prebuilt archive is a self-contained fat bundle that merges every third-party static archive (antlr4, fastpfor, parquet, zstd, yyjson, …) into `liblbug.a` via `BundleStaticLibrary.cmake`. A source build sets `link_bundled_deps=true`, which links both the fat archive *and* the individual third-party archives simultaneously, producing roughly 7399 duplicate-symbol linker errors (upstream bug: `LadybugDB/ladybug-rust#18`).

This exact failure mode is why the flag was already removed from every other workflow and config in this repo:

- `.cargo/config.toml:13-19` — documents the removal and the 0.17.0 fat-bundle change
- `.github/workflows/ci.yml:60-61` — documents the removal, notes the flag is "no longer needed"
- `.github/workflows/release.yml:129-130` — documents the removal, notes the same upstream bug

`bench.yml` was authored one day *after* the flag was removed from `ci.yml` and `release.yml` (2026-06-01 removal, 2026-06-02 `bench.yml` creation), and evidently inherited the flag from a stale template rather than the corrected version. Because `bench.yml` is a `workflow_dispatch`-only workflow (see `CLAUDE.md`'s "Running performance benchmarks" section: `gh workflow run bench.yml`), it has never actually been triggered — `gh run list --workflow=bench.yml` returns no runs. The break has therefore gone unnoticed since creation.

This matters because `CLAUDE.md` instructs contributors to run `gh workflow run bench.yml` directly. The first person to follow that instruction would hit an opaque linker failure with no visible connection to the actual cause (a stale env var), costing debugging time on a problem the project has already solved twice elsewhere.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Contributor runs the benchmark workflow successfully (Priority: P1)

A contributor follows `CLAUDE.md`'s instructions and runs `gh workflow run bench.yml` to measure dedup performance (R-003) after a change. The workflow builds against the prebuilt lbug bundle (as every other workflow does), runs the 1k/10k/50k criterion benches, and uploads both artifacts described in `CLAUDE.md`.

**Why this priority**: This is the entire point of the issue — the workflow is documented as usable but has never successfully run. Without this fix, the documented benchmarking path is broken for every future user.

**Independent Test**: Trigger `gh workflow run bench.yml` after the fix lands and confirm the run completes successfully, producing `bench-results-<sha>` and `criterion-html-<sha>` artifacts.

**Acceptance Scenarios**:

1. **Given** the `bench` job no longer sets `LBUG_BUILD_FROM_SOURCE`, **When** `gh workflow run bench.yml` is triggered, **Then** the job links against the prebuilt lbug bundle without duplicate-symbol errors and completes successfully.
2. **Given** a successful run, **When** the run finishes, **Then** both `bench-results-<sha>` (text) and `criterion-html-<sha>` (HTML reports) artifacts are uploaded, matching `CLAUDE.md`'s description.
3. **Given** a successful run, **When** the correctness gate step executes, **Then** the `dedup_overlap_check` (R-003) behavior is unaffected by this change (that check runs in the `test` job of `ci.yml`, not in `bench.yml`, and is out of scope for this fix).

### Edge Cases

- If, after removing the flag, the benchmark workflow fails for a *different* reason (i.e., a source build turns out to be genuinely required — e.g., for symbol availability that the prebuilt bundle doesn't expose), the fix must not be applied blindly. Instead, this issue should be updated to record why a source build is needed and link the upstream blocker (`LadybugDB/ladybug-rust#18`) as a tracked dependency, and the flag removal should be reverted or reconsidered.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The `env:` block in `.github/workflows/bench.yml` (lines 19-20) setting `LBUG_BUILD_FROM_SOURCE: "1"` MUST be removed from the `bench` job, so the job builds against the prebuilt lbug bundle by default, consistent with `ci.yml` and `release.yml`.
- **FR-002**: After the flag is removed, `gh workflow run bench.yml` MUST be triggered at least once to confirm the workflow completes successfully end-to-end — this is not optional, since "never executed" is precisely how this defect went unnoticed.
- **FR-003**: A successful run MUST produce both artifacts described in `CLAUDE.md`'s "Running performance benchmarks" section: `bench-results-<sha>` and `criterion-html-<sha>`.
- **FR-004**: The change MUST NOT alter the behavior of the `dedup_overlap_check` (R-003) correctness gate, which runs in `ci.yml`'s `test` job and is unrelated to `bench.yml`.
- **FR-005**: If, contrary to expectation, a source build proves genuinely necessary for the benchmarks, the fix MUST instead record that finding in this issue (with rationale) and link `LadybugDB/ladybug-rust#18` as an upstream blocker, rather than silently keeping or removing the flag.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `.github/workflows/bench.yml` no longer sets `LBUG_BUILD_FROM_SOURCE` in the `bench` job.
- **SC-002**: A `gh workflow run bench.yml` invocation, triggered after the fix, completes with a successful (green) run.
- **SC-003**: That successful run's artifact list includes both `bench-results-<sha>` and `criterion-html-<sha>`.

## Assumptions

- The criterion benchmarks in `bench.yml` measure Rust-side dedup code (R-003 correctness/perf), not lbug internals, so they have no inherent need for a source build of lbug — consistent with `ci.yml` and `release.yml` both profiling/testing fine against the prebuilt bundle.
- No other undocumented reason for the source-build flag exists in `bench.yml`'s history; the flag is a copy-paste leftover from before the 2026-06-01 removals, not an intentional divergence.

## Out of Scope

- Enabling the commented-out nightly `schedule:` trigger in `bench.yml`.
- Any changes to the `dedup_overlap_check` (R-003) gate itself, which lives in `ci.yml`.
- Adding the `build-lbug` cache-warming job pattern from `ci.yml` to `bench.yml` (not requested by this issue; `bench.yml` has no such job today and none is required simply to fix the source-build flag).

## Source References

- `.github/workflows/bench.yml:19-20` — the flag to remove
- `.github/workflows/ci.yml:60-61` — prior removal + rationale
- `.github/workflows/release.yml:129-130` — prior removal + rationale
- `.cargo/config.toml:13-19` — fullest documentation of the 0.17.0 fat-bundle change and the duplicate-symbol failure mode
- Upstream blocker (if a source build is ever found necessary): `LadybugDB/ladybug-rust#18`
- Related: issue #256 (corrected `CLAUDE.md`'s description of the lbug build, which had cited this workflow as precedent for a working `LBUG_BUILD_FROM_SOURCE` opt-in)
