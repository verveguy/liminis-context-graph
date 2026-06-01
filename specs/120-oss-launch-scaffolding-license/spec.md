# Feature Specification: OSS Launch Scaffolding — LICENSE, CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, CHANGELOG, README Polish

**Feature Branch**: `fabrik/issue-120`
**Created**: 2026-05-30
**Status**: Draft
**Input**: 2026-05-30 — repository is being prepared for public OSS launch. Surveyed current state: README exists (~9 KB, OSS-aware but internal-leaning quickstart), CLAUDE.md exists, `.github/ISSUE_TEMPLATE/` and `PULL_REQUEST_TEMPLATE.md` exist, ADRs in `docs/adr/`, repo is already public at `github.com/verveguy/liminis-graph`, no secrets in tracked files. Cargo.toml declares `license = "MIT"` on both workspace crates but no LICENSE file exists at the repo root. Missing the canonical OSS scaffolding files: LICENSE, CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, CHANGELOG. This issue covers all that scaffolding plus light README polish in one PR. Larger code-prep items (MCP-over-stdio transport, pluggable LLM extractor, embedder strategy) are tracked separately and intentionally out of scope here.

## Background

The owner has decided:
- **License**: MIT (matches what `Cargo.toml` already declares; simplest, most permissive, no CLA).
- **Scope of this issue**: scaffolding only — no code changes, no transport changes, no API changes.
- **README scope**: light polish only — fix internal-leaning phrasing, add license badge, link to CONTRIBUTING / SECURITY. NOT a full rewrite for an MCP-client audience. That waits until MCP-over-stdio (separate Fabrik issue, currently captured in `ideas/oss-launch-architecture.md` Question 1) actually lands so the README doesn't promise UX that doesn't exist yet.

The deferred-to-later items (explicitly NOT in this issue):

- MCP-over-stdio transport (`rmcp` integration, `--mcp-stdio` flag, tool wiring) — Fabrik issue, separate.
- OpenAI-compatible LLM extractor (generalize `AnthropicExtractor`) — Fabrik issue, separate.
- Embedder strategy decision (Principle V revisit, native vs pluggable HTTP) — needs a decision conversation first.
- Release workflow / prebuilt binaries — separate Fabrik issue once the above land.
- Optional rebrand discussion.

Single dependency-license verification (LadybugDB / `lbug`) is folded into the Research stage of this issue because (a) it's small (read one LICENSE file from the crate), (b) it's a hard blocker on OSS launch if it surfaces anything restrictive, and (c) it doesn't justify a separate issue.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — External Visitor Can Verify The License In One Glance (Priority: P1)

A first-time visitor to the GitHub repo can determine the project's license without reading source code or `Cargo.toml`.

**Why this priority**: this is the single most basic OSS-legibility expectation. Without a LICENSE file at root, GitHub does not render a license badge, security scanners flag the repo, downstream packagers can't include it, and the project effectively reads as "no license = all rights reserved" to a strict reader.

**Independent Test**: Open the repo's root in GitHub. Confirm the right-hand sidebar shows "MIT License" (auto-detected from `LICENSE`). Confirm the README has a license shield/badge.

**Acceptance Scenarios**:

1. **Given** the merged PR, **When** GitHub renders the repo home page, **Then** the sidebar shows "MIT License" detected from the root `LICENSE` file.
2. **Given** the README, **When** a visitor opens it, **Then** a license badge appears near the top linking to the LICENSE file or to a canonical MIT explainer.
3. **Given** a downstream packager runs an SPDX scan, **When** they look at the root LICENSE, **Then** it matches the canonical OSI MIT template byte-for-byte (modulo the copyright line).

---

### User Story 2 — Prospective Contributor Knows How To Contribute (Priority: P1)

A developer who wants to file an issue or open a PR can find clear instructions without asking, and the instructions work for someone who is NOT a Fabrik user.

**Why this priority**: today's contribution flow is Fabrik-driven (Spec Kit issues, labeled `fabrik:yolo`, auto-advanced through stages). That works for the maintainer but is opaque to outside contributors. The CONTRIBUTING file must document both paths: (a) "file an issue and let Fabrik work it" for owner-driven work, (b) "fork, branch, PR" for outside contributors who just want to submit a fix.

