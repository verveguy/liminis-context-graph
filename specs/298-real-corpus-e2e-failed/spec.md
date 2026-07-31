# Feature Specification: `real-corpus-e2e` failed 24 consecutive runs over 4 days with no signal to anyone

**Feature Branch**: `fabrik/issue-298`
**Created**: 2026-07-30
**Status**: Draft
**Input**: User description: "`real-corpus-e2e` runs on every push to `main`. It has failed every run since 2026-07-26 — 24 consecutive failures spanning four days and roughly a dozen merges. The last green run was `5259bfb7` at 07-26 14:14; the first red was `ed90d489` at 20:02 the same evening. `mcp_real_corpus_admin_lifecycle_e2e.rs` was added at `db27471` earlier that afternoon as part of #236, so the suite went red essentially when it was introduced and has never been green on `main`. Nobody noticed until the failure was found by hand while smoke-testing the v0.11.0 release, after the tag had been pushed. The underlying test failure is real but minor (a status flag; filed separately). The process defect is the serious one: this workflow exists specifically to catch regressions post-merge before they reach a release, and it did catch one — it just had no way to tell anyone."

## Background

`real-corpus-e2e` runs on every push to `main`. It has failed **every run since 2026-07-26** — 24 consecutive failures spanning four days and roughly a dozen merges. The last green run was `5259bfb7` at 07-26 14:14; the first red was `ed90d489` at 20:02 the same evening. `mcp_real_corpus_admin_lifecycle_e2e.rs` was added at `db27471` earlier that afternoon as part of #236, so the suite went red essentially when it was introduced and has never been green on `main`.

Nobody noticed until the failure was found by hand while smoke-testing the `v0.11.0` release, after the tag had been pushed.

The underlying test failure is real but minor (a status flag; filed separately — see Out of Scope). **The process defect is the serious one:** this workflow exists specifically to catch regressions post-merge before they reach a release, and it did catch one — it just had no way to tell anyone. Its own header comment states the intent:

> this workflow runs the ignored tests on every push to main (post-merge verification, before a regression reaches a release)

That guarantee is currently vacuous. A red `real-corpus-e2e` and a green one are indistinguishable to every human and agent working in this repo.

This is the second instance of the same shape found in one day. PR #294 corrected documentation that had drifted from the code across 45 PRs — six undocumented environment variables, four undocumented telemetry events, two telemetry events documented as "not yet emitted" that had been emitting for weeks. Same root cause: a signal with no watcher.

### Current workflow inventory (as of this spec)

For context going into Research/Plan, the repo's non-PR-gate workflows as they exist today:

- **`real-corpus-e2e.yml`** — triggers on `push: branches: [main]` and `workflow_dispatch`. Runs automatically on every merge to `main`. This is the workflow that has been silently red for 24 runs.
- **`bench.yml`** — `workflow_dispatch` only; its nightly `schedule` trigger is present but commented out. Does not currently run automatically against `main`.
- **`eval.yml`** — `workflow_dispatch` only. Does not currently run automatically against `main`.
- **`swift.yml`** — triggers exist for `push` (path-filtered to `native/local-inference/**` and its own workflow file) and `pull_request`, but the workflow's only job carries `if: false` and is currently dormant (waiting on GitHub's `macos-latest` runner image to gain macOS 26 / Swift 6.2 support — see the workflow's own header comment). It does not execute today even when its trigger paths change, so it produces no signal to cover and is out of scope until re-enabled; the mechanism should pick it up as part of coverage at that point.
- **`claude-review.yml`** — triggers on `pull_request` events and `workflow_dispatch`; does not run against `main` directly (it targets PR branches).

Per GitHub branch protection on `main` (`required_status_checks.contexts`), the only currently-required PR check is `test (ubuntu-latest)` (from `ci.yml`). Every workflow above is therefore "non-gating" in the sense FR-006 means: none of them block a merge today, and this issue's mechanism must not change that.

