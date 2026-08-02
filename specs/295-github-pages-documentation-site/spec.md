# Feature Specification: GitHub Pages documentation site

**Feature Branch**: `fabrik/issue-295`
**Created**: 2026-08-02
**Status**: Draft
**Input**: User description: "Build a GitHub Pages documentation site for liminis-context-graph, following `~/dev/fabrik` as the reference implementation, splitting reference material out of README.md so it stops silently drifting from the code."

## Background

`liminis-context-graph` has no documentation site. Everything a user needs is in a single ~950-line `README.md` plus 58 files under `docs/` (52 of them ADRs). That structure has a demonstrated failure mode: PR #294 found that across the 45 PRs merged for 0.11.0, the README had undercounted the JSON-RPC surface, omitted six environment variables the code reads, and `docs/telemetry.md` had described two telemetry events as "not yet emitted" while documenting a payload whose fields do not exist. Nobody audits a 950-line file, so nobody caught it.

This matters more than usual for this project. It is built from source by outside users — six community-reported issues drove the 0.11.0 release — and its documentation is read by coding agents pointed at the repo. A doc that is confidently wrong is worse than one that is missing.

`~/dev/fabrik` (public at `handarbeit/fabrik`) is the reference implementation to follow: Jekyll served directly from `docs/` via GitHub Pages "deploy from branch", custom `_layouts`, kramdown/GFM with rouge highlighting, `jekyll-feed` + `jekyll-seo-tag`, and an `exclude:` list that keeps internal engineering material in the repo but off the published site. It also ships `docs/llms.txt` and `docs/llms-full.txt` with a `docs-drift.yml` workflow that regenerates the latter on every PR and fails if it is stale.

**Tone: reference documentation, not marketing.** No landing-page pitch, no feature-benefit copy, no testimonials. Facts, tables, and working examples.

## Decisions already made

- **URL**: `https://verveguy.github.io/liminis-context-graph`. No custom domain, no `CNAME`.
- **Scope**: full split. Reference material moves out of `README.md` onto the site; the README becomes a short overview plus links.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — A newcomer installs and runs the service (Priority: P1)

Someone who has just found the repo can get the service running without reading source.

**Acceptance Scenarios**:

1. **Given** the published site, **When** a visitor follows the getting-started page, **Then** they can install and start the binary against a workspace using only instructions on that page.
2. **Given** the repo landing page, **When** a visitor reads only `README.md`, **Then** they can still install and run — the README keeps a working quickstart and does not merely redirect.

### User Story 2 — An operator configures and monitors a deployment (Priority: P1)

**Acceptance Scenarios**:

1. **Given** the configuration page, **When** an operator looks up any environment variable read by the code, **Then** it is present with its default and effect.
2. **Given** the telemetry page, **When** an operator builds a parser from a documented event payload, **Then** the field names and types match what the service emits.

### User Story 3 — A coding agent answers questions about the project (Priority: P1)

An agent pointed at the repo or the site gets accurate, current answers.

**Acceptance Scenarios**:

1. **Given** `docs/llms.txt` and `docs/llms-full.txt`, **When** an agent ingests them, **Then** they reflect the current published documentation.
2. **Given** a PR that changes published docs without regenerating `llms-full.txt`, **When** CI runs, **Then** the build fails with an actionable message.

### User Story 4 — A contributor finds the decision record behind a behaviour (Priority: P2)

**Acceptance Scenarios**:

1. **Given** the site's ADR index, **When** a contributor follows any entry, **Then** the ADR renders and its internal links resolve.

---

### Edge Cases

