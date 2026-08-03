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

All three must pass. Measured on a worktree with a warm `target/debug` cache: ~8.6 minutes total
(`cargo test` is the dominant cost at ~8.5 min; `fmt` and `clippy` are each under 10s once `test`
has already built the debug artifacts) — inside the ~10-minute foreground-call budget documented
in `CLAUDE.md`. This gate deliberately does **not** include a release build or `cargo bench`: CI's
release-linked test suite and its R-003 dedup-overlap correctness gate (a `cargo bench` target)
are CI's job, not this gate's — see `CLAUDE.md` for why, and see ADR-0316 for how the bench gate
itself was made fast enough to stay PR-blocking.

CI runs additional checks beyond this local gate (the release build/test, the R-003 bench gate,
eval-script guards, an ML-dependency check); a failure in any of them blocks merge. See
[`CLAUDE.md`](CLAUDE.md) for the detailed rationale and CI configuration notes.

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

## Release runbook (maintainers)

The release version lives in `[workspace.package]` in `Cargo.toml`; cargo-dist derives the
release from it and **requires the pushed tag to match that version**, so the bump and the tag
must agree. Per this repo's worktree rule, prepare the release on a branch and land it via a PR —
never commit release prep directly to `main` — then tag the merge commit.

0. **Check non-gating workflow health.** `real-corpus-e2e`, `bench`, and `eval` run outside the
   required PR gate (deliberately, for cost reasons) and can go silently red for days — see
   [ADR-0298](docs/adr/0298-ci-failure-notification.md). Run
   `gh issue list --label ci-failure --state open` before proceeding. If it's empty, continue. If
   it isn't, either fix the underlying failure first or record in the release PR why the release
   is proceeding anyway — don't ship over a known-broken post-merge check silently the way
   `v0.11.0` did (#298).
1. **Bump the version.** In a worktree off `main`, set `version` under `[workspace.package]` in
   `Cargo.toml` to `x.y.z` (all workspace crates inherit it via `version.workspace = true`), then run
   `cargo update -p lcg-core -p lcg-service -p lcg-eval` to sync the workspace entries in `Cargo.lock`.
   Add any newly-introduced workspace member to that command — a crate left out keeps a stale version
   in the lockfile. Also update `docs/_config.yml`'s `version:` field to match, and run
   `scripts/generate-docs-llms-full.sh` to regenerate `docs/llms-full.txt` — the docs-drift CI
   check fails the PR if either is left stale (see [issue #295](https://github.com/verveguy/liminis-context-graph/issues/295)).
2. **Update `CHANGELOG.md`:** rename `## [Unreleased]` to `## [x.y.z] - YYYY-MM-DD`. If no
   `[Unreleased]` section has been maintained, write the section from the merged PRs since the last
   tag (`gh pr list --state merged --search "merged:>=<last-release-date>"`).
3. **Open a PR and merge it** to `main` once CI is green.
4. **Tag the merge commit and push:** `git tag vX.Y.Z <merge-sha> && git push origin vX.Y.Z`.
   The tag (`vX.Y.Z`) must equal the `Cargo.toml` version, or cargo-dist's `plan` step fails.
5. The release workflow builds all three platforms and publishes the GitHub Release
   automatically (~30–45 min).

If a release build fails: delete the local and remote tags (`git tag --delete vX.Y.Z` and
`git push --delete origin vX.Y.Z`), fix the issue on a branch, merge it, then re-tag the
corrected commit and re-push.

## Questions

Open an issue or start a GitHub Discussion. The maintainer will respond on a best-effort basis.
