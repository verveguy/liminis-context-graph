# ADR-0298: CI Failure Notification for Non-Gating Workflows

**Status**: Accepted
**Date**: 2026-07-30
**Issue**: #298

## Context

`real-corpus-e2e.yml` runs on every push to `main`. Commit `db27471` introduced
`mcp_real_corpus_admin_lifecycle_e2e.rs` as part of #236 on 2026-07-26. The first failed run was
`ed90d489` at 20:02 that evening, followed by 24 consecutive failures spanning four days and
roughly a dozen merges. Nobody noticed until the failure was found by hand while smoke-testing
the `v0.11.0` release, after the tag had already been pushed.

The underlying test failure was real but minor (a status-flag assertion; filed separately). The
process defect was the serious one: `real-corpus-e2e.yml`'s own header comment states it exists
specifically for "post-merge verification, before a regression reaches a release" — and it did
catch a regression, it just had no way to tell anyone. A red run and a green run were
indistinguishable to every human and agent working in this repo unless someone opened the
Actions tab and checked.

This is the second instance of the same shape found in one day: PR #294 corrected documentation
that had drifted from the code across 45 merged PRs, discovered the same way — nobody was
watching a signal that existed but had no watcher.

Per GitHub branch protection on `main`, the only required PR check is `test (ubuntu-latest)`
(`ci.yml`). `real-corpus-e2e.yml`, `bench.yml`, and `eval.yml` are non-gating by design — see
each file's own header comment — and this ADR does not change that. `swift.yml` is currently
`if: false` (dormant, pending a GitHub Actions macOS image update) and is not watched.
`claude-review.yml` only triggers on `pull_request` and never runs against `main` directly, so
it is out of scope entirely.

## Decision

### A single centralized `workflow_run` listener, not a trailer job per source workflow

`.github/workflows/ci-failure-notify.yml` is triggered by `on: workflow_run`, watching
`real-corpus-e2e.yml`, `bench.yml`, and `eval.yml` by their `name:` values. GitHub computes one
aggregate `conclusion` per triggering run — including across `real-corpus-e2e.yml`'s 5
independent parallel jobs — so the listener needs no `needs:` graph wired into any source
workflow to see an aggregate result.

The alternative (a trailer job appended to each source workflow) was rejected: it would require
`needs:` listing all 5 of `real-corpus-e2e.yml`'s jobs, duplicate the same `gh` scripting three
times (or need a composite action to avoid that), and would need to reconstruct
`cancelled`-vs-`failure` disambiguation by hand in each workflow instead of getting it for free
from `workflow_run`'s single `conclusion` field. The three source workflows are unmodified by
this change — no new `permissions:`, no new steps, no `needs:` graph — which also makes FR-006
trivially true: nothing here can become a required check, because it isn't a check on the source
workflows at all, it's a separate workflow that fires strictly after them.

