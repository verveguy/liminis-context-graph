# Feature Specification: Consolidate the Swift Sidecar — lcg is Source of Truth

**Feature Branch**: `fabrik/issue-503`
**Created**: 2026-08-25
**Status**: Specified
**Input**: User description: "The Swift sidecar exists as two diverged copies in two repositories, and the one that ships (`liminis/liminis-app/native/local-inference/`) is the older one — the copy in `liminis-context-graph/native/local-inference/` is not consumed by anything. `liminis-context-graph` is to become the source of truth: reconcile the 14-line divergence deliberately (not by assuming the newer side wins, since the app's copy is what has actually run in production), and establish how the app will obtain a built binary going forward (released artifact, CI build step, or documented local build path). The copy in `liminis/liminis-app` is to be deleted and the app repointed at whatever distribution mechanism is chosen — that part is a companion change in the other repo, not doable from here."

## Background

The Swift sidecar (`native/local-inference/`) implements the OpenAI-compatible embedder and chat-completions contract this repo depends on (ADR-0006, ADR-0016). It currently exists as two independently-maintained copies:

| copy | source last changed | binary built | consumed by |
|---|---|---|---|
| `liminis-context-graph/native/local-inference/` (this repo) | 2026-07-12 | 2026-07-25 | **nothing** |
| `liminis/liminis-app/native/local-inference/` (companion app repo) | 2026-06-22 | 2026-06-21 | the Electron app |

The Electron app spawns its own copy — `liminis-app/native/local-inference/.build/arm64-apple-macosx/release/LocalInference` when unpackaged, `resources/bin/local-inference` when packaged. A contributor who improves the copy in this repo changes nothing that actually ships. This repo's own README currently documents this as an intentional, temporary arrangement ("A near-identical copy lives at `liminis-app/native/local-inference/`... The two copies are intentionally maintained side-by-side for now").

The two copies have drifted: **14 lines**, confined to `FoundationModelsAdapter.swift` and `InferenceAdapter.swift` (both on the chat-completions side), differ between them as of issue filing. `EmbeddingsHandler.swift` is byte-identical between the copies as of issue filing, so the embedding path is unaffected today. There is no sync mechanism and no CI on either copy — `.github/workflows/swift.yml` in this repo is disabled (`if: false`) because GitHub's `macos-latest` runner image is still on macOS 15 + Swift 6.1, while the package requires macOS 26 + Swift 6.2 (Foundation Models). That gap is why the drift went unnoticed for two months, and is luck rather than design.

This issue makes `liminis-context-graph` the sole source of truth for the sidecar: reconcile the drift deliberately, then establish how the app is meant to obtain a binary built from this repo's source going forward, so that a contributor who fixes something here actually ships that fix.

**Cross-repo scope**: this repo cannot delete `liminis-app/native/local-inference/` or edit `getLocalInferenceBinaryPath()` — those live in `verveguy/liminis`, a separate repository, and are explicitly called out in the source issue as "a companion change, not doable from this repo." This spec covers only the portion of the work that can be delivered from this repo's PR; see Out of Scope below for the companion-repo portion and how it is expected to proceed.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A fix made in this repo's sidecar copy is the one that ships (Priority: P1)

A contributor fixes a bug or makes an improvement in `native/local-inference/`'s chat-completions handling. Once this issue lands (and the companion change in `verveguy/liminis` follows it), that fix is what the app actually runs — not silently discarded because the app was still building from its own stale copy.

**Why this priority**: This is the core problem statement. Every other requirement in this issue exists to make this true.

**Independent Test**: After reconciliation, diff this repo's `FoundationModelsAdapter.swift` and `InferenceAdapter.swift` against the pre-reconciliation `liminis-app` copy (the one that has been running in production). Confirm that every one of the 14 differing lines has been deliberately resolved — kept, changed, or intentionally overridden — with the reasoning recorded, rather than silently defaulted to whichever side happened to be newer.

**Acceptance Scenarios**:

1. **Given** the 14-line divergence between the two copies, **When** reconciliation is complete, **Then** each diverged hunk has an explicit, recorded resolution (which side was kept and why), grounded in which behavior has actually been running in production — not simply "the newer copy wins."
2. **Given** `EmbeddingsHandler.swift` was byte-identical between the copies at issue filing, **When** reconciliation runs, **Then** the implementation re-verifies that this file is still identical before treating it as unaffected (drift may have continued since filing).
3. **Given** the reconciled source, **When** `swift build -c release` and `swift test` are run locally on a macOS 26 / Swift 6.2 machine, **Then** both succeed (this is the verification gate while `swift.yml` CI remains disabled).

---

### User Story 2 - The app can obtain a binary built from this repo's source (Priority: P1)

