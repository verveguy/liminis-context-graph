# Feature Specification: run `real-corpus-e2e` on the PR path as a non-required check

**Feature Branch**: `fabrik/issue-328`
**Created**: 2026-08-03
**Status**: Draft
**Input**: User description: "`real-corpus-e2e` runs only on push to `main` and `workflow_dispatch`. It never runs on PRs. That is why a regression introduced by `f51c40c` on 2026-07-26 went unnoticed for 38 consecutive runs across a week (#317, #325), and why every change merged in that window — including the entire 0.12.0 milestone — landed without e2e verification. Add it to the PR path as separate parallel jobs, reporting but not gating, without undoing #322's docs-only fast path."

## Background

`real-corpus-e2e` runs only on push to `main` and `workflow_dispatch`. It never runs on PRs. That is why a regression introduced by `f51c40c` on 2026-07-26 went unnoticed for 38 consecutive runs across a week (#317, #325), and why every change merged in that window — including the entire 0.12.0 milestone — landed without e2e verification.

It is also why the fix for that regression (#325 / PR #327) currently cannot be verified before merging: the suite that would prove it works does not run against the branch.

The workflow's own header explains the original exclusion:

> adding that to every PR's ~16-17 min `cargo test --release` job would be a material tax

That reasoning is about adding these tests **to** the existing job, sequentially. As **separate parallel jobs** the premise does not hold.

### Measured timing

From run 30776173198 on `main`:

| job | duration |
|---|---|
| `real_corpus_e2e` | 4m26s |
| `mcp_real_corpus_e2e` | 4m23s |
| `mcp_real_corpus_admin_data_e2e` | 4m50s |
| `mcp_real_corpus_mutation_e2e` | 6m09s |
| `mcp_real_corpus_admin_lifecycle_e2e` | 3m42s |
| **suite wall clock** | **8m46s** |

The required `test (ubuntu-latest)` check measured **17m45s – 19m48s** across four PRs the same evening. The e2e suite therefore completes roughly **10 minutes before** the check that already gates every PR. Expected added latency to a PR's mergeable time: **zero**.

The repo is public, so Actions minutes are free; there is no billing argument either.

### Risks this must respect

**1. The lbug build-cache race.** `.github/workflows/ci.yml`'s trigger comment documents that concurrent runs sharing the cache key intermittently link against a half-written archive and fail with `duplicate symbol: yyjson_*` / `antlr4::*` — spurious failures that previously paused Fabrik. Five additional release-building jobs per PR multiplies the contenders for that cache. This is the only path to a real timing regression: a spurious failure costs a full 18–20 minute re-run, not 9 minutes.

**2. Runner concurrency.** A PR occupies roughly 2 heavy job slots today; this makes it ~7. In the measured run, `mcp_real_corpus_admin_lifecycle_e2e` started **5 minutes after** the other four because it queued — inside a single run, at current load. There is ~10 minutes of slack before queueing eats the margin.

**3. It must not undo #322.** #322 (ADR-0322, merged 2026-08-02) makes docs-only PRs skip the 18-minute `test` job, so they now merge in well under a minute. `real-corpus-e2e.yml` is a **separate workflow** and #322's classification does not cover it. Adding an unconditional `pull_request` trigger would take a docs-only PR from ~30 seconds back to 6–9 minutes, handing back most of what #322 just won.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A code PR gets e2e signal before merge (Priority: P1)

A contributor opens a PR that touches Rust source, manifests, the lockfile, `.cargo/**`, `build.rs`, or a CI workflow file. All five `real-corpus-e2e` jobs run against the PR head in parallel and report their results directly on the PR, alongside the existing `test (ubuntu-latest)` check — without blocking the PR from merging if one of them fails.

**Why this priority**: This is the core complaint in the issue — the absence of this signal on PRs is why a regression shipped silently for 38 runs across a week (#317, #325), and why #325's own fix (PR #327) currently cannot be verified before merge.

**Independent Test**: Can be fully tested by opening a real PR that touches a `.rs` file, observing that all five e2e jobs run and report a conclusion on the PR (pass or fail), and confirming a failing e2e job does not block the merge button — delivers "code PRs get e2e signal" on its own, independent of the docs-only fast-path behavior in Story 2.

**Acceptance Scenarios**:

1. **Given** a PR touching Rust or build files, **When** CI runs, **Then** all five e2e jobs run against the PR head and report their results on the PR.
2. **Given** that PR, **When** an e2e job fails, **Then** the PR still *can* be merged — the check reports but does not gate (see FR-002).

---

### User Story 2 - Docs-only PRs stay fast (Priority: P1)

A contributor opens a PR that changes only documentation, spec, or other non-code files. The e2e jobs are skipped entirely, and the PR reaches a mergeable state exactly as fast as it did after #322 shipped — the addition of these jobs to the PR path must not claw back any of that win.

**Why this priority**: Equal in priority to Story 1 — the e2e signal on code PRs must not come at the cost of #322's docs-only fast path, which this repo shipped one day before this issue was filed specifically to fix a real, measured regression (PR #320 blocked 15+ minutes on zero-Rust changes).

**Independent Test**: Can be fully tested by opening a PR that touches only a markdown file and confirming the e2e jobs show a skipped (not pending, not run) conclusion, with the PR's overall time-to-mergeable unchanged from its post-#322 baseline — delivers "docs-only PRs stay fast" independent of whether Story 1's code path is exercised in the same test.

**Acceptance Scenarios**:

1. **Given** a docs-only PR, **When** CI runs, **Then** the e2e jobs are skipped, and the PR's time-to-mergeable is unchanged from its post-#322 behaviour.

---

### User Story 3 - The post-merge signal is preserved (Priority: P1)

A merge to `main` still triggers the full `real-corpus-e2e` suite exactly as it does today, and a failure still files or updates the tracking issue that #298's notifier maintains — adding a PR-path trigger must be strictly additive to the existing post-merge behavior, not a replacement for it.

**Why this priority**: Equal in priority to Stories 1–2 — regressing the post-merge safety net while adding a PR-path one would trade one blind spot for another, and #298 exists precisely because this suite going silent for days was already a real incident.

**Independent Test**: Can be fully tested by merging a PR to `main` and confirming `real-corpus-e2e` runs post-merge exactly as before, then (separately, without needing a real failure) verifying `ci-failure-notify.yml`'s `workflow_run` listener is unaffected by the new trigger — delivers "post-merge signal preserved" independent of Stories 1–2.

**Acceptance Scenarios**:

1. **Given** a merge to `main`, **When** CI runs, **Then** `real-corpus-e2e` still runs exactly as it does today, and #298's failure notifier still files/updates its tracking issue.

---

### Edge Cases

- Fork PRs cannot access secrets or caches the same way; state whether e2e can run on them at all, and fail loudly rather than silently skipping if not.
- A PR that changes the e2e fixture (`crates/core/tests/fixtures/real_corpus_wal/**`) must run the suite even if it otherwise looks docs-only.
- Fabrik's internal merge train (`merge_train: on` in `.fabrik/config.yaml`, ADR-059 — distinct from GitHub's native merge queue feature, which this repo does not have enabled; see Assumptions) stages ready `fabrik:yolo` PRs against a trial branch. Confirm the e2e trigger behaves sensibly there.
- `cancel-in-progress: true` plus PR triggers means a rapid push sequence cancels intermediate runs — acceptable for a non-required check, but confirm it does not leave a PR showing a permanently "cancelled" e2e status that reads as a failure.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Add a `pull_request` trigger to `real-corpus-e2e.yml`, keeping the existing `push: branches: [main]` and `workflow_dispatch` triggers intact.
- **FR-002**: The e2e jobs MUST NOT be added to `main`'s required status checks in this change. They report on PRs without gating merges. Promotion to required is a separate, later decision — see FR-007.
- **FR-003**: The e2e jobs MUST be skipped on PRs whose diff does not warrant them, **reusing #322's existing path classification** (the `changes` job in `ci.yml`) rather than duplicating it. A second, drifting copy of the docs/code classifier is explicitly not acceptable (this repeats #322's own FR-006).
- **FR-004**: The `concurrency` group MUST be scoped so a PR's runs cancel that PR's superseded runs without cancelling `main`'s post-merge run. The current group is `real-corpus-e2e-${{ github.ref }}`; verify it behaves correctly once PR refs enter the picture.
- **FR-005**: Verify and report whether adding five release-building jobs per PR aggravates the documented lbug cache race. If contention is observed, state the mitigation (distinct cache keys, job serialisation, or reusing artifacts from the `test` job) rather than leaving it to chance.
- **FR-006**: Measure and report, on a real PR, the delta to time-to-mergeable versus the current baseline of 17m45s–19m48s. The expectation is zero; the PR must show it rather than assert it.
- **FR-007**: Document the promotion criteria — what would justify making these required checks later (e.g. N days with no spurious failures and no cache-race incidents) — so the trial has a defined end rather than drifting indefinitely as a permanently non-required check nobody reads.

### Key Entities

Not applicable — this feature changes CI workflow trigger/job logic and does not introduce or modify any data entities.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A code-touching PR shows all five e2e jobs reporting on the PR.
- **SC-002**: A docs-only PR shows the e2e jobs skipped and reaches mergeable in under two minutes.
- **SC-003**: A PR's time-to-mergeable is unchanged within noise from the pre-change baseline, demonstrated with measurements.
- **SC-004**: A merge to `main` still triggers the suite and, on failure, still updates #298's tracking issue.
- **SC-005**: The path classification exists in exactly one place, shared with `ci.yml`.

## Assumptions

- "Reusing #322's existing path classification" (FR-003, SC-005) means the `changes` job's `code_changed` output in `ci.yml`, or an equivalent single source of truth derived from it — the exact mechanism (e.g. a reusable/callable workflow, a composite action, or duplicating just the `changes` job definition into a shared location both workflows call) is a Research/Plan-stage implementation decision, not a product decision open for negotiation here. What is fixed: there must be exactly one classification, not two independently maintained deny-lists.
- `merge_train: on` (referenced in Edge Cases) is Fabrik's own internal merge-train feature (`.fabrik/config.yaml`, ADR-059), which stages ready `fabrik:yolo` PRs against a trial branch. It is distinct from GitHub's native merge queue product. This repository does not have GitHub's merge queue enabled (confirmed via `gh api repos/.../branches/main/protection` and `gh api repos/.../rulesets`, both showing no merge-queue-specific configuration) — the same fact #322's spec recorded. Research/Plan must confirm how a Fabrik merge-train trial branch's ref shape interacts with the `pull_request` trigger and the concurrency group in FR-004.
- `ci-failure-notify.yml` (#298) already guards its `notify` job with `if: github.event.workflow_run.head_branch == 'main'`, so a PR-path run of `real-corpus-e2e` failing does not, by itself, file or update a tracking issue — that behavior is scoped to post-merge runs on `main` today and this issue does not need to change it. SC-004 is about confirming this existing guard continues to hold, not adding new logic.
- Fork PRs are addressed by the Edge Cases entry (state whether e2e can run on them, fail loudly if not) rather than by a Functional Requirement, because the answer depends on how secrets/cache access actually behaves for a fork-originated `pull_request` event — an investigation for Research, not a product decision to prescribe here.

## Out of Scope

- Making these checks required (FR-007 defines when to revisit).
- Reducing the suite's runtime.
- Fixing #325.

## Source References

- #317 / #325 — the regression that went unnoticed for 38 runs because this suite never ran on PRs
- #322 / ADR-0322 — the docs-only fast path whose classification FR-003 must reuse
- #298 — the post-merge failure notifier that FR-003/SC-004 must not break
- `.github/workflows/ci.yml` — the trigger comment documenting the lbug cache race behind FR-005
- `.github/workflows/ci-failure-notify.yml` — the `workflow_run` listener and its `head_branch == 'main'` guard (Assumptions)