Because there is exactly one implementation site, no composite action or reusable
`workflow_call` workflow was introduced either — there is only one copy of the `gh` scripting to
begin with. Extending coverage to a future non-gating workflow (or re-adding `swift.yml` once
it's re-enabled) is a one-line addition to the `workflows:` array in
`ci-failure-notify.yml`, not a new copy of the mechanism.

### Dedup key: two labels, not the issue title or body

Every tracking issue carries a fixed marker label (`ci-failure`) plus a per-workflow label
(`workflow:<slug>`), where `<slug>` is the filename stem of the source workflow
(`real-corpus-e2e`, `bench`, `eval`), taken from `github.event.workflow_run.path` rather than
the workflow's human-readable `name:`. The filename-derived slug stays stable if the display
name is edited later; the `name:` values themselves are only used for the `workflow_run` trigger
match, which is unavoidable since GitHub's `workflow_run` trigger keys on `name:`, not path.

The listener searches for an open issue carrying both labels before creating a new one. A repeat
failure appends a comment to the existing issue rather than rewriting its body, so the issue's
comment history preserves *when* a workflow flapped versus stayed broken continuously — only
creation and closure touch the issue's body/state directly. Both labels are created on every
invocation, not as a one-time setup step. An "already exists" error is ignored; any other
label-creation error fails the workflow loudly, because `gh issue create --label <label>` fails
outright against a label that doesn't exist yet — without this, the very first real failure could
be dropped silently, which is the kind of gap this mechanism exists to close.

### Lifecycle mapping from `conclusion`

- `success` → close the existing tracking issue (if any) with a comment linking the passing run.
  No-op if none exists.
- `cancelled` or `skipped` → no-op entirely. `real-corpus-e2e.yml` sets
  `concurrency: cancel-in-progress: true`, so a superseded run reports `cancelled` — treating that
  as a failure would file spurious issues every time two pushes land close together.
- anything else (`failure`, `timed_out`, `action_required`, …) → create or comment-update the
  tracking issue, listing every job with `conclusion == failure` by name (so a 3-of-5 fan-out
  failure in `real-corpus-e2e.yml` isn't reduced to one arbitrary job name) and a bounded
  `--log-failed` excerpt from the *first* failing job only, so the issue body stays readable —
  full detail is always one click away via the run URL.

### `main`-only filter

The job runs under `if: github.event.workflow_run.head_branch == 'main'`. `real-corpus-e2e.yml`
also declares `workflow_dispatch` with no branch restriction (in addition to
`push: branches: [main]`), and `bench.yml`/`eval.yml` are `workflow_dispatch`-only — all three
can be dispatched against any ref, so this guard is load-bearing for all of them, not just
`bench.yml`/`eval.yml`. Without it, a developer's manual dispatch of any of them on a feature
branch would file a "main is broken" issue.

### Ownership, and deliberately no `fabrik:yolo`/`fabrik:cruise`

New issues are filed with `--assignee verveguy` — this is a single-maintainer repo with no
CODEOWNERS or team-routing convention to defer to instead.

The auto-filed issue intentionally does **not** carry `fabrik:yolo` or `fabrik:cruise`. This
repo's Fabrik pipeline picks up issues carrying those labels and autonomously works them through
Specify → Research → Plan → Implement → Review → Validate. A CI-triage issue — "this workflow
failed, here's the failing job and a log excerpt" — is not a spec, and letting Fabrik
auto-specify one before a human has looked at it is not obviously useful. The maintainer triages
the issue and opts it into the pipeline manually (by adding the label) if it turns out to warrant
a fix PR. A future contributor extending this mechanism should not add those labels by habit.

## Consequences

- A failing post-merge run on `main` now produces a durable, assigned, deduplicated GitHub issue
  automatically, closing the gap #298 reports. FR-001/FR-002/FR-003 are satisfied by the
  create/comment/close lifecycle above.
- Coverage extends to every non-gating workflow that runs against `main` today except the two
  explicitly excluded ones (`swift.yml`, dormant; `claude-review.yml`, PR-only) — see the PR body
  for the full enumeration required by FR-004/SC-005.
- `README.md`'s release runbook gained a pre-flight step (FR-005): check
  `gh issue list --label ci-failure --state open` before proceeding, and either it's empty or the
  release PR records why the release proceeds anyway.
- The listener's own reliability is unverified by this mechanism — if a `gh` call inside
  `ci-failure-notify.yml` itself fails, nothing surfaces that failure. That would be the same
  shape of gap this ADR closes, one level removed. Out of scope here: the listener is
  deliberately best-effort and non-gating, consistent with FR-006, and does not attempt to
  monitor itself recursively.
- Extending coverage to a future non-gating workflow (or `swift.yml`, once re-enabled) requires
  adding its `name:` to the `workflows:` list in `ci-failure-notify.yml` — no other file needs to
  change.

## Alternatives Considered

**Per-workflow trailer job.** Rejected — see Decision above. Requires `needs:` wiring into every
source workflow (5 jobs for `real-corpus-e2e.yml` alone), duplicates `gh` scripting per workflow,
and loses the free `cancelled`-vs-`failure` disambiguation `workflow_run`'s aggregate
`conclusion` provides.

**GitHub's native workflow-failure email/notification.** Rejected implicitly by the issue itself
(Assumptions: "GitHub Actions' native notifications are insufficient here, since they did not
surface 24 consecutive failures to anyone actively working in the repo") — native notifications
already existed during the 24-run incident and did not surface it.

**A project-board item instead of a GitHub issue.** Considered per the spec's Key Entities
section ("an issue, a board item, or an equivalent notification"). An issue was chosen because it
composes directly with `gh issue list`/`create`/`comment`/`close` — the same primitives this
repo's other automation (Fabrik) already depends on — with no separate board-API integration to
build or maintain.

## Related

- #298 — motivating issue.
- #236 — introduced `mcp_real_corpus_admin_lifecycle_e2e.rs`, the test that has been red on
  `main` since `db27471`.
- PR #294 — the documentation-drift audit; the same signal-without-a-watcher failure shape found
  the same day, in a different medium.