A maintainer needs a way to get a `LocalInference` binary — built from this repo's (now-authoritative) source — into the app, without the app carrying its own copy of the Swift source.

**Why this priority**: Without this, consolidating the source doesn't consolidate what ships; the app would still need *some* way to get a binary, and today the only way it knows is "build from its own vendored copy."

**Independent Test**: Following the documented mechanism (released artifact, CI build step, or documented local build procedure — chosen during Research/Plan), a maintainer can produce a `LocalInference` binary from this repo's source alone, without checking out or modifying anything in `verveguy/liminis`.

**Acceptance Scenarios**:

1. **Given** this repo's reconciled sidecar source, **When** a maintainer follows the documented build/distribution path, **Then** they obtain a `LocalInference` binary suitable for the app to spawn.
2. **Given** the chosen mechanism, **When** it is documented (e.g., in `native/local-inference/README.md`), **Then** the documentation is specific enough that someone unfamiliar with this issue can execute it without re-deriving context.
3. **Given** the sidecar's macOS 26 / Swift 6.2 requirement and the current GHA `macos-latest` image gap (macOS 15 / Swift 6.1), **When** the mechanism is chosen, **Then** it accounts for this constraint rather than assuming a standard GitHub-hosted macOS runner can build it today.

---

### User Story 3 - The reconciliation reasoning is auditable later (Priority: P2)

A future maintainer investigating why the sidecar behaves a certain way (e.g., a Foundation Models adapter quirk) wants to know whether that behavior was a deliberate choice made during consolidation, and why — without having to dig through two separate, now-partially-dead git histories.

**Why this priority**: Valuable for long-term maintainability, but the consolidation itself (Stories 1–2) delivers the actual fix; this story is about not losing the "why" once it's made.

**Independent Test**: Read the PR that closes this issue (or a referenced ADR). For each of the 14 diverged lines, confirm the resolution and its rationale are stated somewhere findable from the PR.

**Acceptance Scenarios**:

1. **Given** the completed reconciliation, **When** a reader opens the PR description (or a linked ADR), **Then** each resolved hunk's decision and reasoning is recorded there — not only implicit in the diff.

---

### Edge Cases

- What happens when a diverged hunk can't be cleanly attributed as "one side is simply correct" — e.g., each side fixes a different bug? Both fixes should be preserved where possible; where they conflict, the choice and the tradeoff must be recorded (Story 3).
- What happens if `EmbeddingsHandler.swift` has drifted between issue filing and implementation (contradicting the "byte-identical today" premise)? Treat it as in scope for reconciliation like the other two files — the premise was a snapshot at filing time, not a guarantee.
- What happens if GitHub's `macos-latest` runner gains Swift 6.2 support while this issue is being implemented? Re-enabling `swift.yml` becomes newly possible but remains a follow-up, not a requirement of this issue (see Out of Scope).
- What happens to this repo's README claim that a "near-identical copy... is intentionally maintained side-by-side for now"? It must be updated to reflect sole ownership once this issue lands — it is a statement of the very arrangement this issue undoes.
- What happens if the production (`liminis-app`) behavior for a given hunk is itself the bug — i.e., the newer copy in this repo already fixed something the app is still shipping broken? The default is to preserve current production behavior unless the reconciliation deliberately chooses to change it, in which case that choice and its justification must be recorded (Story 3) — "production wins by default" is a tie-breaker, not a mandate to keep known bugs.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The sidecar source in this repo's `native/local-inference/` MUST become the sole authoritative copy: every one of the (at least) 14 diverged lines between this repo's copy and the `liminis-app` copy, confined to `FoundationModelsAdapter.swift` and `InferenceAdapter.swift`, MUST be resolved deliberately, with the production (`liminis-app`) behavior as the default unless a documented reason justifies otherwise.
- **FR-002**: `EmbeddingsHandler.swift` MUST be re-verified as identical between the two copies (as of implementation time, not just as of issue filing) before being treated as unaffected by this reconciliation; if it has drifted, it is in scope too.
- **FR-003**: This repo MUST establish and document a mechanism by which `verveguy/liminis` can obtain a `LocalInference` binary built from this repo's source, without vendoring or embedding the Swift source in the app repo. The specific mechanism (a released artifact, a CI build step, or a documented local build procedure) is a Research/Plan-stage decision, constrained by the current toolchain gap: the package requires macOS 26 + Swift 6.2, and GitHub's hosted `macos-latest` runner image does not yet provide that (see Background).
- **FR-004**: `native/local-inference/README.md` MUST be updated to remove the "two copies maintained side-by-side" language and instead state that this repo is the sole source of truth, plus describe the distribution mechanism established under FR-003.
- **FR-005**: After reconciliation, `swift build -c release` and `swift test` MUST both succeed when run locally on a macOS 26 / Swift 6.2 machine. This is the verification gate for this issue's Implement/Review stages while `swift.yml` CI remains disabled (see Out of Scope).
- **FR-006**: The resolution and rationale for each diverged hunk MUST be recorded somewhere reachable from the PR that closes this issue (PR description, an ADR, or equivalent) — not left implicit in the diff.
- **FR-007**: The PR that closes this issue MUST clearly describe the companion actions still required in `verveguy/liminis` (deleting its copy of `native/local-inference/`, repointing `getLocalInferenceBinaryPath()` at the chosen distribution mechanism, and renaming the `GRAPHITI_EMBEDDING_DIM` env var to `LCG_EMBEDDING_DIM` in `context-graph-lifecycle.ts`) in enough detail that they can be executed without re-deriving context from this issue.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Every one of the diverged lines between this repo's sidecar copy and the pre-reconciliation `liminis-app` copy has an explicit, recorded resolution — none are left as an unexamined "newer wins" default.
- **SC-002**: A maintainer unfamiliar with this issue can follow `native/local-inference/README.md` alone to obtain a `LocalInference` binary built from this repo's source, without cloning or modifying `verveguy/liminis`.
- **SC-003**: `swift build -c release` and `swift test` both pass against the reconciled source on a macOS 26 / Swift 6.2 machine.
- **SC-004**: The instructions left for the companion `verveguy/liminis` change (copy deletion, binary path repointing, env var rename) are specific enough to execute without needing to re-read this issue's discussion.
- **SC-005** *(cross-repo, tracked but not solely gated by this PR)*: Once the companion change in `verveguy/liminis` also lands, exactly one copy of the sidecar source exists across both repositories, and the app runs a binary built from it.

