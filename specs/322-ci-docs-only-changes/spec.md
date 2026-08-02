# Feature Specification: docs-only changes pay a full 15–18 minute Rust CI cycle

**Feature Branch**: `fabrik/issue-322`
**Created**: 2026-08-02
**Status**: Draft
**Input**: User description: "`test (ubuntu-latest)` is the sole required status check on `main`, and `ci.yml` triggers it on every `pull_request` regardless of what changed. A PR touching zero `.rs` and zero `Cargo.*` files still pays a full release build and test suite — 15–18 minutes — to prove nothing about itself."

## Background

`test (ubuntu-latest)` is the sole required status check on `main` (confirmed via branch protection: `required_status_checks.contexts == ["test (ubuntu-latest)"]`, `enforce_admins.enabled == true`), and `.github/workflows/ci.yml` triggers it on every `pull_request` regardless of what changed. A PR touching zero `.rs` and zero `Cargo.*` files still pays a full release build and test suite — 15–18 minutes — to prove nothing about itself.

Concrete instance: PR #320 (the documentation site, issue #295) changed 23 files, none of them Rust. Its two *meaningful* checks — "Build site and check internal links" (18s) and "Verify llms-full.txt is up to date" (7s) — were green in under half a minute. It still could not merge, because `test (ubuntu-latest)` was mid-run. `gh pr merge --admin` does not help: `enforce_admins=true`, so branch protection applies to maintainers too.

This is a different lever from #316. That issue asks how to make the suite itself faster; this one asks not to run it at all when it cannot be affected by the diff. The two are complementary, and this one is much cheaper.

### The trap this must avoid

**A required status check that never runs leaves the PR permanently blocked.** If `ci.yml` gains a `paths-ignore` that filters the *workflow* out for docs-only PRs, the `test (ubuntu-latest)` check never reports at all, and branch protection waits for it forever. That converts a 15-minute delay into an unmergeable PR — strictly worse than today.

The workflow must therefore still trigger; only the *expensive job* may be skipped. GitHub treats a job skipped via `if:` as satisfying a required check, whereas a workflow filtered out by `paths`/`paths-ignore` never produces the check at all. Any implementation must demonstrate this distinction is handled, not assumed.

### A second constraint specific to this repo

`ci.yml`'s trigger block is load-bearing and carries an in-file explanation: `push` is scoped to `branches: [main]` because `on: {push, pull_request}` previously produced two concurrent runs of the same commit that raced the shared lbug build cache (same cache key), intermittently linking against a half-written archive set and failing with `duplicate symbol: yyjson_*` / `antlr4::*`. Any change to triggers or job structure must preserve that property — one CI run per PR commit, one cache consumer.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A docs-only PR merges without waiting on the Rust suite (Priority: P1)

A contributor opens a PR that changes only documentation, spec, or markdown files. CI runs, the required `test (ubuntu-latest)` check reports success quickly (its Rust build/test job was skipped rather than executed), and the PR becomes mergeable without anyone waiting on a 15–18 minute Rust suite that cannot be affected by the diff.

**Why this priority**: This is the core complaint in the issue — it is the scenario that currently blocks doc-only PRs like #320 for no benefit.

**Independent Test**: Can be fully tested by opening a real PR that touches only files outside the Rust/manifest/lockfile/CI-build classification (e.g., a markdown-only change), observing that the Rust build/test job is skipped, and confirming the PR is mergeable per branch protection — delivers the "docs-only PRs merge fast" outcome on its own.

**Acceptance Scenarios**:

1. **Given** a PR changing only documentation, spec, or markdown files, **When** CI runs, **Then** the required check reports success without executing the Rust build/test suite.
2. **Given** that same PR, **When** branch protection evaluates it, **Then** it is mergeable — the required check is satisfied, not merely absent.

---

### User Story 2 - A code change still gets full coverage (Priority: P1)

A contributor opens a PR that touches any Rust source, manifest, lockfile, or CI/build file — or a PR that touches both docs and code. CI runs the exact same full `cargo test --release` suite as today, with no reduction in coverage or duration.

**Why this priority**: Equal in priority to Story 1 — the fast path must never come at the cost of the safety net. A regression here (code changes silently skipping tests) is worse than the problem this issue fixes.

**Independent Test**: Can be fully tested by opening a PR that adds/modifies a `.rs` file (and separately, one that touches both a `.rs` file and a doc file), and confirming the full suite executes exactly as it does today — delivers "code changes are never under-tested" independent of whether Story 1 is implemented.

**Acceptance Scenarios**:

1. **Given** a PR touching any `.rs`, `Cargo.toml`, `Cargo.lock`, or build/workflow file, **When** CI runs, **Then** the full `cargo test --release` suite executes exactly as today.
2. **Given** a PR touching *both* docs and code, **When** CI runs, **Then** the full suite executes — mixed PRs take the conservative path.