This inventory is a starting point, not the final word — confirming it (and enumerating exactly what the delivered mechanism covers) is part of FR-004's scope, to be stated explicitly in the implementing PR.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A post-merge failure is visible without being looked for (Priority: P1)

A workflow that runs automatically after merge to `main` (starting with `real-corpus-e2e`) fails. Today that failure is visible only to someone who opens the Actions tab and checks — as this issue demonstrates, that can go unnoticed for days and multiple releases. Instead, the failure should produce a durable artifact that stays visible until it's resolved, without anyone having to go looking for it.

**Why this priority**: This is the core defect the issue reports — a real regression was caught and then effectively discarded because nothing surfaced it. Without this, every other part of the fix is moot.

**Independent Test**: Force a failure in `real-corpus-e2e` on a branch/PR built for this purpose (or by temporarily breaking the workflow on a test push to a throwaway ref), observe that a durable artifact (issue, board item, or equivalent) is created, then fix the failure and observe the artifact resolves.

**Acceptance Scenarios**:

1. **Given** a push to `main` whose `real-corpus-e2e` run fails, **When** the run completes, **Then** a durable, visible artifact is created — an issue, a board item, or an equivalent notification — without anyone checking the Actions tab.
2. **Given** a subsequent push that fails the same way, **When** it completes, **Then** the existing artifact is updated rather than duplicated, so 24 failures do not produce 24 issues.
3. **Given** a push that fixes the failure, **When** the run succeeds, **Then** the artifact is closed or resolved automatically.

---

### User Story 2 - Release preparation cannot silently proceed over a red post-merge suite (Priority: P1)

Someone preparing a release currently has no prompt to check whether `real-corpus-e2e` (or the other non-gating workflows) are green on `main`. The `v0.11.0` release shipped with a suite that had been red for 24 runs, discovered only after the tag was pushed. Release prep should surface that state as a matter of course.

**Why this priority**: This is the concrete, high-stakes moment where the missing signal actually caused harm — a release shipped over a known-broken post-merge check without anyone knowing it was broken.

**Independent Test**: Follow the release runbook in `README.md` with `real-corpus-e2e` intentionally left red on `main`; confirm the runbook's pre-flight step surfaces that state before the release proceeds.

**Acceptance Scenarios**:

1. **Given** the most recent `real-corpus-e2e` run on `main` is failing, **When** a release is prepared, **Then** that state is surfaced as part of preparing the release rather than discovered by manual inspection afterwards.

---

### User Story 3 - The same gap is closed for the repo's other non-gating workflows (Priority: P2)

`real-corpus-e2e` is the workflow that happened to fail this time, but the underlying gap — a workflow that runs outside the PR gate with no failure signal — applies equally to `bench`, `eval`, and any future workflow set up the same way. Fixing only `real-corpus-e2e` would leave the same defect in place for its siblings.

**Why this priority**: Important for closing the gap completely, but strictly lower urgency than getting the one workflow that's *currently* silently red (User Story 1) visible, and lower than protecting the next release (User Story 2).

**Independent Test**: Force a failure in `bench` or `eval` via `workflow_dispatch` against `main` and confirm the same artifact mechanism fires as in User Story 1.

**Acceptance Scenarios**:

1. **Given** the set of workflows that run outside the PR gate (`real-corpus-e2e`, `bench`, `eval`, and any other non-required workflow), **When** one fails on `main`, **Then** it produces the same visible signal.

---

### Edge Cases