- ADRs are immutable decision records and several describe superseded behaviour (e.g. ADR-0025's lazy index build, annotated in #294). Publishing them must not present superseded decisions as current — the ADR index needs framing that says these are historical records, not current documentation.
- `CLAUDE.md` is agent guidance, not user documentation. It should not be published, but `llms.txt` may reasonably reference it.
- Some README content is maintainer-only (the release runbook). It should move to `CONTRIBUTING.md` or an excluded page, not onto the public site.
- The MCP tool descriptions in `crates/service/src/mcp/tools.rs` are user-facing documentation that lives in code. The site should point at them rather than copy them, or they become a fourth place to drift.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The site MUST be Jekyll served from `docs/` via GitHub Pages "deploy from branch", following `~/dev/fabrik`'s structure (`_config.yml`, `_layouts/`, `Gemfile`).
- **FR-002**: Published pages MUST cover, at minimum: getting started/install; configuration reference (every environment variable and CLI flag); IPC + MCP method reference; telemetry reference; ontology; operations (WAL, recovery, degraded mode); and an ADR index.
- **FR-003**: `README.md` MUST be reduced to an overview: what the project is, install, a working quickstart, and links into the site. Reference sections that move MUST NOT be duplicated in both places — one canonical home each.
- **FR-004**: `_config.yml` MUST carry an `exclude:` list so internal engineering material (`specs/`, and any design/postmortem directories) stays in the repo but is not published.
- **FR-005**: ADR files MUST stay at their current paths. Any existing relative link to `docs/adr/*` from anywhere in the repo must continue to resolve.
- **FR-006**: The site MUST ship `docs/llms.txt` and `docs/llms-full.txt`, generated by a script in `scripts/`, following fabrik's precedent.
- **FR-007**: A CI workflow MUST regenerate `llms-full.txt` on every pull request and fail if the committed copy is stale, with a message naming the script to run.
- **FR-008**: Content MUST be reference-style. No marketing copy, no benefit-oriented headlines, no social-preview/OG imagery beyond what Pages requires.
- **FR-009**: Every internal link on the published site MUST resolve, and every code example MUST be one that runs against the released binary.
- **FR-010**: The site MUST state the version it documents, so a reader can tell whether it describes the current release or `main`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `https://v3rv.com/liminis-context-graph/` serves the site from `main`. (Originally written as `https://verveguy.github.io/liminis-context-graph` when the default Pages URL was picked. That address can never serve: the account carries a Pages custom domain, so every `verveguy.github.io` request 301-redirects to `v3rv.com`, and `gh api repos/verveguy/liminis-context-graph/pages` reports `html_url` as `http://v3rv.com/liminis-context-graph/` while this repo's own `cname` is null — nothing in this repo can change that. Corrected to the address that actually serves. A dedicated subdomain is a later change and does not block this issue.)
- **SC-002**: Every environment variable read via `env::var`/`lcg_env_var` under `crates/*/src` appears in the configuration reference. At time of writing that is 26 (by Implement, the actual count had grown to 27 — see `docs/configuration.md`, which documents whatever `crates/*/src` currently reads, not this fixed baseline).
- **SC-003**: Every `TelemetryEvent` variant has a documented section whose field list matches the struct. At time of writing that is 11 (by Implement, the actual count had grown to 12, including `ExtractionFailure` — see `docs/telemetry.md`).
- **SC-004**: `README.md` is under 250 lines, down from ~950, and still contains a quickstart that works standalone.
- **SC-005**: A PR touching published docs without regenerating `llms-full.txt` fails CI.
- **SC-006**: No internal link on the published site 404s.

## Assumptions

- GitHub Pages is enabled for the repository with source set to `main` / `docs`.
- No custom domain, so no `CNAME` and no DNS work.
- Jekyll's GitHub Pages plugin allowlist is sufficient; no custom plugins needed.

## Out of Scope

- Custom domain and DNS.
- Versioned documentation (docs for multiple releases side by side). The site documents one version; FR-010 only requires stating which.
- Publishing generated API documentation (`cargo doc`).
- Any redesign of the ADR numbering or content beyond framing them correctly in an index.

## Source References

- `~/dev/fabrik` — reference implementation (`docs/_config.yml`, `docs/_layouts/`, `.github/workflows/docs-drift.yml`, `scripts/generate-llms-full.sh`)
- PR #294 — the drift audit motivating this, and the two mechanical checks it verified by hand (26 env vars, 11 telemetry events) which SC-002/SC-003 make permanent