---

### User Story 3 - The docs checks stay meaningful (Priority: P2)

On a docs-only PR, the checks that actually validate the change (docs-site build, internal link check, `llms-full.txt` drift check) continue to run and can still fail the PR — skipping the Rust suite must not become an excuse to skip validation altogether.

**Why this priority**: Lower priority than Stories 1–2 because these checks already exist and already run independently today (per the PR #320 example); this story only asserts they must keep running once Story 1 is implemented, not that they need to be newly built.

**Independent Test**: Can be fully tested by introducing a broken internal link or stale `llms-full.txt` in a docs-only PR and confirming CI still fails it, even though the Rust suite was skipped.

**Acceptance Scenarios**:

1. **Given** a docs-only PR, **When** CI runs, **Then** the docs-site build and `llms-full.txt` drift checks still run and can still fail the PR.

---

### Edge Cases

- A PR that only edits `.github/workflows/ci.yml` itself must run the full suite; it is not a docs change.
- Renames spanning both categories (moving a `.rs` file into `docs/`) must take the conservative path.
- `scripts/**` is ambiguous: `scripts/generate-docs-llms-full.sh` is docs tooling, but other scripts may affect the build. Classify explicitly rather than by directory.
- Merge-queue / merge-train trial branches: if the batch contains any code PR, the combined validation must run the full suite. (This repo does not currently use GitHub's merge queue feature — see Assumptions — so this edge case has no live trigger today, but the classification logic must not assume a single-PR-per-run model in a way that would need rework if merge queue is adopted later.)

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A PR whose diff touches no Rust, manifest, lockfile, or CI/build file MUST NOT execute the Rust build/test job.
- **FR-002**: The required status check MUST still report a result on such a PR, so branch protection is satisfied. Skipping the *job* is acceptable; filtering out the *workflow* is not.
- **FR-003**: Any PR touching `.rs`, `Cargo.toml`, `Cargo.lock`, `.cargo/**`, `build.rs`, or `.github/workflows/**` MUST run the full suite unchanged.
- **FR-004**: A mixed docs+code PR MUST run the full suite.
- **FR-005**: The existing one-run-per-commit property MUST be preserved — no reintroduction of concurrent `push` + `pull_request` runs sharing the lbug cache key.
- **FR-006**: The path classification MUST live in one place and be readable, not duplicated across jobs where it can drift.
- **FR-007**: The PR that implements this issue MUST demonstrate both paths empirically — a real docs-only PR that skips the Rust job and reaches a mergeable state, and a real code-touching PR that runs the full suite — rather than asserting the behavior in prose or in a unit test of the workflow file alone.

### Key Entities

Not applicable — this feature changes CI workflow trigger/job logic and does not introduce or modify any data entities.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A docs-only PR reaches a mergeable state in under two minutes.
- **SC-002**: A code-touching PR's CI duration is unchanged from today.
- **SC-003**: Branch protection reports the required check as satisfied on a docs-only PR, verified on a real PR rather than reasoned about.
- **SC-004**: `.rs` changes cannot reach `main` without the full suite having run — demonstrated by an intentional test case.

## Assumptions

- This repository does not currently have GitHub's merge queue / merge-train feature enabled on `main` (confirmed via `gh api repos/.../branches/main/protection`, which shows a plain required-status-check configuration with no merge-queue-specific settings). The merge-queue edge case above is retained for completeness and future-proofing but is not exercised by any success criterion today.
- "Rust, manifest, lockfile, or CI/build file" (FR-001, FR-003) is the same classification named explicitly in FR-003: `.rs`, `Cargo.toml`, `Cargo.lock`, `.cargo/**`, `build.rs`, `.github/workflows/**`. The exact full allow/deny list (including how top-level `scripts/**` and `crates/eval/scripts/**` are each classified, per the Edge Cases entry) is a classification-table detail for the Research/Plan stage, not a product decision open for negotiation here.
- The docs-site build and `llms-full.txt` drift-check jobs referenced in User Story 3 and the Background (from issue #295 / PR #320) are separate, already-independent CI workflow(s) that are not part of `ci.yml`'s `test` job and are not required status checks today; this issue does not change their triggers, only the Rust test job's.
- "Reaches a mergeable state" (SC-001) means branch protection shows all required checks green and no other blocking condition (merge conflicts, review requirements) applies — it does not mean the PR is auto-merged.

## Out of Scope

- Reducing the suite's own 15–18 minute runtime (#316).
- Changing what the suite covers.

## Source References

- #316 — the complementary lever: make the suite itself faster.
- PR #320 / issue #295 — the motivating case: 23 files, zero Rust, blocked 15 min on a Rust suite.
- `.github/workflows/ci.yml` — the trigger-block comment documenting the lbug cache race that constrains FR-005.
