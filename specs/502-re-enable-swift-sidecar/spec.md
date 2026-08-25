# Feature Specification: Re-enable Swift Sidecar CI

**Feature Branch**: `fabrik/issue-502`
**Created**: 2026-08-25
**Status**: Specified
**Input**: User description: "The Swift sidecar has had no CI signal for almost three months. `.github/workflows/swift.yml` is disabled with `if: false`, because GitHub's `macos-latest` runner image was on macOS 15 + Swift 6.1 as of 2026-05-30 while the package (`native/local-inference/`) requires `swift-tools-version: 6.2` and `platforms: [.macOS(.v26)]` for Foundation Models. Every run since has reported skipped, including on release tags v0.13.0 through v0.13.4. The only gate is an honour-system README instruction to run `swift test` locally on a macOS 26 machine before pushing."

## Background

`native/local-inference/` is the macOS Swift sidecar that serves on-device CoreML embeddings and Apple Foundation Models chat completions behind an OpenAI-compatible API. It requires macOS 26 and Swift 6.2 because Foundation Models is a macOS 26 framework — a hard requirement of the package, not something this issue can relax.

`.github/workflows/swift.yml` was disabled on 2026-05-30 (`if: false` on the `swift-test` job) because GitHub's `macos-latest` runner image was still on macOS 15 + Swift 6.1, so `swift test` failed immediately with a tools-version mismatch. Since then, every workflow run — including on every release tag from v0.13.0 through v0.13.4 — has reported `skipped`, giving no CI signal at all.

