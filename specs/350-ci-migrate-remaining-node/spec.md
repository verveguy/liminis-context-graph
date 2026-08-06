# Feature Specification: CI — migrate remaining Node 20 actions to Node 24 releases

**Feature Branch**: `fabrik/issue-350`
**Created**: 2026-08-05
**Status**: Draft
**Input**: User description: "CI: migrate remaining Node 20 actions to Node 24 releases — the v0.12.1 release run surfaced a GitHub Actions warning that `actions/cache/restore@v4` targets the deprecated Node 20 runtime and is being force-run on Node 24 by GitHub's compatibility shim. The repo is mid-migration and inconsistent: some workflows already use Node 24 majors (`checkout@v6`, `upload-artifact@v7`, `download-artifact@v8`) while others are still on the Node 20 `@v4` releases of `checkout`, `cache`, `cache/restore`, `upload-artifact`, and `download-artifact`. 30 call sites across 8 files need to move to Node 24 releases, with three sites (the cargo-dist-generated `build-setup.yml`, the lbug cache contract in `ci.yml`, and the artifact permission-preservation step) called out as load-bearing and not safe to blind-bump."

## Background

GitHub Actions runners deprecated Node 20 and now force any action declaring a Node 20 runtime onto Node 24 via a compatibility shim, emitting a deprecation warning rather than failing outright ([changelog](https://github.blog/changelog/2025-09-19-deprecation-of-node-20-on-github-actions-runners/)). This surfaced during the v0.12.1 release run ([31027981661](https://github.com/verveguy/liminis-context-graph/actions/runs/31027981661)):

```
Node.js 20 is deprecated. The following actions target Node.js 20 but are being
forced to run on Node.js 24: actions/cache/restore@v4.
```

Nothing is broken today — the shim keeps these actions running. The risk is twofold: (1) the actions are executing on a runtime they were never tested against, with the warning as the only signal something is off, and (2) when GitHub eventually withdraws the shim, every workflow still pinned to a Node 20 release stops working outright, with no advance notice beyond this warning.

The repository is already mid-migration and inconsistent — `release.yml` and `swift.yml` use a mix, while `ci.yml`, `bench.yml`, `eval.yml`, `docs-drift.yml`, `claude-review.yml`, and `.github/build-setup.yml` are still substantially on the deprecated `@v4` releases. This spec covers finishing that migration deliberately, because three of the affected call sites are load-bearing in ways a blind find-and-replace would break (see Edge Cases and Assumptions below):

1. `.github/build-setup.yml` is cargo-dist-generated territory with a hand-patched cache step preserving valid YAML across `dist generate` runs.
2. `ci.yml`'s cache save/restore and artifact upload/download hand-off implements the lbug build-cache contract tuned in #341 / ADR-0341, including a no-recompile assertion (FR-008 in that work).
3. `ci.yml` carries an explicit "Restore executable bits" step compensating for `upload-artifact` not preserving POSIX permissions — a newer major's behavior here needs confirming, not assuming.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Consistent Node 24 action versions across all workflows (Priority: P1)

As a maintainer of this repository's CI, I want every `actions/checkout`, `actions/cache`, `actions/cache/restore`, `actions/upload-artifact`, and `actions/download-artifact` reference across all workflow files and `build-setup.yml` to target the same Node 24 release per action, so that CI no longer depends on GitHub's forced-compatibility shim and won't break outright when that shim is withdrawn.

**Why this priority**: This is the core problem statement — the deprecation warning and the risk of a future hard failure. Without this, the issue is not resolved.

**Independent Test**: Grep the repository for `uses: actions/(checkout|cache|cache/restore|upload-artifact|download-artifact)@v` and confirm every match uses a single Node 24 major version per action, with no `@v4` remaining anywhere in `.github/workflows/*.yml` or `.github/build-setup.yml`.

**Acceptance Scenarios**:

1. **Given** the current mix of `checkout@v4` (14 sites) and `checkout@v6` (5 sites), **When** the migration is complete, **Then** every `checkout` call site uses the same Node 24 major version.
2. **Given** `actions/cache@v4` and `actions/cache/restore@v4` in `ci.yml` and `build-setup.yml`, **When** the migration is complete, **Then** both use the same Node 24 major version (the `cache` and `cache/restore` actions ship from the same repository release).
3. **Given** the mixed `upload-artifact@v4`/`@v7` and `download-artifact@v4`/`@v8` sites, **When** the migration is complete, **Then** each action uses a single consistent Node 24 major version repo-wide.
4. **Given** a full CI run after the migration, **When** the run completes, **Then** the run log contains no Node.js 20 deprecation warning for any of these five actions.

---

### User Story 2 - `build-setup.yml`'s cargo-dist patch survives regeneration (Priority: P1)

As a maintainer relying on `cargo dist generate` to keep `build-setup.yml` in sync with the rest of the cargo-dist-managed release tooling, I want the version bump inside `build-setup.yml`'s cache step to be expressed in a form that `dist generate` reproduces exactly, so that the hand-patched literal-block-scalar fix (which keeps cargo-dist's emitted YAML valid) isn't silently reverted or broken by the next regeneration.

**Why this priority**: `build-setup.yml` is explicitly flagged in the issue as not safe to blind-bump — `Cargo.toml`'s `allow-dirty = ["ci"]` exists specifically to protect a hand-patch here. Breaking this either produces invalid YAML or silently reverts the fix on the next `dist generate`.

**Independent Test**: Run `dist generate` (or the project's equivalent regeneration command) after the change and confirm it produces no diff against the committed `build-setup.yml`, or that the existing `allow-dirty` exception still documents and covers the delta.

**Acceptance Scenarios**:

1. **Given** the bumped `actions/cache/restore` version in `build-setup.yml`, **When** `dist generate` is run, **Then** it produces no diff against the committed file (or the documented `allow-dirty` exception still applies and is unchanged in scope).
2. **Given** the regenerated or hand-edited `build-setup.yml`, **When** the file is parsed as YAML, **Then** it remains valid (the literal-block-scalar patch for cargo-dist's column-0 continuation lines is preserved).

---

### User Story 3 - lbug cache hit rate and e2e artifact hand-off remain intact (Priority: P1)

As a maintainer depending on `ci.yml`'s tuned build-cache and artifact hand-off (from #341 / ADR-0341) to avoid recompiling lbug and to pass release binaries to the five e2e jobs, I want the `actions/cache`, `actions/upload-artifact`, and `actions/download-artifact` version bumps to preserve both the cache-hit behavior and the executable-permission handling, so that a green CI run genuinely reflects a working pipeline rather than masking a regression a major version bump introduced.

**Why this priority**: These behaviors were tuned recently and are easy to break silently — a CI run can pass while quietly losing the cache hit (paying a full lbug rebuild) or losing executable bits (an e2e job failing for an unrelated-looking reason). The issue explicitly warns not to trust a green checkmark here.

**Independent Test**: Run CI twice on the same commit after the bump. On the second run, confirm the existing FR-008-style assertion (no "Compiling lbug" in the build log) still passes, and confirm all five e2e jobs pass using downloaded artifacts with correct executable permissions on the test binaries.

**Acceptance Scenarios**:

1. **Given** a commit that previously produced a cache hit under `actions/cache@v4`, **When** the same commit's CI is re-run under the bumped `actions/cache` major, **Then** the restore step still registers a hit and the build log contains no "Compiling lbug" line.
2. **Given** the `build release artifacts` job uploads binaries via the bumped `upload-artifact` major, **When** each of the five e2e jobs downloads them via the bumped `download-artifact` major, **Then** the existing "Restore executable bits" step (or its confirmed-unnecessary removal, if the newer major preserves permissions natively) results in binaries that execute correctly in every e2e job.

---

### Edge Cases

- What happens if `actions/cache@v6`'s (the selected Node 24 release) cache-key matching semantics differ subtly from `@v4`, producing a lower hit rate rather than an outright failure? This would degrade CI speed without failing any check, so it needs an explicit before/after hit-rate comparison rather than relying on "the job passed."
- What happens if `upload-artifact@v7` / `download-artifact@v8` changed their permission-preservation behavior since `@v4`? The existing "Restore executable bits" workaround step could become redundant (harmless but dead) or — worse — insufficient if the new default interacts with it unexpectedly.
- What happens if `.github/build-setup.yml`'s hand-patched literal-block-scalar formatting doesn't survive because the underlying cargo-dist tool version also needs bumping to emit an equivalent structure? This spec assumes the existing patch technique still applies; if cargo-dist's output format has changed, that's new information for Research/Plan to surface, not something to force through unchanged.
- What happens to any other workflow file not enumerated in the issue's table (e.g. any composite or reusable action definitions) that reference these same five actions? Out of scope per this spec unless discovered during implementation — see Out of Scope.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Every `actions/checkout`, `actions/cache`, `actions/cache/restore`, `actions/upload-artifact`, and `actions/download-artifact` reference across all GitHub Actions workflow files (`.github/workflows/*.yml`) and `.github/build-setup.yml` MUST target a Node 24 release. No `@v4` of `checkout`, `cache`, `cache/restore`, `upload-artifact`, or `download-artifact` may remain in any `uses:` reference in `.github/workflows/*.yml` or `.github/build-setup.yml`. (Historical or illustrative mentions of `@v4` in documentation, ADRs, or this spec's own Background section are out of scope — this requirement governs only actual workflow `uses:` pins.)
- **FR-002**: For each of the five actions, exactly one version MUST be used across every `uses:` call site in `.github/workflows/*.yml` and `.github/build-setup.yml` — no such file may pin a different major version of the same action than any other.
- **FR-003**: The `build-setup.yml` change MUST survive a `dist generate` run: regenerating the file MUST produce no diff against the committed version, or any resulting delta MUST remain covered by the existing `allow-dirty = ["ci"]` exception in `Cargo.toml`. The literal-block-scalar patch that keeps the cargo-dist-emitted YAML valid MUST NOT be lost.
- **FR-004**: After the bump, the lbug build cache MUST still register a hit on a second CI run of the same commit, verified by the existing no-recompile assertion in `ci.yml` (no "Compiling lbug" in the build log) — not merely by the job reporting success.
- **FR-005**: The e2e artifact hand-off (upload in the `build release artifacts` job, download in each of the five e2e jobs) MUST continue to work end to end, including correct executable permissions on the downloaded test binaries, after the `upload-artifact` / `download-artifact` version bump.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A full CI run on the migration PR emits no Node.js deprecation warning for any GitHub Actions action.
- **SC-002**: The lbug cache-hit assertion (no "Compiling lbug" in the build log) passes on a second CI run of the same commit.
- **SC-003**: All five e2e jobs pass using downloaded artifacts, with no recompilation and no permission-related failures on test binaries.
- **SC-004**: `dist generate` produces no diff against the committed `build-setup.yml` after the change, or the documented `allow-dirty` exception still covers the full delta.

## Assumptions

- The target Node 24 releases are the same ones already in use elsewhere in the repo for the actions that have already migrated: `actions/checkout@v6`, `actions/upload-artifact@v7`, `actions/download-artifact@v8`. For `actions/cache` and `actions/cache/restore` (not yet used anywhere in this repo at a Node 24 version), the target is `actions/cache@v6` / `actions/cache/restore@v6` — `actions/cache`'s v6 release is the current Node 24 release as of this migration, and `cache`/`cache/restore`/`cache/save` ship from the same repository and share a version line.
- GitHub-hosted runners already support executing these actions on Node 24 — this is implied by the fact that GitHub's compatibility shim is *already* force-running the Node-20-declared actions on Node 24 today, so no runner-side changes are needed.
- No action's public input/output interface changed in a way that requires updating `with:` parameters in this repo's usage — a version bump is expected to be drop-in at the workflow-YAML level. If Research finds otherwise for a specific action, that becomes an implementation detail for the Plan stage, not a spec change.
- This migration is CI-tooling-only: it changes no shipped artifact, no application behavior, and no user-facing surface. It can merge whenever ready and does not block or get blocked by any other in-flight feature work.

## Out of Scope

- Any GitHub Actions usage beyond `actions/checkout`, `actions/cache`, `actions/cache/restore`, `actions/upload-artifact`, and `actions/download-artifact` — other third-party or first-party actions not named in the issue's table are not covered by this spec, even if they also carry deprecation warnings.
- Enabling the currently-commented-out nightly/scheduled benchmark run in `bench.yml`, or any other CI behavior change unrelated to the Node version migration.
- Re-tuning the lbug cache key, cache TTL, or artifact retention policy — this spec only requires that the *existing* tuned behavior (from #341 / ADR-0341) continues to hold after the version bump, not that it be improved.
- Any future divergence between `actions/cache` and `actions/cache/restore` version lines, should GitHub ever version them independently — a new issue if and when that happens.

## Source References

- `.github/workflows/ci.yml`, `bench.yml`, `eval.yml`, `docs-drift.yml`, `claude-review.yml`, `release.yml`, `swift.yml`
- `.github/build-setup.yml` (cargo-dist-generated, `github-build-setup = "../build-setup.yml"` and `allow-dirty = ["ci"]` in `Cargo.toml`)
- ADR-0341 — the lbug cache and artifact hand-off contract this migration must not break
- Issue #322 — docs-only CI fast path
- Issue #328 — e2e on the PR path
- GitHub changelog: [Deprecation of Node 20 on GitHub Actions runners](https://github.blog/changelog/2025-09-19-deprecation-of-node-20-on-github-actions-runners/)
- Release run [31027981661](https://github.com/verveguy/liminis-context-graph/actions/runs/31027981661) — where the warning was first surfaced
