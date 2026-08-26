# ADR-0503: `liminis-context-graph` Is the Sole Source of Truth for the Swift Sidecar

**Date**: 2026-08-25
**Status**: Accepted

## Context

The Swift sidecar (`native/local-inference/`) — the OpenAI-compatible embedder and
chat-completions process this repo's ADR-0006/ADR-0016 contract targets — existed as two
independently-maintained copies:

| copy | source last changed | binary built | consumed by |
|---|---|---|---|
| `liminis-context-graph/native/local-inference/` (this repo) | 2026-07-12 | 2026-07-25 | nothing |
| `liminis-app/native/local-inference/` (the `verveguy/liminis` app repo) | 2026-06-22 | 2026-06-21 | the Electron app |

The app spawns its own vendored copy (`getLocalInferenceBinaryPath()` in
`liminis-app/src/main/local-inference-lifecycle.ts`), never this repo's. A fix made here
changed nothing that actually ran in production. There was no sync mechanism between the two
copies and no CI on either — `.github/workflows/swift.yml` in this repo has been disabled
(`if: false`) since commit `cf1a873b` (2026-05-30) because, at the time, GitHub's
`macos-latest` runner image was still on macOS 15 + Swift 6.1 while the package requires
macOS 26 + Swift 6.2 (Foundation Models). This is the reason the two copies drifted for two
months without anyone noticing: nothing built or tested either copy against the other's
behavior.

This ADR is issue #503's own record of that consolidation, since the issue asks for one and
the reasoning would otherwise live only implicitly in a diff.

## Decision

### 1. `liminis-context-graph`'s copy is authoritative; production behavior is the default resolution unless a documented reason overrides it

Re-diffing this repo's `native/local-inference/Sources/LocalInference/` against
`verveguy/liminis`'s `liminis-app/native/local-inference/Sources/LocalInference/` at
implementation time (2026-08-25) confirms the divergence is exactly the 14 lines the issue
named, confined to two files:

**`FoundationModelsAdapter.swift`** (8 lines) — both `stream()` sites in `InferenceActor`
and `FoundationModelsAdapter` capture their inner `Task` into a local `let task` and cancel it
from `continuation.onTermination`. Without this, a consumer that cancels the stream early
(e.g. a client disconnect) never cancels the underlying Foundation Models streaming call — it
keeps running after the caller has stopped listening.

**`InferenceAdapter.swift`** (10 lines) — `extractJSON(from:)` gained a `firstBrace <=
lastBrace` guard (plus an explanatory comment). Without it, a model response containing a
stray `}` before the next `{` (garbled or dangling braces in prose) constructs an invalid
(inverted) `Range` and crashes.

