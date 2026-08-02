# ADR-0295: GitHub Pages Documentation Site

**Status**: Accepted
**Date**: 2026-08-02
**Issue**: #295

## Context

`liminis-context-graph` had no documentation site. Everything a user needed was in a single
~1000-line `README.md` plus loose files under `docs/`. That structure had a demonstrated
failure mode: PR #294 found that across the 45 PRs merged for `v0.11.0`, the README had
undercounted the JSON-RPC surface, omitted six environment variables the code reads, and
`docs/telemetry.md` described two telemetry events as "not yet emitted" while documenting a
payload whose fields didn't exist. Nobody audits a 1000-line file, so nobody caught it — and
this Research pass found a second, independent instance of the same failure mode already:
`TelemetryEvent::ExtractionFailure` (added by ADR-0306) had no documented section at all.

This matters more than usual for this project. It is built from source by outside users — six
community-reported issues drove the `v0.11.0` release — and its documentation is read by coding
agents pointed at the repo. A doc that is confidently wrong is worse than one that is missing.

`handarbeit/fabrik` was named as the reference implementation to follow: Jekyll served directly
from `docs/` via GitHub Pages "deploy from branch", custom `_layouts`, kramdown/GFM with rouge
highlighting, an `exclude:` list that keeps internal engineering material in the repo but off
the published site, and a `docs/llms.txt`/`docs/llms-full.txt` pair with a CI workflow that
regenerates the latter on every PR and fails if it's stale.

## Decision

### Jekyll served from `docs/`, one layout, no marketing plugins

