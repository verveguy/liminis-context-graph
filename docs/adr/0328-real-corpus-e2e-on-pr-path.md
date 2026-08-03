# ADR-0328: Run `real-corpus-e2e` on the PR Path as a Non-Required Check

**Status**: Accepted
**Date**: 2026-08-03
**Issues**: #328

## Context

`real-corpus-e2e.yml`'s five jobs (`real-corpus-e2e`, `mcp-real-corpus-e2e`,
`mcp-write-mutation-e2e`, `mcp-admin-data-e2e`, `mcp-admin-lifecycle-e2e`) ran only on
`push: branches: [main]` and `workflow_dispatch` — never on a PR. A regression
introduced by `f51c40c` on 2026-07-26 went unnoticed for 38 consecutive runs across a
week (#317, #325), and every change merged in that window — including the entire
0.12.0 milestone — landed without e2e verification. It is also why #325's own fix (PR
#327) could not be verified before merge: the suite that would prove it works did not
run against the branch.

The workflow's own header comment explained the original exclusion: adding these tests
to every PR's `test` job would be a material tax. That reasoning is about adding the
tests **to** the existing job, sequentially. As **separate parallel jobs**, the premise
does not hold — see Decision 1.

## Decision

### 1. Add a `pull_request` trigger; run the five jobs in parallel, not sequentially appended to `test`

Measured on run 30776173198 on `main`: the five jobs' wall clock is 8m46s (longest job
6m09s). The required `test (ubuntu-latest)` check measured 17m45s–19m48s across four
PRs the same evening. Run in parallel alongside `test` (not appended after it), the e2e
suite is expected to finish roughly 10 minutes before the check that already gates every
PR. Expected added latency to a PR's mergeable time: zero — PR #329 (implementing this
change) is the live measurement of that expectation; see its own Test Plan for the
observed delta once its CI run completes (FR-006/SC-003 remain pending confirmation
until then, not yet a completed result as of this writing).

### 2. Non-required, non-gating (FR-002)

The five jobs report their conclusion directly on the PR but are **not** added to
`main`'s required status checks. A failing e2e job does not block the merge button.
This is a deliberate trial period — see Decision 6 for the promotion criteria.

### 3. Reuse `ci.yml`'s classifier via a shared composite action, not a second deny-list

`.github/actions/classify-changes/action.yml` is the diff-classification logic
extracted verbatim from `ci.yml`'s `changes` job (see ADR-0322), parameterized with an
optional `extra-deny-pattern` input. `ci.yml`'s `changes` job now calls this action with
no extra pattern (a behavior-preserving refactor — same fail-safe defaults, same
deny-list, same output). `real-corpus-e2e.yml` gains its own `changes` job calling the
same action with `extra-deny-pattern: ^crates/core/tests/fixtures/real_corpus_wal/`, so
a change to the golden real-corpus WAL fixture (`.json`/`.jsonl`/`.yaml` files, none of
which match the base deny-list) still runs the suite even though it would otherwise
look docs-only.

There is exactly one classification implementation (FR-003/SC-005), not two
independently maintained copies that can drift. A composite action was chosen over a
`workflow_call`-triggered reusable workflow because it runs as a step inside the
calling job and never leaves that job's event context — `github.event.pull_request.*`
is simply the calling workflow's own context, with no reusable-workflow event-context
question to reason about.

Each of the five e2e jobs is gated with `needs: changes` plus the same fail-safe `if:`
pattern `ci.yml` uses for `build-lbug`/`test`: `!cancelled() &&
(needs.changes.result != 'success' || needs.changes.outputs.code_changed == 'true')`.
A job-level `if:` skip, not a trigger-level `paths:` filter — same reasoning as
ADR-0322: a `paths:` filter would mean no check run posts at all on a skip, which only
matters once/if these become required (FR-007), but the skip-mechanics choice is made
consistent with `ci.yml` now rather than revisited later.