This absence of CI is not just a coverage gap. It is called out as the mechanism that let two copies of this package (this repo's `native/local-inference/` and `liminis-app/native/local-inference/`) diverge silently, which a separate consolidation effort (#503) now has to reconcile. Re-enabling CI here is what stops that kind of silent drift from recurring once consolidation lands. In the meantime, the only check is the README's instruction to run `swift test` locally on a macOS 26 machine before pushing — unenforced and easy to skip.

A related, non-blocking issue (#501) proposes splitting the sidecar into embeddings / completions / both modes, which would eventually let a completions-only build-and-test job run without any model assets at all. That work is not a prerequisite here — a compile-only (and, where possible, test) CI job is worth having regardless of whether mode selection ever lands.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A sidecar PR gets real CI signal (Priority: P1)

A contributor opens a pull request that touches `native/local-inference/**`. Today, the `Swift sidecar CI` check reports `skipped` regardless of whether the change compiles or breaks existing behavior — the PR merges with no automated signal either way, relying entirely on whether the contributor happened to run `swift test` locally on a macOS 26 machine.

After this change, the same PR triggers a CI job that actually builds the package and runs whatever part of the test suite doesn't depend on the ~400 MB CoreML embedding asset, and reports pass/fail like any other CI check in the repo.

**Why this priority**: This is the core problem statement — three months with zero enforced signal on a component that ships in every release. Without this, nothing else in the issue matters.

**Independent Test**: Open a PR that introduces a compile error in `native/local-inference/Sources/LocalInference/`, and confirm the `Swift sidecar CI` check fails instead of reporting `skipped`. Open a second PR with a passing, well-formed change and confirm the check reports success.

**Acceptance Scenarios**:

1. **Given** a PR that touches a file under `native/local-inference/**`, **When** the PR is opened or updated, **Then** a CI job runs `swift build` for the package on a runner that supports the package's declared `swift-tools-version: 6.2` and `platforms: [.macOS(.v26)]` requirements, and its outcome (pass/fail) is visible as a check on the PR.
2. **Given** the same PR, **When** the CI job runs, **Then** it also executes whatever subset of `swift test` does not require downloading or generating the ~400 MB CoreML embedding model asset produced by `prepare-embedding-assets.sh`, and reports that outcome as part of the same or an adjacent check.
3. **Given** a change that breaks compilation or fails an executed test, **When** CI runs, **Then** the check fails visibly (not `skipped`), giving the contributor a signal before merge.
4. **Given** a new release tag is pushed, **When** the tag's CI runs, **Then** the Swift sidecar CI check reports an actual pass/fail outcome rather than `skipped`.

---

### User Story 2 - No hosted runner works, and that fact is recorded (Priority: P2)

If, after investigation, no GitHub-hosted runner image can build and test macOS 26 / Swift 6.2 code, the workflow should not simply stay silently disabled the way it has been for three months. The gap should be documented in the issue with concrete evidence (what was checked, when, and what the result was), so a future contributor doesn't have to rediscover the same dead end, and so there's a visible trigger to revisit it (e.g., re-check when GitHub updates its runner images, or evaluate a self-hosted runner).

**Why this priority**: This is the fallback path, not the expected outcome — the issue's own investigation note suggests the original blocker may have already aged out since 2026-05-30. But the acceptance criteria explicitly require this outcome to be handled if the primary path isn't available, so it's part of the spec, not optional polish.

**Independent Test**: Simulate or confirm the "no hosted runner available" outcome; check that the issue (or linked documentation reachable from it) states what was tried and what evidence supports the conclusion, rather than the workflow file simply remaining `if: false` with a stale comment.

**Acceptance Scenarios**:

1. **Given** no currently available GitHub-hosted runner offers macOS 26 + Swift 6.2, **When** this is confirmed, **Then** the finding is recorded (in this issue, an ADR, or equivalent linked documentation) with the evidence that supports it (e.g., runner image version/changelog reference, an actual failed run).
2. **Given** that recorded finding, **When** a future contributor reads it, **Then** they can tell what was checked and why the workflow is (or isn't) enabled, without needing to re-derive it from scratch.

---

### Edge Cases

- A PR touches `native/local-inference/**` but the change is documentation-only within that directory (e.g., README) — the existing path filter already triggers the workflow for any file under that path; whether that's desirable is unchanged by this issue.
- A test in the current suite turns out to depend on the real (non-stub) CoreML model or on live Apple Foundation Models availability (Apple Intelligence enabled and downloaded) rather than the in-tree stub fixtures — such a test cannot run in a CI environment that has neither, and must not be force-included in the CI-run subset.
- The package's `swift-tools-version` or minimum platform requirement changes in the future (e.g., bumps again ahead of runner support) — CI should fail clearly (tools-version mismatch) rather than silently skip, so the gap is visible immediately rather than discovered three months later.
- A release-tag push and a PR both touch `native/local-inference/**` in the same CI run window — both should get equivalent CI signal; the issue's acceptance criteria call out release tags specifically because they previously had none.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: CI MUST run `swift build` for the `native/local-inference` package on pull requests that touch `native/local-inference/**` (or the workflow file itself), using a runner/toolchain combination that satisfies the package's declared `swift-tools-version: 6.2` and `platforms: [.macOS(.v26)]` requirements.
- **FR-002**: CI MUST also run this build on pushes that touch the same paths (matching the existing `swift.yml` trigger configuration), including on release tags, so that release artifacts are no longer built and shipped with zero CI signal.
- **FR-003**: CI MUST run whichever subset of `swift test` can execute without downloading, generating, or otherwise depending on the ~400 MB CoreML embedding model asset produced by `prepare-embedding-assets.sh`.
- **FR-004**: CI MUST NOT require Apple Intelligence / Foundation Models to be enabled or downloaded on the runner in order for the executed test subset to pass — tests that only exercise the on-device chat-completions path in an "unavailable" fallback mode are in scope; tests that require a live, available Foundation Models instance are out of scope for this CI job.
- **FR-005**: The workflow's CI check outcome MUST be reported as pass or fail (or otherwise visibly actionable) rather than the current unconditional `skipped` state, for every run the existing path filters would trigger.
- **FR-006**: If, after investigation, no currently available GitHub-hosted runner can satisfy FR-001's toolchain/platform requirement, that finding MUST be recorded with supporting evidence in this issue or in linked documentation reachable from it, rather than the workflow being left disabled without an explanation newer than the original 2026-05-30 note.
- **FR-007**: Enabling this CI MUST NOT require committing the ~400 MB CoreML model asset to the repository or downloading it as part of the CI job.

### Key Entities

- **Swift sidecar CI workflow** (`.github/workflows/swift.yml`): The GitHub Actions workflow gating `native/local-inference/**` changes; currently disabled via `if: false` on its sole job.
- **CoreML embedding model asset**: The ~400 MB `bge-base-en-v1.5.mlpackage`, produced locally by `prepare-embedding-assets.sh`, not committed to the repo, and not required by the existing stub-fixture-based test suite under `Tests/LocalInferenceTests/Fixtures/`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The next pull request that touches `native/local-inference/**` shows a `swift build` (and, where applicable, `swift test`) CI check with a real pass/fail outcome, not `skipped`.
- **SC-002**: The next release tag push shows the same real pass/fail CI outcome for the Swift sidecar, closing the gap observed across v0.13.0–v0.13.4.
- **SC-003**: A contributor can determine, from CI alone (no local macOS 26 machine required), whether a sidecar change compiles and passes the asset-independent portion of the test suite.
- **SC-004**: If no hosted runner can satisfy the toolchain requirement, that conclusion is documented with dated, checkable evidence rather than being inferred from a silent `skipped` status, and running the check has clear no-signal (in `swift.yml`) vs recorded-finding (this issue) states.

## Assumptions

- Issue #501 (embeddings / completions / both mode selection) is related but explicitly not a prerequisite for this issue, per the original issue text — this issue's scope does not depend on mode selection landing first.
- Issue #503 (consolidating the sidecar so this repo is the source of truth, removing the diverged copy in `liminis-app`) is a separate effort. This issue concerns CI for the copy already in this repo at `native/local-inference/**` and does not itself perform or block on consolidation.
- The existing test suite under `Tests/LocalInferenceTests/` already uses stub `.mlpackage` fixtures rather than the real downloaded model for most or all of its tests (per the package README); which specific tests, if any, still require real assets or live Foundation Models availability — and are therefore excluded from the CI-run subset — is a determination left to the Research stage, not this spec.
- "Hosted runner" means a GitHub-hosted Actions runner image. A self-hosted macOS 26 runner is in scope only as a fallback investigated if no hosted option satisfies FR-001, consistent with the issue's stated option ordering (re-check hosted runner → pin a newer runner label → self-hosted as last resort).
- Whether the resulting CI check becomes a required status check (blocking merge) versus a non-blocking reported check is an implementation/policy decision left to the Plan stage; this spec requires only that a real outcome be visible, per FR-005.

## Out of Scope

- Implementing mode selection (embeddings / completions / both) from #501.
- Performing the sidecar consolidation from #503.
- Running the portion of `swift test` that genuinely requires the ~400 MB downloaded CoreML model or a live, available Foundation Models instance, in CI.
- Provisioning or maintaining a self-hosted macOS 26 runner, unless investigation shows no hosted runner can satisfy the requirement (see FR-006 and Assumptions).

## Source References

- `.github/workflows/swift.yml` — the currently disabled workflow
- `native/local-inference/Package.swift` — declares `swift-tools-version: 6.2` and `platforms: [.macOS(.v26)]`
- `native/local-inference/README.md` — documents the local `swift test` honour-system gate and the stub-fixture test approach
- `native/local-inference/Tests/LocalInferenceTests/` — existing test suite, largely built on stub `.mlpackage` fixtures
- Issue #501 — mode selection (embeddings / completions / both), related but non-blocking
- Issue #503 — sidecar consolidation, the issue this CI gap is cited as having enabled