Both hunks trace to commit `cf1a873b` ("fix(local-inference): address PR review feedback",
2026-05-30) — real bugs an automated PR reviewer flagged on this repo's copy shortly after
the sidecar was first imported (#122), fixed here, and never ported back to `liminis-app`.
**Production (`liminis-app`) is therefore the side carrying both bugs.** Per the spec's own
edge case ("production wins by default is a tie-breaker, not a mandate to keep known bugs"),
both hunks are kept exactly as this repo already has them — no source changes were needed to
reconcile them, only this record of *why* this repo's version is the one that ships. See
`cf1a873b`'s commit message for the original PR-review context.

Every other file under `native/local-inference/Sources/LocalInference/` —
`AppRouter.swift`, `ChatCompletionsHandler.swift`, `EmbeddingsHandler.swift`,
`EmbeddingTypes.swift`, `LocalInferenceErrors.swift`, `Models.swift`, `SetupCache.swift`,
`main.swift` — was re-diffed at implementation time and confirmed byte-identical between the
two copies, including `EmbeddingsHandler.swift` (the ADR-0006/ADR-0016 wire-contract surface),
re-verifying the issue's filing-time snapshot still holds.

### 2. Distribution: a `workflow_dispatch` GitHub Actions build, published to a tagged GitHub Release

`verveguy/liminis` needs a way to obtain a `LocalInference` binary built from this repo's
source without vendoring the Swift source into the app repo (FR-003). The issue's own framing
assumed this was blocked by GitHub-hosted `macos-latest` still lacking Swift 6.2 — the reason
`swift.yml` is disabled. That assumption is now stale: GitHub's `macos-latest` label began
pointing at `macos-26` on 2026-06-15 with rollout complete by mid-July 2026 (macOS 26 runners
themselves went GA 2026-02-26), and that image's default Xcode (26.2+) ships Swift 6.2.3 —
confirmed independently in this worktree, which itself runs Swift 6.3.3 on macOS 26.5.1. A
hosted-runner CI build is therefore viable today, not just a documented local-build fallback.

`.github/workflows/swift-release.yml` is a new, dedicated workflow: `workflow_dispatch`-only
trigger, `runs-on: macos-latest`, builds in release mode, runs `swift test`, packages the
`LocalInference` binary as a tarball with a checksum, and publishes both to a GitHub Release.
This mirrors the distribution shape of `specs/132-publish-versioned-macos-release`'s
cargo-dist-based release of the Rust binary, but is hand-written — there is no Swift
equivalent of cargo-dist — and lives in its own workflow file rather than folded into
`release.yml`.

**Why `workflow_dispatch` and not a tag push**: `release.yml` (autogenerated by cargo-dist) is
already tag-triggered, on the pattern `'**[0-9]+.[0-9]+.[0-9]+*'`. That glob matches any tag
containing a `digit.digit.digit` substring *anywhere*, regardless of prefix — `.` is literal,
`**`/`*` absorb arbitrary prefix/suffix. A dotted semver-style tag picked for a sidecar release
(e.g. `local-inference-v0.1.0`) would satisfy that pattern and silently kick off the Rust
cargo-dist release workflow too, which would then fail because the tag isn't a `Cargo.toml`
package version. Rather than editing the autogenerated `release.yml` to exclude a prefix, the
new workflow avoids the collision at the source: it never triggers on a tag push, and its own
release-tag scheme is `local-inference-v<integer>` (no dots — the run number by default),
which cannot match a `digit.digit.digit` glob.

**Why `swift.yml` stays disabled despite the apparent toolchain-gap closure**: re-enabling it
is explicitly out of scope for this issue (it would additionally require deciding what a
*required* PR check on the sidecar means for merge policy, and possibly touching branch
protection — outside what this issue asked for). It is recorded here as a recommended
immediate follow-up, since CI's absence is exactly what let the two copies drift silently for
two months.

### 3. `native/local-inference/README.md` states sole ownership and documents distribution

The README's "Relationship to the main project" section previously said the two copies were
"intentionally maintained side-by-side for now." That sentence described the arrangement this
ADR undoes and is replaced with a sole-ownership statement plus a "Distribution" section
describing the three ways to obtain a binary: download the latest `local-inference-v*`
Release asset, manually trigger `swift-release.yml`, or build locally.

## Consequences

- A fix or improvement made to `native/local-inference/` in this repo is now the version that
  ships, once the companion `verveguy/liminis` change (below) also lands — the core problem
  this issue exists to fix.
- `verveguy/liminis` gains a stable download path (a GitHub Release asset) instead of vendoring
  Swift source it doesn't build from.
- Two known production bugs (streaming task leak, `extractJSON` inverted-range crash) are
  already fixed on the authoritative side; they ship as soon as `verveguy/liminis` repoints
  itself at a release built from this repo.
- `swift.yml` remains disabled; re-enabling it is a recommended, not required, follow-up.
- **Companion-repo work remains outside this repo's PR** and is tracked as follow-up in
  `verveguy/liminis`: delete `liminis-app/native/local-inference/`, repoint
  `getLocalInferenceBinaryPath()` at the `local-inference-v*` Release download, and (unrelated
  to the sidecar itself, but flagged in the same issue) rename the Rust binary's
  `GRAPHITI_EMBEDDING_DIM` env var to `LCG_EMBEDDING_DIM` in `context-graph-lifecycle.ts`. See
  the PR description for the full instructions.
