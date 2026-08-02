# Feature Specification: the 15–18 minute release test suite is the root cause behind the headless-stall class

**Feature Branch**: `fabrik/issue-316`
**Created**: 2026-08-02
**Status**: Draft
**Input**: User description: "CI's `test` job runs `cargo test --release` and measures 15–18 minutes even on a warm cache. An agent's foreground call is capped at ~10 minutes. That mismatch is the root cause behind a recurring failure class: a stage worker that decides it needs the release suite cannot run it in the foreground, so it backgrounds it, then ends its turn awaiting a notification that never fires headlessly. The stage closes incomplete, retries, and the issue auto-pauses."

## Background

CI's `test` job runs `cargo test --release` and measures 15–18 minutes even on a warm cache. An agent's foreground call is capped at ~10 minutes. That mismatch is the root cause behind a recurring failure class: a stage worker that decides it needs the release suite **cannot** run it in the foreground, so it backgrounds it, then ends its turn awaiting a notification that never fires headlessly. The stage closes incomplete, retries, and the issue auto-pauses.

Measured in `.fabrik/logs/`: **75 `ScheduleWakeup` invocations** across this repo's stage logs, ~33 of them waiting on a local `cargo` run. Casualties include #208, #190, #219, #212, #236, #283, #297.

Guidance and engine changes address the *symptom* — #315 corrects `CLAUDE.md`'s long-task guidance, handarbeit/fabrik#1345 asks for the deferral tool to be withdrawn. **This issue addresses the cause.** If the common-path check fit inside the call budget, the judgement call disappears and the failure mode stops being reachable.

### Where the time goes

`CLAUDE.md` already records a diagnosis: the 15–18 minutes is **Rust compilation and the test suite, not C++** — lbug arrives as a prebuilt fat bundle, not something built from source on every run. Specifically:

- `Cargo.toml`'s `[profile.dev]` sets `debug = "line-tables-only"` because, per its own comment, linking integration-test binaries against the full lbug static archive (+ third-party deps) with full debug info blows the 7 GB Ubuntu CI runner's RAM during `ld`.
- `.cargo/config.toml` sets `RUST_TEST_THREADS = "4"` because lbug mmaps an 8 TB virtual region per `Db::open()`, and default parallelism exhausts a machine's virtual-memory ceiling.

So the cost is believed to concentrate in a small number of release-linked integration-test binaries, not in the library or the bulk of unit tests — but this needs measuring before designing.

**A note on the "six" figure.** Both `Cargo.toml`'s comment and `.github/workflows/ci.yml`'s comment describe "the six integration-test binaries" driving the debug-link OOM that motivates building in release. As of this spec, the workspace has **50 integration test files across three crates** (`crates/core/tests`: 37, `crates/service/tests`: 11, `crates/eval/tests`: 2), each of which compiles as its own separate binary under Cargo's test-target model. The "six" figure is stale relative to the current suite — it likely reflected an earlier, smaller set of tests that first triggered the OOM, or a specific subset (e.g. the largest/slowest linkers) rather than the full count. **FR-001 must establish the true current figure and its composition (which crate(s), which specific targets) rather than carrying "six" forward as fact.** This spec deliberately does not assume which targets are slow; that is measurement work for Research.

### Related in-flight work

- **#298** ("`real-corpus-e2e` failed 24 consecutive runs with no signal to anyone", open as of this spec) is building a post-merge failure-signalling mechanism for non-gating workflows. FR-003's "post-merge gate with visible failure signalling" option assumes that mechanism (or an equivalent) exists — if #298 has not merged by the time this issue reaches Plan, the Plan stage must either sequence behind it, use the "separate required check" alternative instead, or design a self-contained signalling mechanism scoped to this issue. This spec does not mandate which.
- **#315** (open as of this spec) is also editing `CLAUDE.md`'s long-task/local-gate guidance. Both issues touch the same section; whichever merges first, the other should rebase rather than silently conflict. This is a merge-sequencing note, not a scope change for either issue.

## User Scenarios & Testing

### User Story 1 - The common-path check fits the budget (Priority: P1)

An agent working a stage that touches library code runs the documented local pre-commit gate. Today that gate is ambiguous in practice — `CLAUDE.md` tells the agent not to run `cargo test --release` locally and to rely on debug-mode `cargo test`, but the underlying release-suite duration problem this creates for *CI* itself, and for any worker that genuinely needs release-mode coverage before pushing, remains unaddressed. This story delivers a local gate that reliably fits inside the ~10-minute foreground budget, so an agent following it never has to make the judgment call that leads to backgrounding a command and stalling.

