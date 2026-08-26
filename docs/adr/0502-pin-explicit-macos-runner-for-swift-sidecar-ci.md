# ADR-0502: Pin an Explicit `macos-26` Runner for Swift Sidecar CI, Not Yet a Required Check

**Status**: Accepted
**Date**: 2026-08-25
**Issues**: #502

## Context

`native/local-inference/` is the macOS Swift sidecar serving on-device CoreML embeddings and
Apple Foundation Models chat completions. Its `Package.swift` declares
`swift-tools-version: 6.2` and `platforms: [.macOS(.v26)]` — a hard requirement of the
package, since Foundation Models is a macOS 26 framework.

`.github/workflows/swift.yml`'s sole job was `if: false` from 2026-05-30: GitHub's
`macos-latest` runner image was still macOS 15 + Swift 6.1 at the time, so `swift test` failed
immediately with a tools-version mismatch. Every run since — including every release tag from
v0.13.0 through v0.13.4 — reported `skipped`, giving zero CI signal on a component that ships
in every release. The only gate was an honour-system README instruction to run `swift test`
locally on a macOS 26 machine before pushing. This absence of CI is called out in #502 as the
mechanism that let two copies of this package (this repo's and `liminis-app`'s) diverge
silently, which #503 now has to reconcile.

Research for #502 found, via GitHub's public changelog and `actions/runner-images#14167`, that
macOS 26 runners became GA on GitHub-hosted infrastructure 2026-02-26, and `macos-latest`
completed its migration from macOS 15 to macOS 26 by ~2026-07-15. Today (2026-08-25) is well
past that window, so the original blocker has almost certainly aged out — but this was
web-research, not a live run in this repo, at the time the decision below was made.

Research also found the entire existing test suite (`Tests/LocalInferenceTests/`) already runs
without the ~400 MB CoreML embedding asset and without live Foundation Models availability: all
chat-completion/health/error-handling tests use `MockInferenceAdapter`/`UnavailableAdapter`, and
the three suites that do touch CoreML (`LocalInferenceIntegrationTests`,
`EmbeddingOutputValidationTests`, `SetupCacheTests`) load only the in-tree stub
`.mlpackage` fixtures under `Tests/LocalInferenceTests/Fixtures/`. There is no subset to filter
out — FR-003/FR-004/FR-007 are satisfied by running `swift test` unmodified.

## Decision

### Pin `runs-on: macos-26`, not `macos-latest`

The three-month outage happened precisely because the package's minimum macOS requirement
outran `macos-latest`'s rollout. `macos-latest` has now caught up, but the *next* time this
package's minimum platform requirement bumps ahead of what `macos-latest` currently resolves to
(as the issue's own Edge Cases note is a real possibility), floating on `macos-latest` would
silently reproduce this exact gap: the job would keep running, just on a runner whose Xcode/SDK
no longer satisfies the package's `swift-tools-version`, and depending on toolchain resolution
could either fail clearly or, worse, resolve to an unexpected Xcode and behave unpredictably.

Pinning `macos-26` means a future platform bump instead fails loudly and immediately — a job
referencing a still-valid `macos-26` label that simply can't build the new tools-version — a
clear, attributable CI failure instead of a silent `skipped` that takes three months to notice.

The tradeoff is that this label will eventually need a manual bump when GitHub deprecates
`macos-26` in favor of a newer major version — but the issue itself names "pin a newer runner
label" as an accepted option, and a periodic manual relabel is strictly better than another
silent, undetected gap.

### Do not make `Swift sidecar CI` a required branch-protection status check yet

This PR is itself the first real run of this job in three months. The runner-image research
above is plausible but was not verified against a live run before this decision was made — an
unexpected wrinkle (e.g. `macos-26`'s default Xcode not resolving to Swift 6.2+, or a dependency
like `hummingbird`/`swift-transformers` having its own tools-version incompatibility) is exactly
the kind of thing only a real run surfaces. Making the check required before that first run is
observed risks blocking unrelated PRs on a runner/toolchain quirk rather than an actual code
defect.

Once this PR's own CI run — and a couple of follow-on runs — are observed green, making the
check required is a one-line branch-protection change with evidence in hand. That decision is
deferred, not abandoned.

## Consequences

- `.github/workflows/swift.yml` runs `swift build` then `swift test` unmodified on PRs and
  pushes (including release tags) touching `native/local-inference/**`, on `macos-26`, with a
  real pass/fail outcome instead of `skipped`.
- `ci-failure-notify.yml` now watches `"Swift sidecar CI"` (per its own header comment
  instructing this "when it's re-enabled" — ADR-0298), so a post-merge failure on `main` files a
  tracking issue instead of going unnoticed the way the pre-#502 `skipped` state effectively did.
- The check is visible on every PR touching the sidecar but does not yet block merge. A
  runner/toolchain-only failure (unrelated to the actual code change) can currently still be
  merged around; this is the accepted tradeoff of not yet gating on an unverified-until-run job.
- If the first live run(s) reveal that no available `macos-26` runner tier actually satisfies
  the toolchain requirement (e.g. `macos-26` turns out to be a paid/large-tier-only label in this
  repo's plan), the fallback is documented in #502 with the concrete failure evidence, and this
  ADR should be revisited rather than silently reverting to `if: false`.

## Alternatives Considered

- **Continue floating on `macos-latest`**: rejected — this is exactly the mechanism that caused
  the original three-month gap and would silently reproduce it on the next platform bump.
- **Make the check required immediately**: rejected — the job's success is plausible but
  unverified until this PR's own run is observed; requiring it immediately risks blocking
  unrelated merges on a runner-image quirk rather than a real regression.
- **Provision a self-hosted macOS 26 runner**: rejected as premature — the issue's own option
  ordering treats this as a last resort, and a hosted `macos-26` label already exists and should
  be tried first.

## References

- Issue #502
- `docs/adr/0298-ci-failure-notification.md` — the mechanism this ADR wires the re-enabled
  workflow into
- Issue #501 — sidecar mode selection, related but non-blocking
- Issue #503 — sidecar consolidation, the issue this CI gap is cited as having enabled
- `actions/runner-images#14167` — the `macos-latest` → macOS 26 migration this decision relies on
