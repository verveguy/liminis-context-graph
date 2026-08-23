---
layout: default
title: Release Process
---

# Release process

This is the maintainer procedure for verifying CI status before cutting a release. It
exists because reading a job's pass/fail *conclusion* alone is not a trustworthy signal
in this repository: issue #430 found that a `2>&1 | tee <log>` pattern in six `ci.yml`
jobs (the required `test (ubuntu-latest)` gate plus all five real-corpus e2e jobs) and
three `bench.yml` steps ran under GitHub Actions' implicit default shell, which has no
`pipefail` — so each step's exit status was `tee`'s, not the piped test/bench command's.
Three consecutive `main` runs were found with `test result: FAILED` in the job log while
reporting a passing conclusion, and the masked gate let a real regression (#428) ship in
releases 0.13.0, 0.13.1, and 0.13.2, each "verified" only by reading job conclusions. See
[ADR-0430](adr/0430-ci-tee-pipefail.md) for the fix.

The fix (a workflow-level `shell: bash` default, restoring `pipefail`) makes conclusions
trustworthy going forward, but grepping the log is still the documented step here as
defense in depth: the whole point of #430 is that "the conclusion looked right" was
already true of the runs that turned out to be broken.

## Before cutting a release

For the release commit's CI run on `main`, check **both** of the following — a passing
conclusion alone is not sufficient. Set `RELEASE_SHA` to the exact commit being released
(e.g. `RELEASE_SHA=$(git rev-parse HEAD)`) before running either command below.

1. **Job conclusions, bound to the release commit.** `gh run list --json conclusion`
   reports the *workflow run's* overall conclusion, not each job's — and with several
   workflows (`CI`, `Release`, `Docs drift check`, ...) triggering on the same push, an
   unfiltered `--limit 1` isn't even guaranteed to return the `CI` run, nor the run for
   the specific commit being released (a later push to `main` after the release commit
   would shift `--limit 1` off it). Pin the lookup to the release commit's SHA and to a
   completed run, then check the six jobs by name — `test (ubuntu-latest)` and the five
   real-corpus e2e jobs (`real_corpus_e2e`, `mcp_real_corpus_e2e`,
   `mcp_real_corpus_mutation_e2e`, `mcp_real_corpus_admin_data_e2e`,
   `mcp_real_corpus_admin_lifecycle_e2e`) must each show `conclusion: success`:

   ```bash
   run_id=$(gh run list --workflow ci.yml --commit "$RELEASE_SHA" --status completed \
     --limit 1 --json databaseId --jq '.[0].databaseId')
   gh run view "$run_id" --json jobs --jq '.jobs[] | {name, conclusion}'
   ```

2. **Log grep for the actual test result, with retrieval failing closed.** Using that
   same `$run_id`, confirm the run's log contains no `test result: FAILED` line. Capture
   the log to a file and check `gh`'s own exit status first — piping `gh run view --log`
   straight into `grep` would make a failed log fetch (rate limit, expired log, network
   error) look identical to "no match found", which is exactly the kind of masked
   failure this document exists to avoid:

   ```bash
   gh run view "$run_id" --log > /tmp/ci-run.log   # fails loudly if retrieval fails
   grep -a "test result: FAILED" /tmp/ci-run.log
   ```

   No output from the `grep` means no failing test was masked. Any match — even
   alongside a "success" conclusion — means do not cut the release; investigate first.

Do not treat step 1 alone as sufficient evidence that "full e2e passed." Step 2 is the
one that actually verifies it.

## Docs publishing

The docs site is **no longer published from `main`**. Every merge to `main` that
touches `docs/` still runs the PR-time checks below, but does not change the live
site. Publishing happens only when a GitHub Release is published, via
`.github/workflows/docs-publish.yml`. See
[ADR-0477](adr/0477-tag-based-versioned-docs-publishing.md) for the full design.

### What happens automatically when you cut a release

`release.yml` (cargo-dist) creates the GitHub Release once artifact builds finish.
That `release: published` event triggers `docs-publish.yml`, which:

1. Skips entirely if the release's tag doesn't match the `vX.Y.Z` version-tag
   scheme (e.g. a non-version release like `eval-artifacts-2026-07`) — no docs
   action is taken.
2. Builds that tag's `docs/` tree with Jekyll, `--baseurl`-overridden to
   `/liminis-context-graph/v<version>/`, and publishes it to the `gh-pages`
   branch at that path. Every previously published version's path is left
   untouched.
3. Recomputes "latest stable release" fresh from the GitHub Releases API
   (`scripts/docs-publish-latest-stable-version.sh`) — never trusting the
   triggering event alone. If the just-published tag **is** the latest stable
   (non-prerelease) release, its build is also promoted to the site root. A
   prerelease tag only ever gets its own versioned path; it never becomes root.
4. Regenerates `gh-pages/versions.json` from what's actually on disk, which
   drives the version switcher in the page footer.

### What to check after a release publishes

1. Confirm the `Docs publish` workflow run for the release succeeded:
   `gh run list --workflow docs-publish.yml --limit 1`.
2. Visit the root URL (`https://v3rv.com/liminis-context-graph/`) and confirm the
   footer reads the new version.
3. Visit the new version's own URL
   (`https://v3rv.com/liminis-context-graph/v<version>/`) and confirm it's live.
4. Spot-check that the previous version's URL is still reachable and unchanged.

If the workflow run failed (e.g. a transient build error), re-run it with
`workflow_dispatch` rather than cutting a new release — see the republish
procedure below, which uses the exact same mechanism.

### Republishing a correction without a new release (FR-006)

Use this when the docs for an **already-released** version are wrong about
behaviour that has already shipped — the exact situation
[#473](https://github.com/verveguy/liminis-context-graph/issues/473) dealt with by
hand before this workflow existed. This procedure needs no new git tag and no new
GitHub Release.

1. Fix the docs on `main` (or a branch) as you normally would, and merge.
2. Run the publish workflow manually for the affected version:

   ```bash
   gh workflow run docs-publish.yml -f version=0.13.3
   ```

   By default this builds `refs/tags/v0.13.3` — i.e. it rebuilds the tag's own
   `docs/` tree, so it only picks up your fix if you've already fast-forwarded or
   cherry-picked it onto that tag. To publish a fix that lives on `main` instead
   (the common case), pass the ref explicitly:

   ```bash
   gh workflow run docs-publish.yml -f version=0.13.3 -f ref=main
   ```

   `docs-publish-build.sh` passes `DOCS_VERSION=0.13.3` to the site build
   regardless of which ref you build from, so the published page footer still
   reads the correct version even though the content came from `main`. (It used
   to patch `docs/_config.yml` for this; the Astro site takes the value from the
   environment instead, leaving the working tree alone.)

3. "Latest stable" is recomputed fresh from the Releases API on this run too, so
   the root URL is updated automatically if (and only if) `0.13.3` is still the
   current latest stable release. Republishing an older version never touches
   root.
4. Verify using the same steps as "What to check after a release publishes" above.

### One-time manual steps (required once, after this mechanism first ships)

Two follow-ups are manual repo-settings / one-off actions outside any PR diff
(the same category as GitHub Pages' original enablement — see ADR-0295). Until
both are done, this workflow builds and pushes to `gh-pages` correctly, but the
live site keeps serving from `main` as before:

1. **Pages source switch.** In the repository's Settings → Pages, switch
   `source.branch` from `main` to `gh-pages` (`source.path` to `/`,
   `build_type` left as `legacy`).
2. **Backfill.** Only tags carrying `site/` can be built by this workflow, since
   that is what it runs. Every tag up to and including `v0.13.3` shipped the
   Jekyll site instead and cannot be rebuilt under this scheme — the build script
   says so and exits rather than producing something misleading. Publish from the
   first release that includes the Astro site onward; there is nothing to
   backfill before it.

   Backfilling the Jekyll-era versions would mean building them with a site they
   never shipped with, which is the opposite of what per-version copies are for.

If Pages ever needs to be re-pointed (e.g. after a repository transfer), redo
step 1; the `gh-pages` branch itself is unaffected by that setting.

## Related

- [ADR-0430](adr/0430-ci-tee-pipefail.md) — the `tee`/`pipefail` defect this process
  works around, and the workflow-level fix
- [ADR-0477](adr/0477-tag-based-versioned-docs-publishing.md) — the tag-based,
  versioned docs publishing design described above
- `.github/workflows/ci.yml` — the required gate and five e2e jobs
- `.github/workflows/docs-publish.yml` — the docs publishing workflow
- #428 — the regression that shipped behind these jobs while conclusion-only
  verification was in use
- #473 — the docs-drift audit that motivated the FR-006 republish procedure above