- A run that fails for infrastructure reasons (runner outage, cache corruption, network) should not be indistinguishable from a genuine regression — consider capturing the failing job name and first failing assertion in the artifact so triage does not require opening the run.
- `concurrency: cancel-in-progress: true` (set on `real-corpus-e2e.yml`) means a superseded run reports as `cancelled`; a cancelled run must not be treated as a failure.
- If the notification target is a GitHub issue, it needs an owner and a label convention so it does not sit unassigned the way the underlying failure did.
- `bench.yml` and `eval.yml` do not currently run automatically on push to `main` — they are `workflow_dispatch`-only. For these, "fails on `main`" means a manual dispatch run against `main` fails; the mechanism should still apply in that case even though the trigger is manual rather than automatic.
- A workflow that has never had a successful run on `main` (as `real-corpus-e2e` currently has not) should not be treated differently from one that regressed from green — both need the same visible artifact.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A failing `real-corpus-e2e` run on `main` MUST produce a durable, human-visible artifact automatically.
- **FR-002**: Repeat failures MUST update the existing artifact rather than create duplicates.
- **FR-003**: A subsequent success MUST resolve the artifact automatically, so a stale "broken" signal cannot itself become noise nobody trusts.
- **FR-004**: The mechanism MUST cover every non-gating workflow that runs on `main`, not `real-corpus-e2e` alone. Enumerate them in the PR.
- **FR-005**: The release runbook in `README.md` MUST gain a pre-flight step: confirm the latest `main` run of each non-gating workflow is green, or record why the release proceeds anyway.
- **FR-006**: The mechanism MUST NOT make these suites required PR checks. They were deliberately moved off the PR path for cost reasons (see the header comments in `real-corpus-e2e.yml` and `bench.yml`); this issue is about visibility, not about re-gating.

### Key Entities

- **Failure artifact**: The durable, human-visible record created when a non-gating workflow fails on `main` (e.g., a GitHub issue). Identified per-workflow so repeat failures of the same workflow update the same artifact rather than creating new ones. Carries enough detail (failing job name, first failing assertion) to support triage without opening the Actions run. Has an owner and label convention (per Edge Cases) so it doesn't go unassigned. Resolved automatically on the next passing run of that workflow.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Forcing a `real-corpus-e2e` failure on a branch produces the artifact described in FR-001 within one run.
- **SC-002**: A second failure updates rather than duplicates it.
- **SC-003**: A passing run resolves it.
- **SC-004**: `README.md`'s release runbook lists the non-gating-workflow check as an explicit pre-flight step.
- **SC-005**: The PR enumerates every non-gating workflow on `main` and states whether it is covered.

## Assumptions

- GitHub Actions' native notifications are insufficient here, since they did not surface 24 consecutive failures to anyone actively working in the repo.
- "Non-gating" / "outside the PR gate" is defined by GitHub branch protection's required status checks on `main`. As of this spec, the only required check is `test (ubuntu-latest)` (from `ci.yml`); every other workflow — `real-corpus-e2e.yml`, `bench.yml`, `eval.yml`, `swift.yml`, `claude-review.yml` — is non-gating by this definition. The PR's FR-004 enumeration is expected to confirm or update this list, not take it as final.
- `swift.yml`'s only job is currently disabled (`if: false`, pending a GitHub Actions macOS runner image update per its own header comment), so it produces no runs to monitor today and is not part of the mechanism's initial coverage. It should be added once re-enabled — this is a follow-up, not part of this issue's delivered scope.
- The specific notification mechanism (GitHub issue vs. board item vs. some other durable artifact) is a technical decision left to Research/Plan, not fixed by this spec — the Edge Cases note that *if* a GitHub issue is chosen, it needs an owner and label convention, without mandating that choice.
- "Records why the release proceeds anyway" (FR-005) means the runbook step is a check-and-decide gate for the human running it, not an automated release blocker — consistent with FR-006's constraint that these suites stay off the enforced gate.

## Out of Scope

- Fixing the `indices_built` assertion that is currently red in `mcp_real_corpus_admin_lifecycle_e2e.rs` — filed separately.
- Making the expensive suites (`real-corpus-e2e`, `bench`, `eval`) part of the required PR gate.

## Source References

- `.github/workflows/real-corpus-e2e.yml` — the workflow and its stated post-merge-verification intent.
- `.github/workflows/bench.yml`, `.github/workflows/eval.yml` — the other non-gating workflows in User Story 3's scope.
- `README.md` — "Release runbook (maintainers)" section, target of FR-005.
- PR #294 — the documentation-drift audit, the same signal-without-a-watcher failure in a different medium.
- `db27471` (#236) — added the suite that has never been green on `main`.