**Independent Test**: An outside developer with no knowledge of Fabrik reads `CONTRIBUTING.md` end-to-end and can: (a) find where to file a bug, (b) understand how to submit a small PR, (c) find the pre-commit commands they need to run before pushing.

**Acceptance Scenarios**:

1. **Given** a new contributor visits the repo, **When** they look for contribution guidance, **Then** `CONTRIBUTING.md` is present at the root and linked from the README.
2. **Given** `CONTRIBUTING.md`, **When** they read it, **Then** it covers: how to file issues, how to fork + branch + open a PR, the required pre-commit commands (`cargo fmt --all && cargo test && cargo clippy --all-targets -- -D warnings`), where ADRs live, where specs live, the worktree-and-PR convention for maintainer-side work (referenced via CLAUDE.md, not duplicated).
3. **Given** an outside contributor opens a fork-based PR, **When** the maintainer reviews it, **Then** the existing PULL_REQUEST_TEMPLATE.md is appropriate (or is adjusted to be) for both internal and external contributors.

---

### User Story 3 — Security Researchers Have A Private Disclosure Path (Priority: P1)

A security researcher who finds a vulnerability can report it privately via a documented channel rather than filing a public GitHub issue.

**Why this priority**: public-issue disclosure of a security bug is a known failure mode for OSS projects. Having `SECURITY.md` at the root is also a GitHub OSS-readiness signal. Standard expectation; small to write.

**Acceptance Scenarios**:

1. **Given** the merged PR, **When** GitHub renders the repo, **Then** the "Security" tab and the Security Policy link point to the root `SECURITY.md`.
2. **Given** `SECURITY.md`, **When** a researcher reads it, **Then** it states: which versions are supported, how to report a vulnerability (email or GitHub private vulnerability reporting), the expected response time, and the disclosure policy.

---

### User Story 4 — Community Behavior Expectations Are Documented (Priority: P2)

A would-be participant in issues/PRs/discussions can read the project's code of conduct.

**Why this priority**: standard OSS expectation, low cost (Contributor Covenant is a stock document). Not P1 because the project is small and the immediate risk is low; P2 because letting it slip is the kind of detail that signals "not really open source."

**Acceptance Scenarios**:

1. **Given** the merged PR, **When** GitHub renders the repo, **Then** the "Code of conduct" link is auto-detected from root `CODE_OF_CONDUCT.md`.
2. **Given** `CODE_OF_CONDUCT.md`, **When** a visitor reads it, **Then** it is the unmodified Contributor Covenant v2.1 with the maintainer contact email filled in.

---

### User Story 5 — Changes Are Tracked In A Visible Changelog (Priority: P2)

A user or contributor can read what has shipped or is staged to ship by checking `CHANGELOG.md`.

**Why this priority**: P2 because the project pre-launch has no released versions, so the changelog starts essentially empty. Still worth seeding the structure so the first cut of releases populates it cleanly. Keep-a-Changelog format.

**Acceptance Scenarios**:

