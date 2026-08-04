# Feature Specification: CI: build release artifacts once and reuse them across the five e2e jobs

**Feature Branch**: `fabrik/issue-341`
**Created**: 2026-08-04
**Status**: Draft
**Input**: User description: "On a code-touching PR, six CI jobs each run a full `cargo build --release` of the same workspace at the same commit — `ci.yml`'s `test` job plus the five `real-corpus-e2e.yml` e2e jobs. That is five redundant full release builds per PR, and five extra concurrent consumers of a single shared lbug build cache. Build the release artifacts once and have the e2e jobs consume them."

## Background

On a code-touching PR, **six CI jobs each run a full `cargo build --release`** of the same
workspace at the same commit:

| Workflow | Job | Build step |
|---|---|---|
| `ci.yml` | `build-lbug` | `cargo build --release -p lcg-core` (cache-miss only) |
| `ci.yml` | `test` | `cargo build --release` |
| `real-corpus-e2e.yml` | `real_corpus_e2e` | `cargo build --release` |
| `real-corpus-e2e.yml` | `mcp_real_corpus_e2e` | `cargo build --release` |
| `real-corpus-e2e.yml` | `mcp_real_corpus_mutation_e2e` | `cargo build --release` |
| `real-corpus-e2e.yml` | `mcp_real_corpus_admin_data_e2e` | `cargo build --release` |
| `real-corpus-e2e.yml` | `mcp_real_corpus_admin_lifecycle_e2e` | `cargo build --release` |