`docs/_config.yml` configures kramdown/GFM+rouge and exactly two plugins: `jekyll-seo-tag` and
`jekyll-relative-links` (the latter so the ADRs' own pre-existing relative `.md` cross-links,
and every other repo-relative link written by hand, resolve on the built site the same way they
already do on GitHub's native file view). Fabrik's `jekyll-feed` was dropped — there is no blog
content here — and its `defaults:`/`twitter:` social-preview-image block was dropped
deliberately: FR-008 rules out marketing/OG imagery beyond what Pages requires, and this site
carries none.

A single `default.html` layout serves every page, including the landing page. Fabrik splits a
marketing layout from a docs layout because it has a marketing home page; this site has no
marketing tone anywhere, so there is nothing for a second layout to differentiate.

`baseurl` is `/liminis-context-graph`, not `""`. Fabrik's `baseurl: ""` matches its custom
domain; this site is a GitHub Pages *project* page, not a user/org page, and every
`relative_url`-generated link depends on getting this right from the first commit.

`url` is `https://v3rv.com`, not the `https://verveguy.github.io` originally decided in the
issue. The account carries a Pages custom domain, so every `verveguy.github.io` request
301-redirects to `v3rv.com` — `gh api repos/verveguy/liminis-context-graph/pages` reports
`html_url` as `http://v3rv.com/liminis-context-graph/` even though this repo's own `cname` is
null. Nothing in this repo can change that; it is an account-level Pages setting. Pointing `url`
at the domain that actually serves keeps generated absolute links and the sitemap off a
redirect. A dedicated subdomain for this project is a later change.

### `exclude:` replaces Jekyll's own default list — restated it explicitly, plus `vendor`/`.bundle`

Setting a site's `exclude:` key in `_config.yml` **replaces** Jekyll's built-in default
exclusions rather than adding to them. `history/` and `spikes/` (internal engineering write-ups
that live inside `docs/`, the Jekyll source root) are the entries that matter; `specs/` is
included too even though it already lives outside `docs/` under GitHub Pages' "deploy from
branch: main /docs" mode and is therefore a no-op today — kept as defensive documentation of
FR-004's intent in case the Pages source root ever changes. `vendor` and `.bundle` are listed
explicitly for the same reason: they're part of Jekyll's default exclude list, which the
site-specific `exclude:` above silently discards, and a local `bundle install --path
vendor/bundle` would otherwise be scanned as page content (confirmed by a local build that
failed on a `.erb` template shipped inside the `jekyll` gem itself, found only once `vendor/`
was on disk to scan).

### ADR front matter via one `defaults:` scope, not 57 file edits

`docs/adr/**` gets `layout: default` through a `defaults:` scope block in `_config.yml`, rather
than adding YAML front matter to every existing ADR file. ADRs are immutable decision records
(the project's own numbering convention, `CLAUDE.md`); this pattern renders all of them without
touching a single one.

### The ADR index gets explicit "historical record" framing

Several ADRs describe superseded behavior — ADR-0025's lazy index build, later revisited by
ADR-0034/ADR-0036's eager build, is the example this issue's own Edge Cases section names.
Publishing them verbatim without framing would present superseded decisions as if they were
current documentation. `docs/adr/index.md` now opens with an explicit statement that ADRs are
historical decision records, not current-state documentation, and points to the reference pages
(Operations, Configuration, IPC & MCP Reference) for what the system does *today*. This builds
on the index's pre-existing "historical numbering" appendix rather than replacing it.

### `docs/llms.txt` + generated, drift-checked `docs/llms-full.txt`

`scripts/generate-docs-llms-full.sh` (adapted from fabrik's `generate-llms-full.sh`) strips
front matter from a fixed, ordered list of published pages and concatenates them into
`docs/llms-full.txt`, with a `Source:` URL header per section derived from each page's title.
`docs/llms.txt` is hand-written, following fabrik's index shape. `.github/workflows/docs-drift.yml`
regenerates `llms-full.txt` in CI and fails the PR with `git diff --exit-code`, naming the
script to run, if the committed copy is stale (FR-007, SC-005) — this is the same
generated-and-checked pattern that gives the whole site protection against drift, not just a
one-time accurate snapshot the way the original README already proved isn't durable.

The script also resolves the small set of Liquid variables (`site.version`, `site.repository`)
used in page bodies before emitting plain text — `llms-full.txt` is a plain-text bundle, not
Jekyll-rendered output, so an unresolved `{{ site.version }}` would otherwise leak into it
verbatim.

### FR-010 (state the documented version) is enforced mechanically, in the same script

GitHub Pages "deploy from branch" mode runs stock Jekyll with no custom build step, so the
version string has to live in a checked-in file — `docs/_config.yml` carries a `version:`
field, rendered into the page footer and the landing page. `generate-docs-llms-full.sh` checks
that field against `Cargo.toml`'s `[workspace.package]` version on every run and fails with an
actionable message on mismatch, before it even reaches the content-diff check. A manual
release-runbook reminder was considered and rejected: that's exactly the class of unchecked
drift this issue exists to eliminate, and the release runbook (moved to `CONTRIBUTING.md` in
this same PR) already references running the script as part of the version-bump step.

### Internal-link checking is a CI gate, not a manual review step

`docs-drift.yml`'s second job runs `bundle exec jekyll build` against `docs/Gemfile`, then
`htmlproofer ./_site --disable-external --allow-hash-href` to catch broken internal links before
merge (SC-006). `htmlproofer` lives in a CI-only Bundler group so it never reaches the GitHub
Pages build itself. External links are excluded from the check to avoid network flakiness in CI.

Both docs-drift jobs pin Ruby **3.1**, not a newer default. The `github-pages` gem — which pins
`jekyll` 3.9.0 and `liquid` 4.0.3 to mirror GitHub Pages' actual build environment — breaks on
Ruby 3.2+: `liquid` 4.0.3 calls the deprecated `String#tainted?`, which Ruby removed in 3.2, so
every page render fails with `NoMethodError`. This was found by attempting a local
`bundle exec jekyll build` before relying on CI to catch it, per the Plan's own risk note.
Overriding the `github-pages`-pinned `liquid` version was rejected — it risks drifting from what
GitHub Pages itself actually builds with, which is the property this whole `docs-drift.yml` job
exists to verify against.

### GitHub Pages enablement is a manual step, not part of this PR's diff

`gh api repos/verveguy/liminis-context-graph/pages` returned 404 during Research — Pages is not
currently enabled for this repository, despite the spec's own Assumptions section asserting
otherwise. Enabling "deploy from branch: main /docs" is a one-time repository-settings action;
it does not block this PR's content, since every page and internal link is correct independent
of whether Pages is currently switched on. SC-001 (the site actually serving) is verifiable only
once that one-time step is taken.

## Consequences

- New pages under `docs/` per FR-002: `getting-started.md`, `configuration.md`,
  `ipc-mcp-reference.md`, `ontology.md`, `operations.md`, `testing-and-evaluation.md`, plus
  front matter added to the pre-existing `telemetry.md`, `eval-full-corpus-runbook.md`, and
  `extraction-quality-evaluation.md`.
- `README.md` shrank from ~1000 lines to under 250 (SC-004), keeping a full standalone
  quickstart (User Story 1.2) and gaining a "Documentation" section linking to every page above.
  Reference sections that moved (configuration, ontology, embedder/extractor selection,
  cassettes, eval harness, MCP transport) are not duplicated — the site page is each one's only
  canonical home.
- The maintainer-only release runbook moved from `README.md` into `CONTRIBUTING.md`, gaining a
  step to keep `docs/_config.yml`'s version and `docs/llms-full.txt` in sync with the
  `Cargo.toml` bump.
- `docs/telemetry.md` gained the previously-missing `extraction_failure` section, closing the
  same class of drift PR #294 fixed once already, found again independently during this issue's
  own research pass. It also picked up `entities_missing_summary` and the `schema_invalid`
  classification value, both added to `main` by issue #314 after this branch's last rebase.
  `TelemetryEvent` has 13 variants as of this PR's last commit (SC-003) — a count this ADR
  intentionally does not restate elsewhere, since `docs/telemetry.md` is the enumeration's one
  canonical, mechanically-checked home and a second hand-maintained count here would itself be a
  drift risk.
- Every future doc change that touches a published page must run
  `scripts/generate-docs-llms-full.sh` before committing, or `docs-drift.yml` fails the PR. This
  is intentional friction in exchange for the property the original README never had: a stale
  doc is now a CI failure, not a silent gap waiting for the next audit issue.
- `docs/adr/index.md` and every ADR file continue to resolve at their current repo paths
  (FR-005) — nothing under `docs/adr/` moved.

## Alternatives Considered

**A newer Ruby (e.g. 3.3) with an unpinned/overridden `liquid` version.** Rejected — see
Decision above. Overriding a version the `github-pages` gem pins risks the CI verification job
passing against a configuration that doesn't match what GitHub Pages actually builds, defeating
the point of running `bundle exec jekyll build` as a pre-merge check at all.

**Committing `docs/Gemfile.lock`.** Rejected. It was generated locally under a Ruby version
(system Ruby 4.x) that differs from CI's pinned 3.1, and `bundle install`'s platform-specific
gem resolution (native extensions such as `nokogiri`, `ffi`) is not guaranteed to match across
that gap. `docs/.gitignore` excludes it, matching fabrik's own convention (no committed lock file
for its Jekyll site either) and letting `bundle install`/`bundler-cache: true` resolve and cache
fresh in each environment.

**Deriving `llms-full.txt`'s content live from `README.md`'s pre-split state, or from a single
combined file.** Rejected implicitly by FR-003/FR-006: reference content has one canonical home
(the site pages under `docs/`), and `llms-full.txt` is generated *from* those pages, not from a
separate hand-maintained bundle that could itself drift from either.

## Related

- #295 — motivating issue.
- PR #294 — the documentation-drift audit that motivated this issue; also the origin of the
  26-env-var/11-telemetry-event baselines this issue's SC-002/SC-003 make permanently checked
  rather than one-time counts (both baselines had already drifted by the time of this Research
  pass — 27 env vars, 12 telemetry events — and drifted again before this PR's final commit, to
  36 documented vars including deprecated aliases and 13 telemetry events; `configuration.md`/
  `telemetry.md` reflect whatever `main` currently has, not either snapshot).
- ADR-0306 — introduced `TelemetryEvent::ExtractionFailure`, the previously-undocumented event
  closed by this issue's `docs/telemetry.md` update.
- ADR-0314 — introduced `TelemetryEvent::EntitiesMissingSummary` and the `schema_invalid`
  classification value, merged to `main` after this branch's last rebase and documented in the
  same pass as the fixes above.
- ADR-0009, ADR-0027, ADR-0035, ADR-0038, ADR-0283 — content sources for the new Operations and
  IPC & MCP Reference pages; unchanged by this issue.
