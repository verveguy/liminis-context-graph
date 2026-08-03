# Feature Specification: cut the 0.12.0 release

**Feature Branch**: `fabrik/issue-326`
**Created**: 2026-08-02
**Status**: Draft
**Input**: User description: "chore(release): cut 0.12.0 — the 0.12.0 milestone is complete (open=0, closed=7); main carries 11 merges since v0.11.0 and still reads version = \"0.11.0\"; prepare the version bump, changelog, and release PR per the README/CONTRIBUTING release runbook, without pushing the tag."

## Background

The 0.12.0 milestone is complete — `open=0, closed=7`. `main` carries **11 merges** since `v0.11.0` (verified: `git log --oneline v0.11.0..origin/main --merges` returns exactly 11) and still reads `version = "0.11.0"` in `[workspace.package]` in `Cargo.toml`, so no release prep has been done.

Releases here are cargo-dist, triggered by pushing a `vX.Y.Z` tag on `main`. There are no release branches: **the tag ships everything on `main`**, so the milestone is a record of what shipped, not a filter on it.

**Correction to the runbook location named in the original issue**: the "Release runbook (maintainers)" section no longer lives in `README.md`. It moved to `CONTRIBUTING.md` as part of #295 (the GitHub Pages documentation site). Verified present at `CONTRIBUTING.md` (`## Release runbook (maintainers)`, currently steps 0–5). Follow that copy — it is also the concrete illustration of this spec's own edge case ("follow the current text, not memory"): even the issue's own citation had gone stale by the time of this Specify pass. The runbook now includes two elements the 0.12.0 issue text did not account for, both folded into the requirements below:

- **Step 0** (added for #298): before bumping the version, check `gh issue list --label ci-failure --state open`. If non-empty, either fix the underlying failure first or record in the release PR why the release is proceeding anyway — the runbook explicitly calls out *not* repeating what `v0.11.0` did (shipping over a known-broken post-merge check silently, #298). This is procedurally the same honesty obligation as FR-004 below, just with a concrete command attached.
- **Step 1** now also requires updating `docs/_config.yml`'s `version:` field (currently `"0.11.0"`) and regenerating `docs/llms-full.txt` via `scripts/generate-docs-llms-full.sh`. A `docs-drift` CI workflow (`.github/workflows/docs-drift.yml`) gates PRs on these staying in sync — omitting them fails CI, not just the release.

## What is in the release

**0.12.0 milestone (7, all closed — verified via `gh issue list`/`gh issue view`, including the one item that is a merged PR rather than an issue, #304):**

| issue | what |
|---|---|
| #297 | `indices_built` not set after runtime recovery — `knowledge_status` under-reported readiness |
| #304 | record the 2026-07 extraction eval (hosted vs local, ontology effect) — merged PR, not a plain issue |
| #306 | capture extraction failures whole; surface truncation in the report |
| #307 | token-budget policy and edge budget-exhaustion semantics (ADR-0307) |
| #310 | strict ontology mode dropped declared aliases and never told the model the constraint |
| #312 | strict mode still deleted out-of-vocabulary entities while edges were preserved |
| #314 | a missing `summary` field discarded the whole chunk, misreported as malformed JSON |

**Also on `main`, milestoned `Build`** — these do not change the binary but are in the source tarball, so decide deliberately whether they get changelog mentions. The docs site in particular is the most user-visible thing in this release despite changing no code:

- #295 — GitHub Pages documentation site, live at https://v3rv.com/liminis-context-graph/
- #298 — CI failure notification for non-gating workflows
- #301 — e2e tests calling `knowledge_rebuild_from_wal` without `force_clear`
- #315 — `CLAUDE.md` long-task guidance correction

This accounts for all 11 merges on `main` since `v0.11.0`: `#313→#312, #320→#295, #318→#315, #302→#297, #319→#314, #300→#298, #303→#301, #311→#310, #309→#307, #308→#306, #304` (direct merge, no wrapping PR).

## Why minor, not patch

Extraction behaviour changes materially: #307 sets a token-budget policy, #310 and #312 change strict-ontology handling, #314 changes what a malformed chunk does. Re-ingesting a corpus produces a different graph than 0.11.0 did. That belongs in an upgrade note, as it did for 0.11.0.

## Known caveat that must be stated honestly

`real-corpus-e2e` has been failing on `main` since 2026-07-26 — bisected to `f51c40c` (#239 / ADR-0046), 38 consecutive runs, tracked as **#325** and auto-filed as **#317** (verified both open via `gh issue view`). Four of five jobs pass; the failing one is the `mcp_real_corpus_admin_data_e2e` job defined in `.github/workflows/real-corpus-e2e.yml` (runs `crates/service/tests/mcp_real_corpus_admin_data_e2e.rs`) — root-caused in #325 to `knowledge_status` erroring instead of degrading gracefully when a core table is missing, not to anything in the 0.12.0 changeset itself.

**Everything in this release landed without e2e verification.** The other four jobs pass and nothing suggests a specific defect in the release contents, but the assurance that suite exists to provide, we do not have for 0.12.0. The release notes should not imply otherwise. This is also exactly the condition the runbook's step 0 (`gh issue list --label ci-failure --state open`) is designed to catch — as of this spec, that query returns #317 non-empty, so the release PR must record why it is proceeding anyway rather than silently shipping over it.

## User Scenarios & Testing

### User Story 1 — The release is tagged and published (P1)

**Why this priority**: Without a correct version bump and lockfile, cargo-dist's `plan` step fails and no binaries publish — this is the load-bearing mechanical step the rest of the release depends on.

**Independent Test**: Inspect `Cargo.toml` and all three `Cargo.lock` workspace entries directly; run `cargo dist plan` (or equivalent) locally against the release commit before tagging.

**Acceptance Scenarios**:

1. **Given** `main` at the release commit, **When** the version bump lands, **Then** `Cargo.toml` and all three `Cargo.lock` workspace entries (`lcg-core`, `lcg-service`, `lcg-eval`) read `0.12.0`.
2. **Given** the tag `v0.12.0` pushed at the merge commit, **When** cargo-dist runs, **Then** it publishes binaries for all three targets without a `plan`-step version mismatch.
3. **Given** the release commit, **When** `docs/_config.yml` and `docs/llms-full.txt` are inspected, **Then** both reflect `0.12.0` and the `docs-drift` CI check passes.

---

### User Story 2 — A reader can tell what changed and what to expect on upgrade (P1)

**Why this priority**: The changelog is the only artifact a downstream consumer reads before upgrading; an inaccurate or incomplete one either hides a breaking behaviour change or erodes trust in the release notes generally.

**Independent Test**: Diff the merged-PR list (`gh pr list --state merged --search "merged:>=2026-07-31"`) against the changelog's 0.12.0 section line by line; confirm every ADR link resolves.

**Acceptance Scenarios**:

1. **Given** `CHANGELOG.md`, **When** a user reads the 0.12.0 section, **Then** every merged change is represented and attributed to its issue.
2. **Given** the same section, **When** a user with an existing graph reads it, **Then** they learn that extraction output changes and re-ingest will not match prior results.
3. **Given** the same section, **When** a user checks release health, **Then** they learn honestly that `real-corpus-e2e` was red throughout this release cycle (citing #325/#317) rather than finding no mention of it.

---

### Edge Cases

- More work may land on `main` between spec and PR — re-derive the merge list at PR time rather than trusting this issue's snapshot.
- `Build`-milestone items are in the tarball but change no binary behaviour; decide per item whether it earns a changelog line rather than applying a blanket rule.
- The release runbook lives in `CONTRIBUTING.md`, not `README.md` as the original issue text assumed, and gained a CI-health-check step (step 0) and docs-sync sub-steps (step 1) since this repo's last release. Follow the current text at PR time, not this spec's snapshot of it, in case it changes again.
- `gh issue list --label ci-failure --state open` returning non-empty (currently #317) is the runbook's own honesty gate — it is not optional to run, and its result must be reflected in the PR description.

## Requirements

### Functional Requirements

- **FR-001**: Bump `version` under `[workspace.package]` in `Cargo.toml` to `0.12.0`, then run `cargo update -p lcg-core -p lcg-service -p lcg-eval` to sync all three workspace entries in `Cargo.lock`. Missing a crate leaves a stale entry and fails cargo-dist's `plan` step.
- **FR-002**: Update `docs/_config.yml`'s `version:` field to `0.12.0` and regenerate `docs/llms-full.txt` via `scripts/generate-docs-llms-full.sh`. The `docs-drift` CI workflow fails the PR if either is left stale.
- **FR-003**: Before opening the PR, run `gh issue list --label ci-failure --state open` per the runbook's step 0. If it returns any open issue (as of this spec, #317), record in the PR description why the release is proceeding anyway rather than shipping over it silently.
- **FR-004**: Add a `## [0.12.0]` section to `CHANGELOG.md`. There is no `[Unreleased]` section to rename — reconstruct from the merged PRs (`gh pr list --state merged --search "merged:>=2026-07-31"`), as 0.11.0's section was.
- **FR-005**: The changelog MUST carry an upgrade note covering the extraction-behaviour change, in the style of 0.11.0's.
- **FR-006**: The changelog MUST state the `real-corpus-e2e` caveat honestly rather than omitting it, citing #325 and #317 and naming the failing job (`mcp_real_corpus_admin_data_e2e`).
- **FR-007**: Cite the ADR for each change that produced one (#307→ADR-0307, #310→ADR-0310, #312→ADR-0312, #314→ADR-0314, #306→ADR-0306, #295→ADR-0295, #298→ADR-0298; #297, #301, #304, #315 have none), and verify every ADR link resolves before opening the PR.
- **FR-008**: Do NOT push the tag. Open the PR and stop. Tagging is a maintainer action taken after the PR merges — it triggers a public multi-platform build and is not reversible cheaply.
- **FR-009**: Before opening the PR, confirm no unintended work has landed on `main` — in particular that **PR #286** (chunk splitting, #284, milestoned 0.13.0) has **not** merged. It carries #292's data-loss path and must not ship in 0.12.0. (Verified not merged as of this spec: `gh pr view 286` reports `state: OPEN`, `mergedAt: null` — re-check at PR time per the edge case above.)

### Key Entities

Not applicable — this is a release-process change with no new data entities.

## Success Criteria

### Measurable Outcomes

- **SC-001**: `Cargo.toml` and all three `Cargo.lock` entries agree at `0.12.0`.
- **SC-002**: `docs/_config.yml` and `docs/llms-full.txt` agree at `0.12.0`, and the `docs-drift` CI check passes.
- **SC-003**: Every merge on `main` since `v0.11.0` is represented in the changelog or explicitly excluded with a reason.
- **SC-004**: Every ADR/doc link in the new changelog section resolves.
- **SC-005**: The PR is open and green; the tag is NOT pushed.

## Assumptions

- The 0.12.0 milestone's issue/PR set (7 items) and the 11-merge count against `v0.11.0` are stable as of this spec (2026-08-02) but must be re-verified at PR time per the edge case above — nothing here should be trusted as current without re-running the `gh`/`git log` commands that produced it.
- "Green" in FR-008/SC-005 means the required PR gate (not the non-gating `real-corpus-e2e`/`bench`/`eval` workflows) passes — the whole point of FR-003/FR-006 is that the non-gating suite is known-red and shipping anyway is the deliberate, documented choice.
- The release commit is the PR's merge commit into `main`; the tag target is that commit, not the PR head.

## Out of Scope

- Fixing #325 or making `real-corpus-e2e` green.
- Pushing the tag or publishing the release.

## Source References

- `CONTRIBUTING.md` — "Release runbook (maintainers)" (moved here from `README.md` by #295; supersedes the location named in earlier drafts of this issue)
- `CHANGELOG.md` — the 0.11.0 section as the format precedent
- `docs/_config.yml`, `docs/llms-full.txt`, `scripts/generate-docs-llms-full.sh` — docs-sync artifacts the `docs-drift` CI check gates on
- `.github/workflows/docs-drift.yml` — the CI check enforcing docs/version sync
- `.github/workflows/real-corpus-e2e.yml` — defines the `mcp_real_corpus_admin_data_e2e` job referenced in the caveat
- #325 / #317 — the e2e caveat FR-006 requires stating
- #286 / #284 / #292 — the in-flight PR that must not be included (FR-009)