`real-corpus-e2e.yml` runs alongside `ci.yml` on every code-touching PR as of #328 / ADR-0328
(PR #329) — before that change these five jobs ran only on push-to-`main` and
`workflow_dispatch`. Each job independently restores the `lbug-cache-*` key and then compiles
everything the cache does not cover. That is five redundant full release builds per PR, and five
extra concurrent consumers of a single shared cache key.

### The lbug cache does much less than its name suggests

Worth stating plainly, because it shapes the fix: **lbug is not compiled from source in CI, and
has not been since 0.17.0.** `build.rs`'s `try_download_prebuilt_lbug()` fetches a self-contained
prebuilt fat-bundle `liblbug.a` from the `LadybugDB/ladybug` releases and caches it under the
crate's registry source dir (`.cache/lbug-prebuilt/<key>`). `LBUG_BUILD_FROM_SOURCE` was removed
from `ci.yml`, `release.yml`, and `bench.yml` because the source path is broken — the bundle
already contains every third-party archive, so a cmake build links both and fails with 7399
duplicate symbols (upstream `LadybugDB/ladybug-rust#18`).

So `lbug-cache-*` covers only three paths:

```
target/release/build/lbug-*
target/release/.fingerprint/lbug-*
target/release/deps/liblbug-*
```

It saves a build-script re-run. It does **not** cover the registry dir where the downloaded
bundle lives (a cold runner re-downloads), and it does not cover a single one of the workspace's
other dependencies. Everything else in `target/release/` is rebuilt from scratch, six times.

### Observed cost: the duplicate-symbol race, now with five more contenders

#336 (2026-08-04, `main`) failed `mcp_real_corpus_e2e` with:

```
rust-lld: error: duplicate symbol: antlr4::atn::ATNConfig::ATNConfig(...)
>>> defined at ATNConfig.cpp
... in archive .../target/release/deps/rustcEXyILw/libantlr4_runtime.a
```

This is the race documented verbatim in `ci.yml`'s trigger comment: concurrent runs sharing the
cache key, one linking against a half-written archive set. It was spurious — a re-run of the
identical commit `1700756` went green on four of five jobs, and the next `main` run (`f3b1987`)
was fully green.

`ci.yml`'s `concurrency:` group solved the original instance by making a PR branch run CI once.
It does not extend across workflows: at 01:59 the release build, the release PR's CI, and the
post-merge e2e all overlapped on the same key.

**This is the empirical answer to #328's FR-005**, which asked whoever implemented it to verify
whether adding five release-building jobs per PR aggravates the documented race. #336 landed
within hours of #328 shipping, and it is the first observed instance since. Note, per
ADR-0328's Decision 4, that all five `real-corpus-e2e.yml` jobs restore the lbug cache and never
save to it — only `ci.yml`'s `build-lbug` job writes. Whatever the precise mechanism connecting
"five more concurrent consumers" to this specific failure, root-causing it exactly is not this
issue's job (see Out of Scope); what is in scope is removing the redundant builds and making it
structurally impossible for any job to link against a cache entry another job is still writing.

The failure mode is expensive out of proportion to its frequency: it is indistinguishable from a
real regression until someone re-runs it, it files a `ci-failure` tracking issue each time
(#298's notifier), and left alone it will erode trust in exactly that notifier.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A code-touching PR builds the workspace once, not six times (Priority: P1)

A contributor opens a PR that touches Rust source, manifests, the lockfile, or CI/build files.
Across the entire CI run (`ci.yml` plus `real-corpus-e2e.yml`), the workspace's release build is
compiled exactly once; the five e2e jobs and `ci.yml`'s `test` job all consume that single
build's outputs instead of each independently invoking `cargo build --release`.

**Why this priority**: This is the core problem in the issue — five redundant full release
builds per PR is the wasted work this issue exists to eliminate.

**Independent Test**: Can be fully tested by opening a real PR that touches a `.rs` file,
inspecting the resulting CI run's job logs, and counting how many jobs actually invoke a full
workspace `cargo build --release` (or equivalent) rather than downloading a prebuilt artifact —
delivers "one build, not six" on its own, independent of whether the race in Story 2 recurs.

**Acceptance Scenarios**:

1. **Given** a PR touching Rust or build files, **When** CI runs, **Then** exactly one job in the
   combined run performs a full `cargo build --release` of the workspace.
2. **Given** that same PR, **When** the e2e jobs and `ci.yml`'s `test` job need the compiled test
   binaries and the `liminis-context-graph` service binary, **Then** they obtain them from the
   single build's output rather than compiling their own copies.

---

### User Story 2 - No CI job can link against a cache entry another job is still writing (Priority: P1)

The `lbug-cache-*` key is written by at most one job in a given CI run. No job reads a partially
written cache entry left by a concurrently running write, and the specific duplicate-symbol
failure class observed in #336 cannot recur for this reason (as opposed to being merely less
likely).

**Why this priority**: Equal to Story 1 — this is the concrete, user-visible pain (#336): a
spurious failure indistinguishable from a real regression that files a false tracking issue and
erodes trust in the failure notifier. Reducing build count without also removing the race
mechanism would only make the failure less frequent, not impossible.

**Independent Test**: Can be verified by inspecting the resulting workflow files and confirming
that only one job's steps write to the shared cache path — a structural property, checkable
without needing the race to reproduce live — and by the absence of cache-race-attributable
failures on subsequent code-touching PR runs.

**Acceptance Scenarios**:

1. **Given** the CI workflow definitions after this change, **When** inspected, **Then** at most
   one job across the combined run performs a cache *write* to the `lbug-cache-*` key; every
   other job that touches lbug build artifacts only *restores*.
2. **Given** a code-touching PR runs CI, **When** the run completes, **Then** no job fails with a
   duplicate-symbol link error attributable to a half-written cache entry.

---

### User Story 3 - The e2e signal does not get slower or later (Priority: P1)

A contributor's PR still gets the e2e jobs' pass/fail signal without a net increase in
time-to-mergeable. If consuming a shared build artifact requires the e2e jobs to wait on the job
that produces it, that added latency does not push the e2e signal meaningfully later than it
already lands relative to the required `test` check today.

**Why this priority**: Equal to Stories 1–2 — #328 established that these jobs run in parallel
specifically so they add zero latency to a PR's mergeable time (the e2e suite's own ~9 minute
wall clock finishes well before the ~17-20 minute required `test` check). A fix for redundant
builds that reintroduces meaningful latency trades one regression for another.

**Independent Test**: Can be fully tested by measuring a code-touching PR's total wall-clock to
mergeable-status before and after this change and confirming no regression — independent of
Stories 1 and 2, which are about build count and cache safety rather than timing.

**Acceptance Scenarios**:

1. **Given** a code-touching PR, **When** its CI run completes, **Then** the PR's total
   wall-clock time to a mergeable state is not worse than before this change, with the
   before/after numbers recorded in the implementing PR's description.

---

### User Story 4 - A silent regression back to redundant builds is caught automatically (Priority: P2)

If a future change causes an e2e job to recompile code after consuming the shared build artifact
— silently reintroducing the redundant-build problem this issue fixes — CI fails loudly on that
job rather than passing while quietly creeping back to the old behavior.

**Why this priority**: Lower than Stories 1–3 because it is a regression guard rather than the
primary fix, but it mirrors an existing, valued pattern in this repo: `ci.yml`'s FR-008 already
asserts lbug was not recompiled on a cache hit by grepping the build log, specifically because an
unguarded win tends to erode silently.

**Independent Test**: Can be fully tested by intentionally causing an e2e job to recompile after
downloading the shared artifact (e.g., in a throwaway branch) and confirming the new guard fails
the build with a clear message.

**Acceptance Scenarios**:

1. **Given** an e2e job that has consumed the shared build artifact, **When** it nonetheless
   compiles or recompiles any workspace code afterward, **Then** CI fails with an explicit,
   readable assertion identifying the regression — not a silent pass.

---

### Edge Cases

- The five e2e jobs live in `real-corpus-e2e.yml`, a separate workflow file with its own
  independent `pull_request` trigger, from `ci.yml`'s `test` job — the job the issue's sketch
  proposes as the artifact producer. Whether that requires cross-workflow artifact access,
  restructuring how the two workflows relate (e.g. one calling the other), or some other
  mechanism is a feasibility question for Research/Plan to resolve, not assumed by this spec.
- `real-corpus-e2e.yml` also supports a standalone `workflow_dispatch` trigger, which can run
  without `ci.yml` running in the same invocation — the chosen mechanism must still let that path
  obtain (build or reuse) the binaries it needs.
- A push-to-`main` event runs both `ci.yml`'s `test` job and `real-corpus-e2e.yml`'s post-merge
  jobs together, the same combination of jobs targeted by this issue's PR-path framing — the
  one-build guarantee applies there too, not only to the `pull_request` path.
- A cache-miss on the shared lbug cache during the single consolidated build must still populate
  the cache for subsequent runs, same as today's `build-lbug` job.
- Artifact retention/expiry for a long-lived PR or a delayed manual re-run must not leave a job
  unable to obtain its binaries; if this is a real constraint it should be reflected in the
  implementation's error handling (fail loudly, not silently rebuild) rather than left unhandled.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: On any single CI run that includes both `ci.yml`'s `test` job and the five
  `real-corpus-e2e.yml` e2e jobs (a code-touching PR push, or a push to `main`), the workspace's
  release build MUST be compiled at most once across all jobs combined.
- **FR-002**: The five e2e jobs (`real_corpus_e2e`, `mcp_real_corpus_e2e`,
  `mcp_real_corpus_mutation_e2e`, `mcp_real_corpus_admin_data_e2e`,
  `mcp_real_corpus_admin_lifecycle_e2e`) MUST consume prebuilt test binaries and the
  `liminis-context-graph` service binary rather than each independently compiling them.
- **FR-003**: At most one job in a given CI run MAY write to the shared `lbug-cache-*` key; every
  other job that touches lbug build artifacts MUST be restore-only. No job may read a cache entry
  while another job in the same run is still writing it.
- **FR-004**: The mechanism used to share build outputs between jobs MUST NOT transfer
  `target/release` wholesale (measured locally at ~2.7 GB, ~2.2 GB of it in `deps/`). It MUST be
  scoped to only the binaries the consuming jobs need — the five test binaries plus the
  `liminis-context-graph` service binary — with the actual measured transfer size recorded as
  part of the implementation.
- **FR-005**: Consuming jobs MUST invoke the shared binaries directly (e.g. running the compiled
  test executable, or an equivalent direct invocation) rather than restoring `target/` and
  re-running `cargo test` — unless empirical measurement demonstrates that a restored `target/`
  reliably avoids recompilation for this workspace, in which case that measurement must be
  recorded and the restore approach may be used instead.
- **FR-006**: CI MUST include an explicit automated assertion that fails the build if any e2e job
  compiles or recompiles workspace code after consuming the shared build artifact, mirroring the
  existing FR-008 lbug-cache-recompilation guard in `ci.yml`.
- **FR-007**: This change MUST NOT regress a code-touching PR's total wall-clock time to a
  mergeable state; before/after timings MUST be measured and recorded in the implementing PR's
  description.
- **FR-008**: If making the e2e jobs depend on the artifact-producing job (e.g. via `needs:`)
  would delay the e2e signal past its current effective landing time (today, well before the
  required `test` check completes), the implementation MUST avoid that delay — for example via a
  dedicated minimal artifact-producing job that both `test` and the e2e jobs depend on in
  parallel — rather than accept a regression to FR-007.
- **FR-009**: If, after investigation, artifact reuse is rejected as infeasible or not worth its
  cost relative to the alternative, the implementation MUST instead give each cache-writing job a
  distinct cache key, which independently eliminates the write-write race describable for #336
  (Story 2) at the cost of keeping the redundant builds, and MUST document why the artifact-reuse
  path was rejected, including the measurements that led to that conclusion.
- **FR-010**: This issue is CI-only; it MUST NOT change the shipped release artifact or its
  contents.

### Key Entities

Not applicable — this feature changes CI workflow job structure and artifact handling and does
not introduce or modify any data entities.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A code-touching PR's combined CI run (all jobs across `ci.yml` and
  `real-corpus-e2e.yml`) performs exactly one full workspace `cargo build --release` — or, if
  FR-009's fallback is taken instead, this criterion is replaced by SC-005 and that substitution
  is documented.