### 4. No cache-race mitigation — the five jobs cannot participate in the write-write race

`.github/workflows/ci.yml`'s header comment documents a `duplicate symbol: yyjson_*` /
`antlr4::*` race when two concurrent jobs **write** to the same lbug build cache key.
All five `real-corpus-e2e.yml` jobs use `actions/cache/restore@v4` exclusively — restore
only, never save. Only `ci.yml`'s `build-lbug` job writes to the cache. Adding five more
read-only consumers cannot reproduce a write-write race; the only effect of a cache-miss
race is that an e2e job independently rebuilds lbug from source in its own isolated
runner — a cost/time effect, not a correctness one, since nothing is shared back to the
cache. No mitigation is implemented because there is nothing here to mitigate (FR-005) —
this is recorded as a finding, not built around, consistent with not adding handling for
scenarios that can't happen.

### 5. No concurrency-group change

The existing group, `real-corpus-e2e-${{ github.ref }}`, already resolves to
`refs/pull/<PR#>/merge` for a `pull_request` event — stable across every push to that
PR, and distinct from `refs/heads/main` used by the `push` trigger. A PR's superseded
runs cancel each other without touching `main`'s post-merge run, with no expression
change required (FR-004). This was confirmed empirically, not just reasoned about: when
PR #329's Review stage pushed a follow-up commit, GitHub cancelled that PR's own
in-flight `real-corpus-e2e`/`ci.yml` runs from the prior commit while `main`'s separate
post-merge concurrency group was untouched.

**Merge-train trial branches (`merge_train: on`, ADR-059) resolve the same way, with no
special-casing needed.** ADR-059 and the trial-branch git mechanics live in the Fabrik
engine, not this repo, so this was confirmed by reading the engine's implementation
(`fabrik/engine/merge_train.go`) rather than from anything visible in
`liminis-context-graph` alone. Two facts settle it:

- `PushTrainBranch` pushes each trial branch to `origin` for real, and the train opens a
  genuine draft "integration PR" against it (`assembleAndValidate`) — so a trial branch
  *does* fire a real `pull_request` event, and the five e2e jobs run on it exactly like
  any other PR.
- Every trial — the initial batch and every bisection sub-trial spawned while isolating
  a poisoning member (ADR-059 D4) — gets a distinct branch name and its own draft PR
  (`baseTrialName := fmt.Sprintf("merge-train-%s-%d", ...)`, incremented per re-form/
  bisection). A distinct PR means a distinct `github.ref` (`refs/pull/<integration-PR#>
  /merge`), so each trial's concurrency group is already isolated from every other
  trial, from any individual member's own PR group, and from `main`'s post-merge group —
  by the same `github.ref`-keyed mechanism as ordinary PRs, with nothing train-specific
  to add. Repeated pushes to the *same* trial branch (e.g. force-pushes within one
  bisection attempt) still correctly cancel only that trial's own stale e2e runs.

The practical effect: each merge-train trial/bisection draft PR picks up an extra ~9
minutes of restore-only e2e jobs, same as any code-touching PR. Since the checks are
non-required (Decision 2), this cannot affect a landing decision — the train's combined
Validate only polls required checks.

### 6. Promotion criteria (FR-007)

These jobs stay non-required until **14 consecutive calendar days of PR-path runs with
zero failures attributable to cache contention or runner queueing** (i.e., zero
spurious e2e-suite failures unrelated to a real code regression the suite correctly
caught). At that point, file a follow-up issue to add the five job names to `main`'s
required status checks. This is a concrete, checkable trigger rather than an
open-ended "revisit later" that a permanently non-required check tends to become.

## Consequences

