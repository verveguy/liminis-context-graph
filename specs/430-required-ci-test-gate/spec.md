# Feature Specification: Required CI test gate cannot fail — tee without pipefail masks test failures

**Feature Branch**: `fabrik/issue-430`
**Created**: 2026-08-17
**Status**: Specified
**Input**: User description: "The required `test (ubuntu-latest)` merge gate cannot fail on a failing test, and neither can any of the five real-corpus e2e jobs. Six steps in `.github/workflows/ci.yml` end in `2>&1 | tee <log>` with no `set -o pipefail` in that step. GitHub Actions' default shell is `bash -e` without `pipefail`, so a pipeline's exit status is its last command's — `tee` essentially always succeeds, so `cargo test`'s status is discarded and the step passes regardless of the test result."

## Background

Six `run:` steps in `.github/workflows/ci.yml` pipe a test-running command's combined stdout/stderr through `tee` so the output is both streamed to the job log and saved to a file, e.g. `cargo test --release 2>&1 | tee /tmp/build.log`. GitHub Actions' default shell is `bash -e` *without* `pipefail`, so the shell reports the exit status of the last command in a pipeline — here, `tee`, which essentially always succeeds regardless of what the piped command did. The result: none of these six steps can fail on a failing test. One of them is the `test (ubuntu-latest)` job, which is configured as a required status check for merging — so a PR that breaks a test currently merges green.

