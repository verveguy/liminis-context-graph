# ADR-0477: Tag-Based, Versioned Docs Publishing via a `gh-pages` Accumulator Branch

**Status**: Accepted
**Date**: 2026-08-23
**Issue**: #477

## Context

GitHub Pages for this repository was configured `build_type: legacy`, publishing `main` at
path `/docs` to `v3rv.com/liminis-context-graph/`. Every merge to `main` went live
immediately, which meant work merged for an unshipped milestone (the lbug 0.19.1 upgrade
#398, summary embeddings #470) published its reference changes to the public site before
the release containing them existed. A reader on the site could not tell which behaviour
they actually had.

`docs/_config.yml` carried a hand-maintained `version:` field rendered in the page footer;
it only happened to be correct when `main` and the last release coincided. #473 (merged
just before this issue) had to hand-audit and close four gaps where already-shipped
behaviour (CHANGELOG 0.13.0–0.13.3) was never reflected on the site — the inverse failure
mode (describing unreleased behaviour) was equally possible and, by construction,
unavoidable under always-publish-from-`main`.

Full requirements are in `specs/477-publish-the-docs-site/spec.md`; this ADR records the
mechanism, not the requirements themselves.

## Decision

Publish from release tags, not `main`, and retain every previously published version at a
stable URL, using a `gh-pages` branch as a persistent accumulator that a new workflow
(`.github/workflows/docs-publish.yml`) builds into on every release.

### Publishing mechanism: `gh-pages` branch + `build_type: legacy`, not `build_type: workflow`

GitHub Pages' deploy model — both `legacy` and `workflow` (`actions/deploy-pages`) —
fundamentally *replaces* the whole published tree on each deploy; there is no native
"add this subdirectory, keep everything else" primitive. Retaining every version therefore
needs a persistent accumulator outside any single ephemeral workflow run. A `gh-pages`
branch is the standard pattern: each publish run fetches the existing accumulated tree,
adds/replaces only the `vX.Y.Z/` directory being built (and the root copy, only when that
tag is the current latest stable), commits, and pushes.

Given that accumulator, Pages' `source.branch` only needs to point at `gh-pages` instead of
`main` (`source.path: /`), keeping `build_type: legacy`. The alternative —
`build_type: workflow` with `actions/deploy-pages` — needs new OIDC-based permissions
(`pages: write`, `id-token: write`) and an artifact-upload step for content the workflow
didn't itself just build (the rest of `gh-pages`), for no capability this design needs
(no custom 404, no atomic-deploy requirement). Smaller diff from the current
configuration wins.

### `--baseurl` override, verified empirically

`docs/_config.yml` hardcodes `baseurl: "/liminis-context-graph"`. A version served at
`/liminis-context-graph/v0.13.3/` needs that overridden per build
(`jekyll build --baseurl /liminis-context-graph/v0.13.3`). This was flagged in
Research/Plan as architecturally plausible but empirically unverified — internal links go
through the `relative_url` Liquid filter and the `jekyll-relative-links` plugin (ADR-0295),
not hardcoded absolute URLs, so a CLI override *should* propagate, but no one had actually
built the site at a non-root baseurl and inspected the output.

Verified directly during Implement, against a `ruby:3.1` container matching the pinned CI
toolchain (see `docs-drift.yml`'s Ruby-3.1 pin — `github-pages`'s Jekyll 3.9.0 / liquid
4.0.3 breaks on Ruby 3.2+): building with `--baseurl /liminis-context-graph/v0.13.3`
correctly rewrites nav links (`/liminis-context-graph/v0.13.3/getting-started.html`) and
`jekyll-relative-links`-rewritten ADR cross-links to the versioned path, in both the root
and non-root builds. `docs/adr/index.md`'s own inter-ADR links resolved correctly under the
override too. This risk is retired, not assumed away — see `.github/workflows/docs-drift.yml`,
which now runs a second build+`htmlproofer` pass at a non-root baseurl on every PR so this
stays true going forward (FR-005).

### `docs-publish.yml` is a separate workflow from `release.yml`

`release.yml` is cargo-dist-generated (`cargo dist generate`); hand edits survive only via
`allow-dirty = ["ci"]` in `Cargo.toml`, and ADR-0021 already documents why riding on that
mechanism for an unrelated concern is fragile (YAML re-serialization risk on regenerate). A
second, wholly independent workflow is the safer shape and keeps the two concerns
(artifact release vs. docs publish) from coupling accidentally.

`docs-publish.yml` triggers on:
- `release: published`, filtered (FR-009) to tags matching `^v[0-9]+\.[0-9]+\.[0-9]+(-...)?$`
  — so a non-version release (e.g. the existing `eval-artifacts-2026-07`) is silently
  skipped rather than attempting to build a `docs/` tree from an unrelated ref.
- `workflow_dispatch` (`version` required, `ref` optional) — the FR-006 republish path: a
  maintainer can rebuild an already-released version's docs from a corrected `main`/branch
  commit without cutting a new release tag, the exact case #473 needed.

### "Latest stable" is always recomputed fresh, never trusted from the triggering event

`scripts/docs-publish-latest-stable-version.sh` queries the GitHub Releases API on every
run (`prerelease == false`, tag matches the version scheme, highest by semver) rather than
caching or inferring it from whichever event fired. Combined with a queuing `concurrency:`
group (`cancel-in-progress: false`) on `docs-publish.yml`, this makes the end state
self-correcting regardless of the order two close-together publish events actually run in
— the exact "two tags close together in time" edge case the spec calls out. It also means
`release: published` and the FR-006 `workflow_dispatch` republish path share one
implementation of "should this promote to root," rather than each guessing independently.