- A code-touching PR now shows all five e2e jobs reporting pass/fail directly on the PR,
  closing the exact gap that let a regression ship silently for 38 runs (#317, #325).
- A docs-only PR's `changes` job still runs (~10-20s), but all five e2e jobs report
  "Skipped" — the same job-level-skip mechanism ADR-0322 established for `ci.yml`, so
  the docs-only fast path's PR-time-to-mergeable win from #322 is preserved unchanged.
- `main`'s required status checks are unchanged — merging a PR with a failing e2e job
  is still possible today, by design (FR-002). Promotion to required is deferred to the
  criteria in Decision 6.
- A merge to `main` still triggers the full suite exactly as before: `pull_request`'s
  default `types:` (opened/synchronize/reopened) excludes `closed`, so merging a PR does
  not re-trigger `real-corpus-e2e.yml` a second time — only the unchanged
  `push: branches: [main]` trigger fires post-merge.
- `ci-failure-notify.yml` (#298)'s `workflow_run` listener guards its `notify` job on
  `head_branch == 'main'`. Adding a `pull_request` trigger to `real-corpus-e2e.yml`
  makes that guard alone unreliable: `head_branch` isn't namespaced by repository, so
  a *fork* PR whose own branch happens to be named "main" (a common default branch
  name) would report `head_branch == 'main'` on a `pull_request`-triggered run, even
  though it's a PR-path run against the fork's main, not a push to this repo's main.
  Fixed by also requiring `github.event.workflow_run.event != 'pull_request'` — this
  preserves the existing push-to-main and workflow_dispatch-against-main cases the
  guard was written for, while excluding every `pull_request`-triggered run
  regardless of its reported `head_branch`.
- Five additional full `cargo build --release` jobs now run per code-touching PR push
  (previously only on push-to-main/dispatch), a real recurring increase in Actions
  minutes consumed. Per the issue's own framing, this carries no billing cost (the
  repo is public, so Actions minutes are free) and reducing the suite's runtime is
  explicitly out of scope for this issue — an accepted tradeoff, not an oversight.
- `ci.yml`'s `changes` job now delegates to the shared composite action instead of
  inlining the script; its behavior (outputs, fail-safe paths) is unchanged — confirmed
  during PR #329's Review stage, where both workflows' `changes` jobs classified that
  PR's own diff (which touches `.github/workflows/**` and `.github/actions/**`) as
  code-changed and ran the full suite.

## Alternatives Considered

- **Appending the e2e tests to `ci.yml`'s existing `test` job**: rejected — this is the
  approach the original exclusion comment argued against, and remains true: it would
  add ~9 minutes sequentially to the required check. Running the five jobs in parallel,
  alongside `test` rather than inside it, avoids this entirely (Decision 1).
- **A `workflow_call` reusable workflow for the shared classifier** instead of a
  composite action: viable (event-context inheritance was confirmed to work
  unmodified), but a composite action is structurally simpler since it never leaves the
  calling job's own event context — see Decision 3.
- **Duplicating the classify script into `real-corpus-e2e.yml`**: explicitly rejected by
  the spec (FR-003) — a second, drifting copy of the docs/code classifier is exactly the
  failure mode a shared single source of truth avoids.
- **Mitigating the lbug cache race preemptively** (e.g. distinct cache keys per e2e job):
  rejected — the restore-only design structurally rules out the write-write race this
  would defend against; building a mitigation for a race that cannot occur is
  speculative engineering against a scenario that can't happen (Decision 4).
- **Making these checks required immediately**: rejected — out of scope per the spec;
  Decision 6 defines the criteria for revisiting this later.

## References

- Issue #328
- #317 / #325 — the regression that went unnoticed for 38 runs because this suite never
  ran on PRs
- #322 / ADR-0322 — the docs-only fast path whose classification this ADR's Decision 3
  reuses via the new composite action
- #298 / ADR-0298 — the post-merge failure notifier, confirmed unaffected by this change
- `.github/actions/classify-changes/action.yml` — the shared classifier
- `.github/workflows/ci.yml` — the lbug cache-race comment analyzed in Decision 4