This has already been observed on `main`, not merely reasoned about: three consecutive successful `main` runs (`31964057813`, `31961733817`, `31946235305`) each contain `test result: FAILED` in the job log while reporting a passing conclusion. The four tests responsible were tracked and fixed in the companion issue (#429, now closed), which this issue was blocked on — arming the gate before those fixes landed would have turned `main` red immediately and blocked every other PR. That prerequisite is now satisfied.

The masked gate has already had a real consequence: #428, a regression in the per-group WAL migration (never stamping `.wal-generation.json`, which made `knowledge_rebuild_from_wal` refuse to run on any upgraded workspace), shipped in a release whose covering tests were failing silently behind this exact defect. The same masking also voids the project's standing pre-release invariant that full e2e must pass before cutting a release: that invariant has so far been "verified" by reading job conclusions, which is precisely the signal this defect proves untrustworthy. Releases 0.13.0, 0.13.1, and 0.13.2 were all verified this way.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A failing test turns the required merge gate red (Priority: P1)

As a maintainer, when a PR introduces a change that breaks a test covered by `cargo test --release`, I want the `test (ubuntu-latest)` required status check to report failure, so that the PR cannot merge without the failure being noticed and addressed.

**Why this priority**: This is the core defect. The required merge gate existing but not actually gating is the most consequential instance of the pattern — it is the check every PR depends on, and its silent failure already let a real regression (#428) reach a release.

**Independent Test**: On a throwaway branch, introduce a deliberately failing assertion into a test that runs under `cargo test --release`, push it, and observe the `test (ubuntu-latest)` check on the resulting PR/run report a failing (red) conclusion. Revert the induced failure afterward; the check reports success again on the same commit history once genuinely passing.

**Acceptance Scenarios**:

1. **Given** the `test (ubuntu-latest)` job's `cargo test --release` step is modified to guarantee a failing test result, **When** the job runs, **Then** the job's conclusion is failure, not success.
2. **Given** the fix is in place and no test is deliberately broken, **When** the job runs against a clean commit, **Then** the job's conclusion is success and its log contains no `test result: FAILED` line.

---

### User Story 2 - Each real-corpus e2e job turns red on a genuine e2e failure (Priority: P1)

As a maintainer, when one of the five real-corpus e2e binaries (`real_corpus_e2e`, `mcp_real_corpus_e2e`, `mcp_real_corpus_mutation_e2e`, `mcp_real_corpus_admin_data_e2e`, `mcp_real_corpus_admin_lifecycle_e2e`) reports a failing test, I want that job's CI conclusion to be failure, so that e2e coverage is a real signal rather than a job that always reports green regardless of outcome.

**Why this priority**: These five jobs carry out the project's release-readiness e2e invariant ("full e2e must pass before cutting a release"). Under the current defect, that invariant is unverifiable — every one of these jobs can only ever report success, no matter what the underlying binary does.

**Independent Test**: For at least one of the five e2e jobs, deliberately break an assertion or fixture the corresponding binary depends on, run the job, and confirm the job's conclusion goes red. Revert the induced failure afterward.

**Acceptance Scenarios**:

1. **Given** any one of the five e2e jobs' underlying binary is modified to guarantee a failing test result, **When** the job runs, **Then** the job's conclusion is failure, not success.
2. **Given** the fix is in place and the corresponding e2e binary is left unmodified, **When** the job runs, **Then** the job's conclusion is success and its log contains no `test result: FAILED` line.

---

### User Story 3 - The release verification method stops trusting the misleading signal (Priority: P2)

As a maintainer preparing a release, I want the documented way to verify "e2e passed" to be grepping the run log for `test result: FAILED` rather than reading the job's pass/fail conclusion, so that the verification step itself does not depend on the exact signal this issue demonstrates cannot be trusted.

**Why this priority**: Lower priority than the two fixes themselves (P1) because it is a documentation change that only matters once P1 is done — but it is still required, since the acceptance criteria in the source issue explicitly call for it, and skipping it would leave the next release relying on the same kind of blind trust that let #428 through.

**Independent Test**: Locate (or create, since none currently exists — see Assumptions) the documentation a maintainer follows before cutting a release, and confirm it instructs `gh run view <id> --log | grep -a "test result: FAILED"` (or equivalent) rather than "check that the job shows green."

**Acceptance Scenarios**:

1. **Given** a maintainer is about to cut a release, **When** they follow the documented verification step for the e2e jobs, **Then** the documented step is a log grep for `test result: FAILED`, not a job-conclusion check.

---

### Edge Cases

- A step that pipes a command through `tee` where the command's exit status is not actually load-bearing (e.g., a purely informational `echo` or a command already followed by explicit status handling) does not need `pipefail` — only steps where a non-zero exit from the piped command should fail the step are in scope.
- Any additional `run:` step discovered during implementation — in `ci.yml`, `bench.yml`, or `release.yml` — that pipes a status-bearing command through another command (`tee` or otherwise) and currently has no `pipefail` protection is in scope for the same fix, even if not explicitly enumerated by line number in this spec, since workflow files change between when this spec is written and when it is implemented.
- If a chosen fix mechanism is workflow-wide (e.g., `defaults.run.shell: bash -eo pipefail` at the top of `ci.yml`) rather than per-step, every other `run:` step in that file changes shell behavior too, not only the six identified here — this is a broader blast radius than the per-step fix, and its correctness for the whole file (not just the six known steps) is in scope for review, per the issue's own framing of this tradeoff.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Each of the six `run:` steps below MUST fail its containing job when the piped command exits non-zero, not merely when `tee` fails:
  | file | line (at spec time) | job |
  |---|---|---|
  | `.github/workflows/ci.yml` | 295 | `test (ubuntu-latest)` — required merge gate, `cargo test --release` |
  | `.github/workflows/ci.yml` | 419 | `real_corpus_e2e` |
  | `.github/workflows/ci.yml` | 458 | `mcp_real_corpus_e2e` |
  | `.github/workflows/ci.yml` | 497 | `mcp_real_corpus_mutation_e2e` |
  | `.github/workflows/ci.yml` | 536 | `mcp_real_corpus_admin_data_e2e` |
  | `.github/workflows/ci.yml` | 576 | `mcp_real_corpus_admin_lifecycle_e2e` |
- **FR-002**: The mechanism used to satisfy FR-001 (adding `set -o pipefail` to each affected step individually, versus a workflow-wide `defaults.run.shell: bash -eo pipefail`) is an implementation choice, not fixed by this spec — the tradeoff (repetition vs. blast radius across every `run:` step in the file) is called out explicitly in the source issue for the next stage to weigh.
- **FR-003**: `.github/workflows/bench.yml` and `.github/workflows/release.yml` MUST be audited for the same pattern — any `run:` step piping a status-bearing command's output through another command with no `pipefail` protection — and any instance found MUST receive the same fix as FR-001. `bench.yml` is known at spec time to contain three such steps (the `dedup bench 1k`/`10k`/`50k` steps, each piping `cargo bench` through `tee`). `release.yml` MUST be re-checked at implementation time rather than assumed identical to the source issue's description, since it references a specific step (an OpenSSL static-link assertion from a separate in-flight issue, #398) that had not yet merged to `main` as of this spec being written and may or may not be present when this issue is implemented.
- **FR-004**: For the required test gate (the line-295 step) and at least one of the five e2e jobs, the fix MUST be demonstrated working, not merely asserted from reading the YAML: a deliberate, temporary failure is introduced, shown to turn the containing job's CI conclusion red, and then reverted.
- **FR-005**: After the fix lands, `main`'s CI run MUST show a passing conclusion on the affected jobs, *and* a `gh run view <id> --log | grep -a "test result: FAILED"` on that run MUST return no matches — both conditions, not either alone, since the conclusion alone is the signal this issue demonstrates cannot be trusted in isolation.
- **FR-006**: Documentation describing how a maintainer verifies e2e status before cutting a release MUST instruct grepping the run log for `test result: FAILED` (e.g. `gh run view <id> --log | grep -a "test result: FAILED"`) rather than reading the job's pass/fail conclusion.

### Key Entities

- **CI workflow step**: A single `run:` entry within a GitHub Actions job in `ci.yml`, `bench.yml`, or `release.yml`; the unit this issue's fix is applied to.
- **Job conclusion**: GitHub Actions' pass/fail/etc. summary for a job, currently the (untrustworthy) signal used for release verification.
- **Run log**: The full textual output of a workflow run, retrievable via `gh run view <id> --log`; contains the `test result: FAILED` or `test result: ok` line each affected step currently discards from the exit-status calculation.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Each of the six steps in the FR-001 table fails its job when the piped command's exit status is non-zero, confirmed by a passing/failing job conclusion that matches the piped command's actual result rather than always reporting success.
- **SC-002**: A demonstration (per FR-004) shows at least the required test gate and one e2e job turning red from a deliberate, temporary failure, with the failure then reverted and the same jobs returning to green.
- **SC-003**: The next `main` run after the fix merges reports a passing conclusion on the affected jobs, and its log contains zero occurrences of `test result: FAILED` (verified via the grep command in FR-005/FR-006), closing the gap between "job says green" and "tests actually passed."
- **SC-004**: `bench.yml` and `release.yml` carry no remaining instance of the same unprotected-pipe pattern for any `run:` step whose piped command's exit status is meant to be load-bearing.
- **SC-005**: A maintainer following the documented release-verification step performs a log grep, not a conclusion check, for e2e status.

## Assumptions

- The five e2e job names, their line numbers, and the `test (ubuntu-latest)` line number are as observed in `.github/workflows/ci.yml` at spec time (2026-08-17) and matched exactly against the source issue's table. Line numbers may drift by the time this is implemented; the job identities (by name) are the stable reference.
- The companion blocking issue (#429, four failing tests behind the masked gate) is closed as of this spec being written, satisfying the ordering constraint the source issue placed on this work ("do not arm the gate before the failing tests are fixed").
- No release-checklist document currently exists in this repository (checked `docs/`, `README.md`, `CHANGELOG.md`, `.github/PULL_REQUEST_TEMPLATE.md`, and `.github/` for release-process content — none found). FR-006 is therefore satisfied by creating the documentation where it most naturally fits (e.g. a "Release verification" section in `docs/operations.md`, or a new dedicated doc), not by editing a pre-existing checklist. Where exactly this lands is a Plan-stage decision; the requirement is the content and behavior it documents (grep over log, not conclusion), not its file location.
- `release.yml`'s OpenSSL static-link assertion, referenced in the source issue as introduced by #398, was not yet merged to `main` as of this spec being written (#398 remains open, its work sitting on an unmerged branch). FR-003 is worded to audit whatever `release.yml` actually contains at implementation time rather than presuppose that specific step exists.
- `bench.yml` is not a required merge gate (it runs only on `workflow_dispatch`, per its trigger config) — fixing its three affected steps (FR-003) is still in scope per the source issue's explicit ask to check it, but it does not carry the same release-blocking urgency as the `ci.yml` steps.

## Out of Scope

- Fixing any additional failing tests discovered as a side effect of arming the gate — the known failing-test set was the companion issue (#429) and is already resolved; any *new* failure surfaced only by this fix going in is a separate bug to be triaged on its own.
- Changing what the six jobs test, how they're triggered, or their required/optional status as merge gates — this issue only makes their existing pass/fail signal trustworthy.
- Broader CI performance or restructuring work (e.g. anything related to ADR-0322's docs-only fast path) — unrelated to this defect and explicitly called out in the source issue as something not to confuse with it.

## Source References

- `.github/workflows/ci.yml` (lines 295, 419, 458, 497, 536, 576 at spec time)
- `.github/workflows/bench.yml` (lines 36, 43, 50 at spec time)
- `.github/workflows/release.yml`
- #428 — the regression that shipped behind these green jobs
- #429 — companion issue fixing the four tests failing behind the masked gate (blocking prerequisite for this issue, now closed)
- #398 — in-flight lbug upgrade issue that introduces release.yml's OpenSSL static-link assertion (not yet merged as of this spec)
- ADR-0322 — the docs-only CI fast path, unrelated to this defect
