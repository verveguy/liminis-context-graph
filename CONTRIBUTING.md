# Contributing to Liminis Context Graph

Thanks for your interest in contributing. This project is pre-1.0 and the maintainer cannot promise active review SLAs, but well-scoped contributions are welcome.

## Filing issues

Use the GitHub issue templates:

- **Bug report** — reproduction steps, expected/actual behaviour, environment.
- **Feature request** — problem statement, proposed solution, acceptance criteria.

Both templates are in [`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE/). The Constitution alignment section in the feature-request template is maintainer-internal; external contributors may skip it.

## Submitting a pull request

For external contributors, the standard fork-branch-PR flow applies:

```bash
# Fork the repo on GitHub, then clone your fork
gh repo fork verveguy/liminis-context-graph --clone
cd liminis-context-graph

# Create a branch for your change
git checkout -b fix/my-bug-description

# Make your changes, then run the pre-commit gate (see below)
cargo fmt --all && cargo test && cargo clippy --all-targets -- -D warnings

# Push and open a PR
git push -u origin fix/my-bug-description
gh pr create --fill
```

Keep PRs focused — one logical change per PR. A focused bug fix or small feature is much easier to review than a large refactor bundled with a feature.

## Pre-commit gate

Before pushing, run these three commands from the repo root:

```bash
cargo fmt --all
cargo test
cargo clippy --all-targets -- -D warnings
```

All three must pass. CI runs the same checks; a failure blocks merge. See [`CLAUDE.md`](CLAUDE.md) for the detailed rationale and CI configuration notes.

## No CLA, no DCO

No contributor license agreement and no Developer Certificate of Origin sign-off are required. Contributions are accepted under the project's [MIT license](LICENSE) by the inbound=outbound convention: by submitting a PR you agree your contribution is licensed under MIT.

## Project conventions

### Architecture decisions

Significant architectural changes should be recorded as an Architecture Decision Record in [`docs/adr/`](docs/adr/). See [`docs/adr/0001-record-architecture-decisions.md`](docs/adr/0001-record-architecture-decisions.md) for the format. If you're unsure whether your change warrants an ADR, err on the side of writing one — a short ADR is better than an undocumented decision.

**Name it after the issue, not the next free number.** A new ADR is `docs/adr/<issue_number>-<slug>.md`, using the GitHub issue number that motivated the decision, zero-padded to four digits — for example `docs/adr/0283-name-index-scan-fallback-for-endpoint-authority.md`. If there is no motivating issue, use your PR number and say so in the ADR's header.

Please **do not take the next sequential number**. `0001`–`0052` are sequential for historical reasons and are immutable, but a shared counter is claimed at branch time, so two contributors working in parallel both pick the same number. That collision does not show up as a conflict in the ADR files — they have different slugs and both apply cleanly — only as a conflict on the one row each adds to [`docs/adr/index.md`](docs/adr/index.md), which surfaces as a branch that simply won't merge. The gap between `0052` and the first issue-numbered ADR is expected, not missing history.

Add your ADR's row to [`docs/adr/index.md`](docs/adr/index.md) in the same PR.

### Feature specifications

Substantial features (anything with user-facing scenarios and acceptance criteria) are specified using Spec Kit format in `specs/<issue-number>-<slug>/spec.md`. This is the maintainer's primary workflow via the Fabrik pipeline. External contributors don't need to use Fabrik — a well-written issue body covering problem, solution, and acceptance criteria is sufficient for a PR conversation.

### Worktree and PR convention (maintainer-side)

The maintainer works in git worktrees and never commits directly to `main`. See [`CLAUDE.md`](CLAUDE.md) for the full convention. External contributors working in forks are not subject to this constraint.

### CI failure issues

If you see an open issue labeled `ci-failure` with a `workflow:<name>` label (e.g. `workflow:real-corpus-e2e`), it was filed automatically by [`.github/workflows/ci-failure-notify.yml`](.github/workflows/ci-failure-notify.yml): one of the repo's non-gating post-merge workflows (`real-corpus-e2e`, `bench`, `eval`) failed on `main`. It's assigned to the maintainer, updates in place on repeat failures instead of duplicating, and closes itself automatically on the next passing run — see [ADR-0298](docs/adr/0298-ci-failure-notification.md).

## Questions

Open an issue or start a GitHub Discussion. The maintainer will respond on a best-effort basis.