## Assumptions

- The "14 lines / two files" divergence figure reported in the issue is a snapshot as of filing; Research will re-diff against current `liminis-app` state, since time has passed and the two copies have no sync mechanism.
- Reading `liminis-app/native/local-inference/` requires access to the `verveguy/liminis` repository from the Research stage (e.g., a checkout or fetched diff); it is not available in this worktree.
- The companion change in `verveguy/liminis` (deleting its copy, repointing `getLocalInferenceBinaryPath()`, renaming the env var) will be tracked and executed as separate work in that repository — most likely its own issue there — and is not blocked on waiting for this repo's PR to merge first, though it does depend on this repo's chosen distribution mechanism being in place.
- No GitHub-hosted macOS runner image gains Swift 6.2 support during this issue's implementation; if one does, re-enabling `swift.yml` becomes possible but remains optional follow-up work for this issue rather than a new requirement.
- The distribution mechanism only needs to target macOS arm64 (Apple Silicon), consistent with the sidecar's macOS-only, Foundation-Models-dependent nature.
- "Mode selection" work referenced in the issue's Sequencing section is separate, future work; this issue does not need to anticipate its design, only avoid landing after it (per the issue's own sequencing note).

## Out of Scope

- Deleting `liminis-app/native/local-inference/` and repointing `getLocalInferenceBinaryPath()` — lives in `verveguy/liminis`, a separate repository; not doable from this repo's PR.
- Renaming `GRAPHITI_EMBEDDING_DIM` → `LCG_EMBEDDING_DIM` in `context-graph-lifecycle.ts` — same reason; that file lives in `verveguy/liminis`.
- Re-enabling `.github/workflows/swift.yml`. It is currently disabled (`if: false`) because GitHub's `macos-latest` runner image lacks the required Swift 6.2 toolchain; that is an external constraint this issue cannot resolve. Re-enabling it once GitHub's image catches up is recommended as an immediate follow-up, since the absence of CI is what let the two copies drift silently for two months in the first place.
- Designing or implementing "mode selection" — a separate, future feature; the issue only asks that this consolidation land first so that work isn't done twice against a still-diverged sidecar.

## Source References

- `native/local-inference/README.md`
- `native/local-inference/Sources/LocalInference/FoundationModelsAdapter.swift`
- `native/local-inference/Sources/LocalInference/InferenceAdapter.swift`
- `native/local-inference/Sources/LocalInference/EmbeddingsHandler.swift`
- `.github/workflows/swift.yml`
- `docs/adr/0006-embedder-http-contract.md`
- `docs/adr/0016-oai-embedding-contract-uds-transport.md`
- `specs/132-publish-versioned-macos-release/spec.md` — prior art in this repo for distributing a macOS binary to `verveguy/liminis` via a GitHub Release artifact; relevant precedent for the Research/Plan-stage distribution-mechanism decision (FR-003), not a prescribed answer.