**Why this priority**: This is the actual mechanism that removes the failure class. Every casualty issue (#208, #190, #219, #212, #236, #283, #297) traces back to a worker needing more test coverage than the debug-mode local gate provides and reaching for the release suite instead.

**Independent Test**: On a warm `target/` cache, run the documented local gate from a clean checkout and time it; confirm it completes in under 10 minutes without invoking `cargo test --release` or any other command that requires backgrounding.

**Acceptance Scenarios**:

1. **Given** a change touching library code, **When** an agent runs the documented local gate, **Then** it completes inside the foreground call budget with no backgrounding.
2. **Given** the same change, **When** CI runs, **Then** whatever was excluded from the local gate is still covered by CI before merge.

---

### User Story 2 - CI feedback is faster for everyone (Priority: P2)

A contributor or maintainer opens a PR. Today, the required `test (ubuntu-latest)` check takes 15–18 minutes end to end before any signal is available. This story delivers either a faster required check overall, or a fast subset that reports first with the remainder as a separate, still-required-before-merge check — so feedback on common changes doesn't wait on the full release-linked suite.

**Why this priority**: This is a materially better experience for both human and agent contributors, but it is a consequence of solving User Story 1's underlying split, not an independent mechanism — hence P2.

**Independent Test**: Open a PR touching only library code (no integration-test-relevant paths) and observe that CI reports a meaningful result materially sooner than the current 15–18 minute baseline.

**Acceptance Scenarios**:

1. **Given** a PR, **When** CI runs, **Then** the required check reports materially sooner than 15–18 minutes, or reports a fast subset first with the slow remainder as a separate check.

---

### Edge Cases

- Worktrees do not share `target/`, so a fresh Fabrik worktree pays a full cold rebuild regardless of how the suite is split — the local gate's duration must be stated for the cold case too, since that is what workers actually experience.
- `RUST_TEST_THREADS = "4"` and the 8 TB mmap per `Db::open()` bound parallelism; a split that increases concurrent `Db::open()` calls (e.g. by running previously-sequential release binaries in parallel jobs) could regress rather than help.
- The integration tests that require release-mode linking may not have equivalent debug-mode code paths — a debug-profile-only split may silently stop exercising code the release suite currently covers.
- The true count and composition of "slow" targets is unknown at spec time (see the "six" note above) — any FR-004 target-layout property must be derived from measurement, not assumed from the stale figure.

## Requirements

### Functional Requirements

- **FR-001**: Measure and report where the 15–18 minutes actually goes — compile vs link vs execution — broken down per test target, across all three crates (`crates/core`, `crates/service`, `crates/eval`), not just the previously-assumed six. Design follows the measurement; do not restructure on the current hypothesis alone.
- **FR-002**: Identify a **common-path check that completes inside the ~10-minute foreground budget** and state precisely what it does and does not cover.
- **FR-003**: Any split MUST NOT reduce what is verified before merge. Slow targets move to a separate required check or a post-merge gate with visible failure signalling (see #298), not into nothing.
- **FR-004**: If integration tests are separated into fast/slow groups, the separation MUST be a property of the target layout (e.g. directory structure, a Cargo feature, a naming convention enforced by tooling) rather than a hand-maintained list that drifts as tests are added.
- **FR-005**: Document the resulting contract in `CLAUDE.md` and `CONTRIBUTING.md` — what an agent or contributor runs locally, what CI owns, and the expected duration of each. `CONTRIBUTING.md`'s existing "Pre-commit gate" section (currently: `cargo fmt --all && cargo test && cargo clippy --all-targets -- -D warnings`) must be updated if the commands change.

## Success Criteria

### Measurable Outcomes

- **SC-001**: The documented local gate completes in under 10 minutes on a warm cache, measured and recorded.
- **SC-002**: Total pre-merge coverage is unchanged or greater — demonstrated by enumerating targets before and after.
- **SC-003**: A stage worker following the documented gate never needs to background a command.

## Assumptions

- The "six integration-test binaries" figure currently documented in `Cargo.toml` and `.github/workflows/ci.yml` comments is stale; the actual current count (50 test files across `crates/core`, `crates/service`, `crates/eval`) supersedes it, and Research/Plan should treat that figure as the starting point, subject to further change as tests are added.
- `#298`'s post-merge failure-signalling mechanism, if not yet merged when this issue reaches Plan, is a soft dependency for the "post-merge gate" branch of FR-003 only — the "separate required check" branch does not require it. Plan should choose based on #298's state at that time rather than blocking on it.
- `#315`'s `CLAUDE.md` edits and this issue's FR-005 documentation updates both touch the long-task guidance section; whichever lands second rebases against the other.
- This issue is scoped to the test-execution split and its documentation; it does not itself change the `ScheduleWakeup` tool's availability or Fabrik's engine behavior (that is handarbeit/fabrik#1345's scope).

## Out of Scope

- `CLAUDE.md` guidance wording for the deferral judgment call itself (#315) and withdrawing/gating the `ScheduleWakeup` tool at the Fabrik engine level (handarbeit/fabrik#1345). Both mitigate the symptom; this issue removes the cause.
- Designing the post-merge failure-signalling mechanism itself — that is #298's scope; this issue only depends on its existence for one branch of FR-003.

## Source References

- `CLAUDE.md` — the 10-minute budget note, the `[profile.dev]` rationale, and the `RUST_TEST_THREADS` note
- `Cargo.toml` (`[profile.dev]` comment), `.cargo/config.toml` (`RUST_TEST_THREADS` comment), `.github/workflows/ci.yml` (`test` job)
- `CONTRIBUTING.md` — existing "Pre-commit gate" section, to be updated per FR-005
- #315, handarbeit/fabrik#1345 — the symptom-level mitigations
- #298 — post-merge failure signalling, which FR-003 depends on if slow targets move off the PR gate
