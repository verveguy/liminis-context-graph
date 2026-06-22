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

### Feature specifications

Substantial features (anything with user-facing scenarios and acceptance criteria) are specified using Spec Kit format in `specs/<issue-number>-<slug>/spec.md`. This is the maintainer's primary workflow via the Fabrik pipeline. External contributors don't need to use Fabrik — a well-written issue body covering problem, solution, and acceptance criteria is sufficient for a PR conversation.

### Worktree and PR convention (maintainer-side)

The maintainer works in git worktrees and never commits directly to `main`. See [`CLAUDE.md`](CLAUDE.md) for the full convention. External contributors working in forks are not subject to this constraint.

## Questions

Open an issue or start a GitHub Discussion. The maintainer will respond on a best-effort basis.