- **SC-002**: No CI job's build step reads a cache path that another job in the same run
  concurrently writes — verified structurally by inspecting the resulting workflow definitions,
  not solely by absence-of-failure over time.
- **SC-003**: A code-touching PR's measured wall-clock time to a mergeable state, recorded
  before and after this change in the implementing PR's description, shows no regression.
- **SC-004**: An intentional test case that causes an e2e job to recompile after consuming the
  shared artifact is demonstrated to fail CI via the new guard (FR-006), not pass silently.
- **SC-005** *(applies only if FR-009's fallback is taken)*: Each cache-writing job uses a
  distinct cache key, and the PR description documents the measurement that led to rejecting
  artifact reuse.

## Assumptions

- `real-corpus-e2e.yml` currently runs on the `pull_request` path per #328 / ADR-0328 (merged as
  PR #329) — the "six jobs per code-touching PR" premise this issue is built on reflects the
  repository's current `main` branch, not a future or hypothetical state.
- All five `real-corpus-e2e.yml` jobs are restore-only against the lbug cache today (per
  ADR-0328's Decision 4); only `ci.yml`'s `build-lbug` job writes to it. This issue's FR-003 makes
  that already-true property explicit and structurally guaranteed rather than incidental, and
  does not depend on identifying the exact mechanism by which #336's failure occurred.
- CI runs exclusively on `ubuntu-latest`; no cross-platform binary-compatibility concerns apply
  to the shared build artifacts.
- Whether sharing build outputs requires GitHub Actions artifacts (`actions/upload-artifact` /
  `actions/download-artifact`), a restructuring of how `ci.yml` and `real-corpus-e2e.yml` relate,
  or another mechanism entirely is an implementation decision for Research/Plan, not fixed by
  this spec — the issue's own proposal is explicitly a "sketch," not a committed design.
- The exact causal mechanism connecting #328's added e2e jobs to #336's specific failure is not
  required to be fully root-caused by this issue (see Out of Scope) — FR-001 through FR-003 are
  worth doing on their own efficiency and structural-safety merits regardless of that mechanism.

## Out of Scope

- Reducing the e2e suite's own runtime below its current ~9 minute wall clock (established by
  #328 / ADR-0328).
- Promoting the e2e checks to required status checks (ADR-0328 Decision 6 defines a separate
  14-consecutive-day trial period for that).
- Changing what the test suites cover, or the shipped release artifact's contents (see FR-010).
- Fully root-causing the exact mechanism of #336's failure beyond what FR-001 through FR-003
  already guarantee structurally (see Assumptions).
- Reducing the R-003 bench-step CI cost — already addressed by #316 / ADR-0316; this change is
  not expected to move that number.

## Source References

- #328 / PR #329 / ADR-0328 — added the `pull_request` trigger to `real-corpus-e2e.yml` that
  this issue's "six jobs per PR" premise depends on, including the restore-only cache analysis
  (Decision 4) referenced in Background and Assumptions.
- #336 — the observed duplicate-symbol CI failure motivating this issue.
- #298 — the `ci-failure` tracking-issue notifier that fired for #336.
- #322 / ADR-0322 — the docs-only fast path and shared `classify-changes` composite action that
  `real-corpus-e2e.yml` also uses.
- ADR-0316 — why the R-003 bench step, not the release test build, was the historical dominant
  CI cost; this change is not expected to move that number.
- `.github/workflows/ci.yml` — the `build-lbug`/`test` jobs and the existing FR-008
  lbug-recompilation guard this issue's FR-006 mirrors.
- `.github/workflows/real-corpus-e2e.yml` — the five e2e jobs and their current independent
  `cargo build --release` steps.