### `workflow_dispatch` always force-patches `version:`, unconditionally

`docs-publish-build.sh` rewrites the checked-out `docs/_config.yml`'s `version:` field to
the target version before building, every time — not conditionally on whether the ref
already matches. This is a no-op when building straight from the matching tag (the values
already agree) and is the mechanism that makes FR-006 work when building from a correction
commit on `main` instead: the published footer still reads the correct version even though
the content came from a different ref. One code path, not two.

### Version navigation is client-side (`versions.json` + a footer `<select>`)

`docs-publish-build.sh` regenerates `gh-pages/versions.json` from what's actually on disk
(scanned from the `v*/` directories present, not appended-to) on every run.
`docs/_layouts/default.html` fetches it from a **fixed root-relative path**
(`/liminis-context-graph/versions.json`, not `relative_url`-filtered) and renders a
`<select>` in the footer. Fixed-path, not filtered, because a versioned build's *overridden*
`site.baseurl` would otherwise resolve the fetch to a per-version copy of the manifest that
doesn't exist — the manifest only ever lives at `gh-pages` root.

Client-side, not build-time-baked, on purpose: baking the version list into every page's
HTML at Jekyll-build time would mean rebuilding every historical version's *pages* on every
new release just to update their nav — both wasteful and a much sharper reading of "must not
alter any previously published version" (FR-003) than necessary. Regenerating one small JSON
file at `gh-pages` root, alongside the one version directory actually being published, keeps
that guarantee cheap.

### Prereleases get a versioned URL, never root

FR-002 only constrains the root URL ("MUST serve the latest stable release"); it does not
say a prerelease tag should be excluded from publishing altogether. Since
`docs-publish-latest-stable-version.sh` filters to `prerelease == false`, a prerelease tag's
own build is included in `gh-pages` and reachable at its versioned URL, but can never
compute as "latest stable" and therefore never promotes to root — consistent with "keep
every released version available" while satisfying FR-002/FR-009.

### Backfill: `v0.12.0`–`v0.13.3` only, not `v0.9.0`–`v0.11.0`

`v0.9.0`, `v0.10.0`, and `v0.11.0` predate `docs/_config.yml`/`docs/Gemfile` entirely (no
Jekyll setup at those refs — confirmed directly, not assumed). Backfilling them would mean
inventing config that version never actually shipped with, contradicting the entire point
of building from what a tag's `docs/` tree actually contained. `v0.12.0` onward differ from
each other only in the `version:` field and build as-is. Backfill (a repeated
`workflow_dispatch` run per tag, ascending, so `v0.13.3` ends up on root) and the one-time
Pages `source.branch` switch are both manual maintainer follow-ups performed once, outside
this PR's diff — the same category of action as ADR-0295's original Pages-enablement step.

## Consequences

- Merging a docs change to `main` no longer changes the live site (FR-001/SC-002) — a PR
  author verifies content via the existing `check-internal-links`/`check-llms-full` checks
  (FR-007, unchanged in what they gate) and sees it live only once a release publishes.
- The root URL and every versioned URL are independently correct and stable (FR-002/FR-003),
  at the cost of `gh-pages` growing one directory per release forever — acceptable per the
  spec's explicit "retain all" requirement; no pruning mechanism exists or is planned.
- A maintainer has a documented, tag-free path (`docs/release-process.md`, "Republishing a
  correction") to ship a docs-only fix to already-released content without waiting for the
  next release (FR-006), closing the #473-shaped regression tag-based publishing would
  otherwise introduce.
- Two manual, one-time steps are required after this PR merges before the new mechanism is
  actually live: switching Pages' `source.branch` to `gh-pages`, and running the `v0.12.0`–
  `v0.13.3` backfill. Until both happen, the site keeps serving from `main` as before —
  merging this PR alone does not flip production.
- `html-proofer`'s internal-link check was found, during verification of the new
  `check-internal-links` baseurl pass, to report "0 internal links" checked even on the
  pre-existing root-baseurl pass — in this local reproduction and in real CI logs for
  already-merged PRs alike. This is an apparent pre-existing defect in `html-proofer`
  5.2.2's `async`-gem-based file processing, unrelated to this issue's changes; the check
  currently validates that the site *builds*, not that its internal links *resolve*. Left
  as a follow-up (file a separate issue to pin/patch `html-proofer`) rather than fixed here,
  since fixing a third-party gem's concurrency handling is outside this issue's scope.

## Related

- [ADR-0295](0295-github-pages-documentation-site.md) — the original Jekyll docs site this
  issue re-plumbs the publish path for; establishes `jekyll-relative-links`, the `llms.txt`/
  `llms-full.txt` pattern, and the precedent that Pages enablement is a manual step.
- [ADR-0021](0021-cargo-dist-build-setup-env-injection.md) — why `release.yml` is
  cargo-dist-generated and must not be hand-extended for unrelated concerns.
- [ADR-0322](0322-ci-docs-only-fast-path.md) — the docs-only CI fast path (FR-007);
  unaffected by this change.
- `docs/release-process.md` — the maintainer-facing "Docs publishing" procedure (FR-008).
- #473 — the docs-drift audit whose class of fix the FR-006 republish procedure protects.