1. **Given** the merged PR, **When** a contributor opens `CHANGELOG.md`, **Then** it follows Keep-a-Changelog 1.1 format with an `## [Unreleased]` section at the top.
2. **Given** the existing in-flight feature work (recent merges: WAL replay improvements #109/#110, lbug CI cache #115, bench restructure #116, extraction prompts port #92, etc.), **When** a maintainer prepares the first release tag, **Then** the changelog scaffolding makes it natural to populate `[0.1.0]` or whatever the first version is.

---

### User Story 6 — README Doesn't Read As Internal-Only (Priority: P2)

A first-time visitor reading the README does not encounter phrasing that implies the project is only useful inside a closed ecosystem.

**Why this priority**: P2 because the README is already reasonably OSS-aware ("Distinct product from upstream Python graphiti-core library — different language, different DB engine"); it just has internal-leaning bits in the Quickstart (e.g. references to `liminis-framework` as if it were the canonical consumer). Light polish only — do NOT rewrite for an MCP-client audience yet (that waits for MCP-over-stdio to land).

**Acceptance Scenarios**:

1. **Given** the polished README, **When** an external visitor reads the Quickstart, **Then** the instructions are runnable as-written against this repo alone, without assuming `liminis-framework` or `liminis-app` is also installed.
2. **Given** the polished README, **When** they read it end-to-end, **Then** it has: a license badge, a link to CONTRIBUTING, a link to SECURITY, a "what is this / what isn't this" framing near the top (already partly there in the Non-goals section).
3. **Given** the polished README, **When** a maintainer reads the change diff, **Then** structural sections are NOT removed, NOT reorganized, and NOT rewritten — only phrasing tweaks, the badge, and the links.

---

### User Story 7 — LadybugDB License Compatibility Is Verified (Priority: P1, blocking)

Before OSS launch is announced, the LadybugDB (`lbug`) dependency's license is confirmed to be compatible with MIT distribution of this project.

**Why this priority**: legal blocker. If `lbug` is GPL/AGPL/SSPL/BSL or carries any commercial restriction, the project either needs a different storage layer or the launch needs to wait until that's resolved. The ideas doc (`ideas/oss-launch-architecture.md` Question 4) flagged this as a verification step. Folded into Research stage because it's a single LICENSE-file read.

**Acceptance Scenarios**:

1. **Given** the lbug crate v0.16.1 (current pin), **When** the Research stage reads its LICENSE, **Then** it is one of: MIT, Apache-2.0, BSD-2/3-Clause, ISC, Unlicense, MPL-2.0 — any of which is compatible with our MIT distribution.
2. **Given** the LICENSE check fails (lbug is GPL/AGPL/SSPL/BSL/commercial), **When** Research reports this, **Then** the issue is paused with a clearly-described blocker, and the maintainer decides next steps (escalate, switch DB, defer launch). The remaining FRs do NOT block on this — LICENSE/CONTRIBUTING/SECURITY/etc. can ship regardless; only the *public launch announcement* should wait.

## Requirements *(mandatory)*

- **FR-001.** A `LICENSE` file MUST be added at the repo root containing the unmodified OSI canonical MIT License text. The copyright line MUST read `Copyright (c) 2026 verveguy` (or the maintainer's preferred legal-name form — Research stage confirms via the existing pattern in Cargo.toml `authors` field if present; otherwise use the GitHub org name).
- **FR-002.** A `CONTRIBUTING.md` file MUST be added at the repo root covering:
  - Where to file issues (GitHub Issues; link to the existing `.github/ISSUE_TEMPLATE/` choices).
  - The fork-branch-PR flow for external contributors (one short worked example).
  - The required pre-commit commands: `cargo fmt --all && cargo test && cargo clippy --all-targets -- -D warnings`. Cite CLAUDE.md as the deeper reference; do not duplicate the full content.
  - Where ADRs live (`docs/adr/`) and the convention that architectural changes get one.
  - Where specs live (`specs/<issue-number>-<slug>/spec.md`) and the Spec Kit format expectation for substantial features. Note that Fabrik-driven workflow is the maintainer's path; external contributors are not expected to use Fabrik.
  - DCO / sign-off expectation: **no CLA, no DCO required for this license choice**. Contributions are accepted under the project's MIT license by inbound=outbound convention. Document this explicitly.
- **FR-003.** A `CODE_OF_CONDUCT.md` file MUST be added at the repo root containing the unmodified Contributor Covenant v2.1 with the `[INSERT CONTACT METHOD]` placeholder filled in with the maintainer's contact email or GitHub Discussions URL. If no contact email is available, use a GitHub issue mailto link of the form `https://github.com/verveguy/liminis-graph/security/advisories/new` (this aligns the CoC contact with the security-disclosure path).
- **FR-004.** A `SECURITY.md` file MUST be added at the repo root documenting:
  - Supported versions (initially: "All versions; this project is pre-1.0 and the latest `main` is the supported line").
  - How to report a vulnerability: **GitHub's private vulnerability reporting** (`https://github.com/verveguy/liminis-graph/security/advisories/new`) as the primary path. Document an email fallback only if the maintainer supplies one.
  - Expected response time: maintainer to specify, suggested default "within 7 days, best-effort, no formal SLA pre-1.0".
  - Disclosure policy: coordinated disclosure; researcher and maintainer agree on a date.
- **FR-005.** A `CHANGELOG.md` file MUST be added at the repo root following [Keep a Changelog 1.1](https://keepachangelog.com/en/1.1.0/) format with at minimum an `## [Unreleased]` section. Do NOT attempt to backfill historical changes — leave the body sparse with a note like "Pre-1.0 development; see git log for history before 0.1.0."
- **FR-006.** The `README.md` MUST be polished (NOT rewritten) to add:
  - A license badge near the top: `[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)`
  - A "Contributing" subsection (one paragraph) linking to `CONTRIBUTING.md`.
  - A "Security" subsection (one paragraph) linking to `SECURITY.md`.
  - Quickstart phrasing edits: remove or rephrase any text that implies `liminis-framework` / `liminis-app` is required to use this binary. Concretely, the existing line "Preserve the IPC surface liminis-framework consumes" can stay in the Goals (it's accurate context), but the Quickstart commands MUST be runnable in isolation against this repo alone.
  - The README MUST NOT have sections removed, reordered, or substantially rewritten in this pass.
- **FR-007.** The existing `.github/ISSUE_TEMPLATE/` and `.github/PULL_REQUEST_TEMPLATE.md` MUST be reviewed (read-only audit). If they reference Fabrik-specific labels or workflow steps in ways that would confuse external contributors, a one-sentence "external contributors: see CONTRIBUTING.md" hint MUST be added at the top. No other edits to issue/PR templates in this pass.
- **FR-008.** The `lbug` crate (v0.16.1) license MUST be verified during the Research stage by reading its `LICENSE` file. The finding MUST be documented in the PR body (one sentence stating the license + a link to the source). If incompatible, the PR proceeds with all other items but the issue is flagged with a follow-up indicating the launch announcement is blocked.
- **FR-009.** No `NOTICE` file is required (MIT does not customarily require one). If FR-008 surfaces a dep with attribution requirements (e.g. an Apache-2.0-with-NOTICE transitive dep), that's a separate follow-up issue, not a blocker for this scaffolding PR.
- **FR-010.** Pre-commit gate: `cargo fmt --all --check && cargo test && cargo clippy --all-targets -- -D warnings` MUST pass before the PR is opened. For a docs-only change these should all be trivially green; if they aren't, something else is wrong and must be fixed first.

## Scope

**In scope:**

- New files at repo root: `LICENSE`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `CHANGELOG.md`.
- Edits to `README.md`: license badge, Contributing link, Security link, Quickstart phrasing tweaks.
- A one-sentence hint at the top of `.github/PULL_REQUEST_TEMPLATE.md` and the existing `.github/ISSUE_TEMPLATE/` files if they read as Fabrik-internal.
- A note in the PR description recording the lbug license verification finding.

## Out of Scope

- LICENSE headers in source files. MIT does not require per-file headers; the root LICENSE plus the Cargo.toml `license = "MIT"` field is sufficient. Adding per-file headers is a separate, much-larger PR if ever desired.
- README rewrite for an MCP-client audience. Deferred until MCP-over-stdio transport lands.
- MCP-over-stdio transport (separate Fabrik issue).
- Pluggable LLM extractor (separate Fabrik issue).
- Embedder strategy revision (needs decision conversation first).
- Prebuilt-binary release workflow (separate Fabrik issue after the above).
- Updating CLAUDE.md. It's already contributor-friendly and references the right conventions.
- Renaming the project, moving the repo to a new GitHub org, or any other branding work.
- A `NOTICE` file (not required by MIT).
- Backfilling `CHANGELOG.md` with historical entries.

## Edge Cases

- **lbug license is incompatible** (e.g. GPL/AGPL/SSPL/BSL). The PR still merges all other items; a follow-up issue is filed flagging that public launch is blocked. The launch announcement is gated on resolution.
- **Maintainer email not available for CODE_OF_CONDUCT contact**. Default to the security-advisory URL as the contact (it routes to the maintainer privately and is the same channel security researchers use). Document this choice in PR description.
- **README polish accidentally drifts toward rewrite**. The Review stage MUST verify the diff against `User Story 6 Acceptance Scenario 3` — structural sections unchanged, reorder/removal forbidden. If the diff shows significant restructuring, that's a Plan-stage error and the PR should be re-scoped.
- **CI runs `cargo test` and lbug rebuilds from source** despite this being a docs-only change. The recent lbug-cache PR (#115) means the cache should hit on main; this PR's first CI run should be ~7 min on `test`. If it's longer, that's a cache-key issue tracked separately, not a problem to fix in this PR.
- **`SECURITY.md` references GitHub private vulnerability reporting** but the maintainer hasn't enabled it on the repo. Research stage should verify the feature is enabled (Settings → Code security and analysis → Private vulnerability reporting). If not, enable it as part of merging this issue, or fall back to email + flag for follow-up.
- **External-contributor instructions in CONTRIBUTING.md duplicate CLAUDE.md content**. They MUST cite CLAUDE.md rather than duplicate. Duplication leads to drift.
- **PR pre-commit gates fail unexpectedly** for a docs change. This means existing main has a regression unrelated to this PR's intent; debug and fix on main first via a separate worktree, then return to this PR. Do not bypass gates with `--no-verify`.

## Assumptions

- **A1.** The maintainer (verveguy) holds copyright to all current code. No prior contributors have submitted code that would carry their own copyright assertion. (Verifiable via `git log --format='%an' | sort -u` during Research; if other authors appear, the LICENSE copyright line may need to read "Liminis Context Graph contributors" or similar.)
- **A2.** GitHub's private vulnerability reporting is enabled on the repo (or will be enabled as part of merging this issue). If not, fall back to a `mailto:` in SECURITY.md and flag for follow-up.
- **A3.** lbug (v0.16.1, KuzuDB community fork) is under an MIT-compatible license. This is consistent with KuzuDB's original MIT license, but Research MUST verify the fork's current LICENSE file directly rather than assume.
- **A4.** The project is not yet seeking outside contributors actively. CONTRIBUTING.md sets a path but doesn't promise active review SLAs. Pre-1.0 expectations apply throughout the scaffolding text.
- **A5.** The existing `.github/ISSUE_TEMPLATE/` is generally usable for external contributors. Read-only audit will confirm; if it's strongly Fabrik-flavored, a one-sentence hint at the top is the minimum fix in this issue; deeper rework is deferred.
- **A6.** The MIT decision is final for v1. A future relicense or dual-license is possible but out of scope for this issue. The "no CLA, no DCO" stance follows from this decision.

## Success Criteria *(mandatory)*

- **SC-001.** After merge, GitHub renders the right-hand sidebar with "MIT License" detected from the root LICENSE file.
- **SC-002.** After merge, GitHub renders the "Security policy" link on the repo home page pointing to `SECURITY.md`, and the "Code of conduct" link pointing to `CODE_OF_CONDUCT.md`.
- **SC-003.** After merge, all five new files (`LICENSE`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `CHANGELOG.md`) exist at the repo root and are non-empty.
- **SC-004.** The README polish satisfies User Story 6 acceptance: badge present, Contributing + Security links present, Quickstart runnable without `liminis-framework`/`liminis-app`, no structural rewriting.
- **SC-005.** PR body documents the lbug license verification (FR-008) with the license name and a link to the source.
- **SC-006.** Pre-commit gates (`cargo fmt --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`) all pass in CI.
- **SC-007.** A first-time visitor (validate via maintainer self-review against this list) can: see the license, find CONTRIBUTING, find SECURITY, understand they don't need Fabrik to submit a PR.

## Source References

- **`ideas/oss-launch-architecture.md`** — the prior architectural sketch. Questions 1-3 and 6 inform what's deferred from this issue; Question 4 (LadybugDB license) is folded in (FR-008); Question 5 is deferred.
- **`CLAUDE.md`** — the existing contributor guidance. CONTRIBUTING.md should cite it as the deeper reference rather than duplicate.
- **`README.md`** — the existing entry point; this issue polishes it lightly without rewriting.
- **`Cargo.toml` (workspace) and per-crate** — both already declare `license = "MIT"`. This issue makes the actual LICENSE file match what's declared.
- **`docs/adr/`** — 16 existing ADRs. CONTRIBUTING.md should mention this convention.
- **`specs/`** — Spec Kit feature specs. CONTRIBUTING.md should mention this convention for major features.
- **GitHub default community files**: https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/about-community-profiles-for-public-repositories
- **Contributor Covenant 2.1**: https://www.contributor-covenant.org/version/2/1/code_of_conduct/
- **Keep a Changelog 1.1**: https://keepachangelog.com/en/1.1.0/
- **OSI MIT license text**: https://opensource.org/license/mit
