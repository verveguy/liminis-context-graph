# Feature Specification: Publish the docs site from release tags, not main, and keep every released version available

**Feature Branch**: `fabrik/issue-477`
**Created**: 2026-08-23
**Status**: Specified
**Input**: User description: "The docs site is continuously deployed from `main`, so it documents unreleased behaviour. It should be built from release tags instead, and it should keep prior versions available."

## Background

GitHub Pages for this repository is configured `build_type: legacy`, publishing `main` at path `/docs` to `v3rv.com/liminis-context-graph/`. Every merge to `main` goes live immediately. That means work merged for an unshipped milestone — the lbug 0.19.1 upgrade (#398), summary embeddings (#470) — publishes its reference changes to the public site before the release containing them exists. A reader on the site cannot tell which behaviour they actually have.

`docs/_config.yml` already carries `version: "0.13.3"` and `docs/_layouts/default.html`'s footer renders "Documents `liminis-context-graph` v{{ site.version }}", so the site *claims* a version it does not actually correspond to whenever `main` has moved past the last release.

This is a real, recurring failure mode, not a hypothetical one: #473 (merged just before this issue) had to hand-audit and close four gaps where the CHANGELOG for 0.13.0–0.13.3 had already shipped behaviour the docs site never described. Under the current always-publish-from-`main` model, the inverse problem is equally possible — and, by construction, unavoidable — the site can describe behaviour that has *not* shipped yet, with no way for a reader to tell the difference.

This issue moves publishing from "on every merge to `main`" to "on every published release," while preserving every previously published version at a stable URL, and preserving a fast path to correct a *shipped* version's docs without waiting for a new release (the exact case #473 needed).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Root URL always matches the latest shipped release (Priority: P1)

A user reads the docs at the root URL (`v3rv.com/liminis-context-graph/`) to understand how to use the binary they downloaded. Today they cannot tell whether what they're reading matches their binary or a since-merged, unreleased change. After this change, the root URL always reflects the docs tree of the latest *stable* (non-prerelease) release tag — never `main` HEAD, and never a prerelease.

**Why this priority**: This is the core defect the issue exists to fix. Without it, every other story is moot.

**Independent Test**: Merge a docs-affecting change to `main` that does not correspond to any published release (e.g. documents an unreleased feature). Confirm the root URL's rendered content is unchanged. Then publish a new stable release and confirm the root URL updates to match that release's `docs/` tree.

**Acceptance Scenarios**:

1. **Given** the latest published stable release is `v0.13.3`, **When** a docs change merges to `main` describing behaviour that ships in an unreleased milestone, **Then** the root URL's content does not change.
2. **Given** a new stable release `v0.14.0` is published, **When** the publish completes, **Then** the root URL serves `v0.14.0`'s `docs/` tree, and its footer/version indicator reads `0.14.0`.
3. **Given** a prerelease tag (e.g. an rc-style tag cargo-dist marks as a GitHub prerelease) is published, **When** the publish completes, **Then** the root URL is unchanged and still serves the latest stable release.

---

### User Story 2 - A user pinned to an older release can read the docs that match it (Priority: P1)

A user running an older binary (e.g. `v0.13.3`) needs docs for exactly that version, not the latest. They reach a stable, versioned URL for it and can trust it will still be there after later releases publish.

**Why this priority**: Equally core to the issue's stated goal ("keep every released version available"); without it, users on older binaries have no path to accurate docs at all.

**Independent Test**: After two releases have published (e.g. `v0.13.3` then `v0.14.0`), visit the versioned URL for `v0.13.3` directly and confirm its content, including its footer version indicator, still reflects `v0.13.3` — not `v0.14.0`.

**Acceptance Scenarios**:

1. **Given** `v0.13.3` has been published at its versioned URL, **When** `v0.14.0` is subsequently published, **Then** the `v0.13.3` versioned URL still serves `v0.13.3`'s content unchanged.
2. **Given** a user is on any page of the published site, **When** they look at the page, **Then** they can see which version they are viewing and can navigate to any other published version from there.

---

### User Story 3 - A docs correction to already-shipped behaviour reaches users without a new release (Priority: P2)

A maintainer finds that the docs for the *currently shipped* version are wrong about behaviour that has already shipped (the exact situation #473 addressed). They need to fix and publish that correction without cutting a release whose sole purpose would be to force a docs rebuild.

**Why this priority**: Tag-based publishing introduces a real regression for this specific case — today such a fix reaches users on the next merge; after this change it would otherwise wait for the next release. FR-006 exists specifically to prevent that regression, so it is high priority, but it is secondary to the two core publishing-model stories above.

**Independent Test**: With the docs site already published from the current latest release, make a docs-only correction on `main`, then run the documented republish procedure for that release. Confirm the correction appears live at that version's URL (and at root, if that version is still latest) without any new git tag or GitHub Release being created.

**Acceptance Scenarios**:

1. **Given** the latest published release is `v0.13.3` and its published docs contain an error about `v0.13.3`-shipped behaviour, **When** a maintainer fixes the error and runs the documented republish procedure for `v0.13.3`, **Then** the live site reflects the fix without a new release being cut.
2. **Given** the version being corrected is not the current latest stable release, **When** the same republish procedure runs for that older version, **Then** only that version's URL changes — the root URL continues to serve whatever is actually the latest stable release.

---

### Edge Cases

- A GitHub Release whose tag is not part of the project's version-tag scheme (e.g. the existing `eval-artifacts-2026-07` release, which predates and is unrelated to this issue) must not trigger a docs publish or attempt to build a `docs/` tree from that ref.
- A prerelease tag (cargo-dist marks rc-style tags as prereleases automatically) publishes a GitHub Release but must not become the root URL's content and must not disturb the current latest-stable content.
- Publishing a new version must not remove, corrupt, or otherwise affect any previously published version's URL.
- Two tags whose release-publish events are close together in time must still leave the site in a fully consistent end state — the eventual root URL and every versioned URL must reflect their correct respective content, with no partial or interleaved output from an in-progress build.
- Republishing a correction for a version that is *not* the current latest stable must change only that version's URL, not the root.
- Some existing release tags predate the docs site's Jekyll setup entirely — `v0.10.0` and `v0.11.0` have no `docs/_config.yml` at their ref — and so cannot be built under this scheme without modification. This constrains, but does not by itself resolve, the backfill design decision below.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The published site MUST be built from a release tag's `docs/` tree, not from `main`. Merging to `main` MUST NOT change the public site's content.
- **FR-002**: The root URL MUST serve the latest **stable** release. A prerelease MUST NOT become, or replace, the root URL's content.
- **FR-003**: Each released version MUST remain reachable at a stable, versioned URL. Publishing a new version MUST NOT remove or alter any previously published version.
- **FR-004**: Every page MUST state which version it documents and MUST offer navigation to other published versions. The displayed version MUST equal the tag the page was built from, not whatever value happened to be hand-maintained elsewhere.
- **FR-005**: Internal links, the ADR collection, `llms.txt`, and `llms-full.txt` MUST all resolve correctly when served from a versioned (non-root) path.
- **FR-006**: A maintainer MUST be able to correct and republish an already-released version's docs without creating a new release tag. This procedure MUST be documented (see FR-008) and MUST leave every other published version, including the root URL when the corrected version is not the current latest stable, unaffected.
- **FR-007**: The existing PR-time checks (`Build site and check internal links`, `Verify llms-full.txt is up to date`) MUST keep passing/failing on the same basis as today. A docs PR MUST still be validated before merge even though merging no longer publishes.
- **FR-008**: `docs/release-process.md` MUST be updated to state where publishing now happens, what to check after a release publishes, and how to run the FR-006 republish procedure.
- **FR-009**: A GitHub Release whose tag does not match the project's release-version tag scheme (e.g. non-version artifact bundles such as the existing `eval-artifacts-2026-07` release) MUST NOT trigger a docs publish.

### Key Entities

- **Release tag**: An immutable git tag (e.g. `v0.13.3`) with associated GitHub Release metadata, including whether it is marked a prerelease. Its `docs/` tree at that ref is the source of truth for exactly one published version.
- **Published version**: A self-contained, browsable slice of the site — either the root (for the latest stable release) or a versioned path (for every release, including the current latest) — built from one release tag's `docs/` tree, retained indefinitely alongside every other published version.
- **Version indicator / switcher**: The on-page UI (site header `nav` and footer, per `docs/_layouts/default.html`) that states which version the current page documents and links to other published versions.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After publishing a release, the root URL serves that release's docs, and the previous version is still reachable at its versioned URL.
- **SC-002**: Merging a docs change to `main` does not alter the public site.
- **SC-003**: A prerelease tag does not change the root URL.
- **SC-004**: On any page, the version shown matches the tag it was built from, and links to other versions work.
- **SC-005**: A correction to a shipped version's docs is published via the documented FR-006 path, demonstrated once, without a new release being cut.

## Assumptions

- Retention is "keep all released versions" — FR-003's "MUST NOT remove... any previously published version" is an unconditional requirement, not a size-bounded window. Plan may still choose the mechanics (e.g. a `gh-pages` branch accumulating directories vs. some other storage), but the product requirement is unbounded retention, not "last N."
- "Latest stable release" is determined by GitHub's own release/prerelease marking (cargo-dist already flags rc-style tags as prereleases automatically per its `[workspace.metadata.dist]` config in `Cargo.toml`); no additional maintainer curation step is assumed necessary at the product level.
- Deleting or un-publishing a release on GitHub after its docs version has already been published does NOT require automatically tearing down that published version — no such automatic teardown is in scope for this issue.
- The exact versioned-URL naming scheme (e.g. `/liminis-context-graph/v0.13.3/`) is expected to mirror the release tag string, but the precise scheme, and how it composes with Jekyll's `baseurl`, is a Plan/Research decision, not fixed here.
- Whether to backfill existing tags (`v0.10.0` … `v0.13.3`) or start version history from the next release is a Plan decision (see below); `v0.10.0` and `v0.11.0` are confirmed to predate the docs site's Jekyll setup (no `docs/_config.yml` at those refs) and so cannot be backfilled without modification, which constrains but does not resolve that decision.

## Out of Scope

- Replacing Jekyll with a framework that has built-in versioning. That is a much larger change; solve versioning within the current toolchain unless Research finds that genuinely unworkable, and say so explicitly if it does.
- Restructuring docs content or navigation beyond what a version switcher requires.
- The custom domain and HTTPS configuration (`https_enforced` is currently `false` — worth noting separately, not fixing here).
- Automatic teardown of a published version if its underlying GitHub Release is later deleted or un-published (see Assumptions).
- Deciding the exact publishing mechanism, workflow location/shape, trigger set, retention count, or backfill scope — these are explicitly deferred to Plan; see below.

## Design Decisions Deferred to Plan

These are flagged here because they materially affect the shape of the implementation, but they are **not** decided by this spec — Research and Plan own them:

- **Publishing mechanism.** Pages must move off `legacy`. Options include a Pages workflow (`build_type: workflow`) that builds and deploys, or a `gh-pages` branch that accumulates versioned directories. Whichever is chosen, the deploy must be able to rebuild the *whole* site — root plus every retained version — because adding a new version must not drop the others (FR-003).
- **`baseurl` handling.** `docs/_config.yml` hardcodes `baseurl: "/liminis-context-graph"`. A version served at `/liminis-context-graph/v0.13.3/` needs that overridden at build time (e.g. `jekyll build --baseurl …`) or internal links break. Confirm this interacts correctly with the `jekyll-relative-links` plugin rather than assuming it does (FR-005).
- **Which trigger(s).** `release: published` is the obvious one, plus `workflow_dispatch` for manual rebuilds (needed for FR-006). Prereleases must not publish (FR-002/FR-009) — cargo-dist marks rc-style tags as prereleases.
- **Where the workflow lives.** It must **not** go in `.github/workflows/release.yml` — that file is cargo-dist-generated; `allow-dirty = ["ci"]` in `Cargo.toml` preserves hand edits already made there for a different reason (see ADR-0021), and relying on that mechanism for a second, unrelated concern is fragile. A separate workflow is the safer shape.
- **Retention count.** This spec's FR-003/Assumptions already settle the product requirement as "retain all" — Plan owns only the storage mechanics, not whether to retain all vs. last N.
- **Backfill.** Whether to build the existing tags (`v0.10.0` … `v0.13.3`) so version history exists from day one, or start accumulating from the next release. `v0.10.0` and `v0.11.0` predate `docs/_config.yml` entirely (confirmed by inspecting those refs) and cannot be built as-is; `v0.12.0` onward have a working `_config.yml`. Backfilling is more useful but means building old `docs/` trees that were never built under this scheme — verify they still build before committing to it.

## Source References

- Pages today: `build_type: legacy`, `source.branch: main`, `source.path: /docs` (confirmed via `gh api repos/verveguy/liminis-context-graph/pages`).
- `docs/_config.yml` — `baseurl`, `version`.
- `docs/_layouts/default.html` — `header.site-header nav`, footer `v{{ site.version }}`.
- `.github/workflows/docs-drift.yml` — the two PR-time checks referenced by FR-007 (`check-llms-full`, `check-internal-links`).
- ADR-0021 (`docs/adr/0021-cargo-dist-build-setup-env-injection.md`) — documents why `release.yml` is cargo-dist-generated and why hand edits to it (via `allow-dirty`) are fragile; this is the ADR the original issue referred to as "ADR-0398," corrected here after confirming ADR-0398 does not exist in `docs/adr/`.
- ADR-0322 (`docs/adr/0322-ci-docs-only-fast-path.md`) — the docs-only CI fast path that must keep working.
- #473 — the docs-drift fix whose class of change FR-006 protects; its spec lives at `specs/473-docs-site-has-drifted/spec.md`.
- Existing release tags (`gh release list`): `v0.9.0` through `v0.13.3`, plus one non-version release (`eval-artifacts-2026-07`) that FR-009 must not treat as a docs-publish trigger.
