# Feature Specification: Cache lbug C++ Build Artifacts Across CI Runs to Cut PR Wall-Clock From ~1h to ~15min

**Feature Branch**: `fabrik/issue-115`
**Created**: 2026-05-28
**Status**: Draft
**Input**: Live observation 2026-05-28 — Fabrik-driven PRs #113 and #114 ran their `test (ubuntu-latest)` jobs in 1h11m and 1h14m respectively, with all other Ubuntu jobs (`bench compile (stub)`, `bench-dedup`) also in the same band. Both PRs exceeded Fabrik's CI-wait window and were paused as `fabrik:awaiting-input` despite all checks ultimately passing green. The dominant cost is lbug's C++ build from source (`LBUG_BUILD_FROM_SOURCE=1` set in `.github/workflows/ci.yml` because the upstream lbug prebuilt does not ship its third-party static archives). Even with `Swatinem/rust-cache@v2`, the warm-cache time stays at ~1h, which suggests the cache is either being evicted (3 concurrent jobs writing to a 10 GB per-repo cap) or invalidated by unrelated `Cargo.lock` churn. The user has ruled out paid larger GHA runners; the only remaining lever is to stop rebuilding lbug on every run.

## Background

`liminis-graph` depends on lbug (a KuzuDB community fork). Today CI compiles lbug from C++ source in three parallel jobs per PR (`test`, `bench-stub`, `bench-dedup`), each starting from a near-cold cache because:

1. **`Swatinem/rust-cache@v2` keys on `Cargo.lock` + toolchain.** Any unrelated dep bump (a single PR that touches `cargo update`) invalidates the cache, wiping the multi-minute lbug C++ build along with it.
2. **Three concurrent jobs evict each other.** GitHub Actions repo-scoped caches are LRU with a 10 GB cap. A release-mode lbug build with all third-party static archives is large; three jobs writing in parallel cause cross-eviction even when caches would otherwise hit.
3. **`cargo test --release` produces test-only intermediate artifacts** that the cache action may treat as distinct from a pure `cargo build --release` warm cache, depending on how `Swatinem/rust-cache` namespaces its cache keys.

The architectural fix is to treat lbug's compiled output as a versioned binary artefact, not a Cargo build cache. Cache it once per lbug version, share it across all jobs, and only rebuild when the lbug version actually changes — which is rare relative to PR cadence.

The reason this is a P1 fix and not "just live with slow CI": Fabrik's gate behaviour assumes CI completes within a normal window. At 1h+ wall-clock, every PR gets paused for human intervention, defeating the autonomous-Specify-to-merge story. The user has explicitly chosen this fix path over the easier "buy bigger runners" option.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — PR CI Completes Inside Fabrik's Default Wait Window (Priority: P1)

When a Fabrik-driven PR reaches the Validate stage, CI must complete (green or red) within Fabrik's default CI-wait window without operator intervention. Today's ~1h+ wall-clock causes every PR to pause; the target is to bring the gating job's wall-clock under 20 minutes so the wait window doesn't expire.

**Why this priority**: this is the single biggest source of friction in Fabrik-driven development on `liminis-graph`. Every PR currently requires manual unpause to merge. Without this fix, autonomous SDLC is effectively broken on this repo.

**Independent Test**: Open a trivial PR (one-line README change). Measure wall-clock from "push" to "all checks pass". Assert ≤ 20 minutes. Re-run after a `Cargo.lock` bump (simulate dep churn) on a separate PR; assert the same wall-clock target holds because the lbug cache survives unrelated Cargo.lock changes.

**Acceptance Scenarios**:

1. **Given** a PR that does not change the lbug version, **When** the gating CI job runs, **Then** the lbug C++ artefacts are restored from cache without recompilation, and the job completes in ≤ 20 min wall-clock.
2. **Given** a PR that bumps an unrelated Rust dep in `Cargo.lock` (e.g., `serde` minor version), **When** CI runs, **Then** the lbug cache still hits (its cache key is independent of unrelated Cargo.lock entries).
3. **Given** a PR that bumps `lbug-sys` itself, **When** CI runs, **Then** the cache misses cleanly, lbug rebuilds once, the new artefacts are uploaded to the cache, and subsequent PRs on the same lbug version hit the cache.

---

### User Story 2 — Cache Survives Job Concurrency (Priority: P1)

When multiple CI jobs run in the same workflow run, they share the lbug cache rather than evicting each other. Specifically, after the workflow restructure, only ONE job builds lbug; the others depend on it and restore the artefact.

**Why this priority**: today's three-way parallel cache writes are part of why warm-cache hits don't materialise. Centralising the lbug build into a single producer job is the structural fix.

**Acceptance Scenarios**:

1. **Given** a PR that triggers `test`, `bench-stub`, and `bench-dedup` jobs, **When** CI runs, **Then** exactly one job performs the lbug build (or restore), and the others consume its output via cache restore or workflow artefact download.
2. **Given** a cache hit on the lbug artefact, **When** downstream jobs start, **Then** they restore the artefact in under 60 seconds combined and proceed to `cargo test` / `cargo bench` without touching lbug's C++ build steps.

---

### User Story 3 — Cache Invalidation Is Deterministic and Documented (Priority: P2)

Engineers maintaining `liminis-graph` can predict, from the lbug version pinned in `Cargo.lock`, whether a given PR will hit or miss the lbug cache. The cache key is derivable by reading the workflow file; no hidden inputs (timestamps, randomness, host fingerprints) influence it.

**Why this priority**: opaque caches lead to "works on my fork, doesn't on yours" surprises. Deterministic cache keys are how this stays maintainable as Rust deps churn around it.

**Acceptance Scenarios**:

1. **Given** the workflow file at `.github/workflows/ci.yml`, **When** an engineer reads it, **Then** the lbug cache key is visible as a simple expression involving `runner.os`, the Rust toolchain version, and the pinned lbug-sys version (whether read via `hashFiles('Cargo.lock')` filtered to lbug-sys lines, or pinned manually as a workflow env var).
2. **Given** a documentation snippet in `CLAUDE.md` (Rust pre-commit checks section), **When** an engineer needs to bump lbug, **Then** the snippet explains that the bump invalidates the cache and the first post-bump PR will pay full build cost.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001.** A new GHA workflow job (or composite action) MUST build lbug from source exactly once per (runner OS, Rust toolchain version, lbug-sys version) tuple. The build outputs (compiled `.a` static archives, any generated headers, build script OUT_DIR contents that downstream Rust crates link against) MUST be packaged as a portable artefact restorable into another job's `target/` directory.
- **FR-002.** The cache key MUST NOT include the full `Cargo.lock` hash. Unrelated Cargo.lock changes (a `serde` patch bump, a `tokio` minor bump, etc.) MUST NOT invalidate the lbug cache. The key MUST include at minimum: `runner.os`, the Rust toolchain version, and a deterministic identifier of the pinned lbug-sys version (preferred: the lbug-sys line from `Cargo.lock`, hashed; acceptable: an explicit `LBUG_SYS_VERSION` env var in the workflow that is bumped when lbug-sys is bumped).
- **FR-003.** The `test` job MUST consume the cached lbug artefact and skip the lbug C++ build. The `cargo test --release` command MUST still succeed end-to-end against the restored artefact — i.e., the artefact contents MUST be sufficient for cargo to consider lbug-sys "already built" and proceed to link against it.
- **FR-004.** If `bench-stub` and `bench-dedup` jobs remain (they may be folded into `test` per the sibling issue on bench restructure), they MUST consume the same cached lbug artefact, NOT each build their own copy. Workflow `needs:` ordering enforces this — only one job performs the build/restore; others depend on it.
- **FR-005.** The cache MUST be readable on PR branches that haven't yet contributed to it. Specifically, the GHA cache action's branch-fallback behaviour (PR branches read main's cache when their own is empty) MUST work — verify by running a fresh PR on a new branch and confirming the lbug cache hits.
- **FR-006.** When the cache misses (e.g., on the first run after an lbug-sys bump), the workflow MUST rebuild lbug, populate the cache, and succeed. The fallback path MUST NOT degrade — that first post-bump PR pays today's ~1h cost, then all subsequent PRs pay ~20 min. This degradation curve is acceptable and documented.
- **FR-007.** Cache size MUST stay within the GHA 10 GB per-repo cap. A release-mode lbug build artefact is expected to be in the low-hundreds-of-MB range; combined with two or three OS/toolchain variants kept in cache, total usage should stay under 2 GB.
- **FR-008.** A regression check MUST be added: a workflow step that asserts the lbug C++ source compilation does NOT run on cache-hit runs. This can be a simple grep of the `cargo` build output for a "Compiling lbug-sys" or "Compiling kuzu" line, exiting nonzero if present on what was supposed to be a cache-hit run. Without this guard, the cache could silently stop working and CI would slowly creep back to ~1h without anyone noticing.
- **FR-009.** Documentation: `CLAUDE.md` MUST be updated with a one-paragraph note in the Rust pre-commit checks section explaining the lbug cache (when it invalidates, how to manually bust it if corrupted, expected wall-clock on hit vs miss).

## Edge Cases

- **First PR after an `lbug-sys` bump pays the full ~1h cost.** Acceptable per FR-006. Document the one-time hit in CLAUDE.md so it doesn't get misdiagnosed as "the cache is broken" by future engineers.
- **Concurrent PRs both miss the cache** (e.g., two PRs landing simultaneously after an lbug-sys bump). Each builds lbug independently; the first to finish populates the cache, the second's upload is a no-op or overwrites identically. No correctness issue, just wasted runner-minutes for one PR. Acceptable.
- **Cache evicted by GHA's LRU policy** (other workflows in the repo consuming the 10 GB cap). On miss, falls through to rebuild. Frequency depends on repo activity; mitigate by keeping the lbug artefact compact (FR-007).
- **Toolchain change without a Cargo.lock change** (e.g., a `dtolnay/rust-toolchain@stable` update that picks up a new stable Rust). Cache key MUST include the resolved toolchain version, not just the action input string. Use the rustc version as a cache key component.
- **Workflow run on a fork** (untrusted PRs from external contributors). GHA caches are typically read-only for forks; the cache miss falls through to full rebuild. Acceptable — slow CI for fork PRs is normal.
- **Corrupted cache** (extremely rare but possible if a prior run was killed mid-upload). Provide a manual cache-bust mechanism: an environment variable like `LBUG_CACHE_BUST=YYYY-MM-DD` that, when bumped in the workflow file, invalidates the existing cache. Document in CLAUDE.md.
- **lbug-sys version pinned via git revision rather than crate version.** The cache key needs to handle both forms (semver string OR git SHA). Use the full line as it appears in `Cargo.lock`, not the version field alone.

## Assumptions

- **A1.** lbug-sys's `build.rs` produces deterministic outputs given the same source — i.e., binary-identical artefacts are reproducible across runners of the same OS/toolchain. Verification: run the build twice on the same runner image, diff outputs. If non-deterministic (e.g., embedded timestamps), the artefact-restore-vs-link path may still work because cargo uses fingerprints, not content hashes, to decide rebuilds — but this needs confirming during Research.
- **A2.** The cache key can be expressed as a static GHA `key` expression without dynamic computation. The simplest form is `runner.os + toolchain + hashFiles('Cargo.lock')` filtered, or `runner.os + toolchain + LBUG_SYS_VERSION` as a manual env var. Research stage will pick the cleaner option.
- **A3.** lbug-sys is a single C++ build that completes within the cache step's I/O timeout. If lbug-sys is split into multiple crates with separate `build.rs` files, all of their outputs are co-located under `target/release/build/` and can be cached as one tarball.
- **A4.** No other dependency in `Cargo.lock` has a `build.rs` whose output meaningfully changes the link surface for lbug-sys-dependent crates. If something does (e.g., a `*-sys` shim that generates bindings keyed on the lbug-sys ABI), it MUST also be cached or kept outside the cached layer. Research stage should grep `Cargo.lock` for `*-sys` and `build.rs`-having crates.
- **A5.** GHA's `actions/cache@v4` supports the artefact sizes involved. Limit per cache entry is 10 GB; our artefact will be well under that.

## Success Criteria *(mandatory)*

- **SC-001.** A no-op PR (one-line README change) on a branch that has previously had its lbug cache populated completes the full CI workflow in ≤ 20 minutes wall-clock on the gating job. Today's baseline: 1h11–1h14m.
- **SC-002.** A PR bumping an unrelated Rust dep (e.g., `serde` patch version) hits the lbug cache and completes in the same ≤ 20-minute envelope.
- **SC-003.** A PR bumping `lbug-sys` itself misses the cache, completes in roughly today's baseline (~1h), and populates the cache. The IMMEDIATELY following PR (no further lbug-sys change) hits the cache and meets SC-001.
- **SC-004.** Across a sample of 10 typical PRs (after the cache is warmed), at least 9 hit the cache. The 1-in-10 miss is acceptable as a buffer for genuinely heavy PRs (lbug-sys bumps, toolchain bumps) but not as a normal state.
- **SC-005.** The FR-008 regression check fires correctly: introduce a deliberately bad cache key (point it at a stable string that always "hits"), restore stale artefacts into a PR that would otherwise need to recompile lbug, and confirm the workflow exits nonzero with a clear "lbug C++ compilation detected on a cache-hit run" message.
- **SC-006.** Total GHA cache footprint per branch stays under 2 GB. Measured via the repo's "Caches" page after the workflow completes successfully on main.
- **SC-007.** Fabrik PR pause-rate (% of fabrik:yolo PRs that hit `fabrik:awaiting-input` due to CI-wait timeout) drops from "near 100%" (current observation: #109, #110 both paused) to "near 0%" over the 5 PRs following this fix's deployment.

## Out of Scope

- Pushing upstream changes to lbug to ship complete prebuilts (would eliminate `LBUG_BUILD_FROM_SOURCE=1` entirely but is a multi-month effort and gated on upstream maintainers).
- Moving to self-hosted runners. The user has explicitly chosen not to pay for larger GHA runners; self-hosted is even more infra.
- `sccache`-based C++ compiler caching. Stackable on top of this issue but a separate concern — this issue caches the *output*, not the per-TU compiler invocations.
- macOS CI. Today's workflow only runs on ubuntu-latest. macOS builds happen locally per developer; out of scope for this fix.
- Moving any tests off CI; sibling issue handles the perf-bench restructure.

## Source References

- **`.github/workflows/ci.yml`** — the current `test`, `bench-stub`, and `bench-dedup` jobs, including the `LBUG_BUILD_FROM_SOURCE=1` env var and the inline comment explaining why `--release` is mandatory (debug-mode linking OOMs the 7 GB runner). Caching is orthogonal to that constraint.
- **`Swatinem/rust-cache@v2`** — currently used; its limitation (whole-Cargo.lock key) is the root cause of cache underperformance for our specific workload.
- **`Cargo.lock`** — contains the pinned `lbug-sys` version (or git SHA); the source of truth for cache invalidation.
- **PRs #113 and #114** — the concrete data points motivating this issue. Both had all checks pass green at ~1h11–1h14m, both were paused by Fabrik's CI-wait timeout, both required manual `fabrik:paused` removal to merge.
- **`CLAUDE.md` "Rust pre-commit checks" section** — the docs target for FR-009.
- **Sibling issue (bench restructure)** — moves perf benches to on-demand invocation, eliminating two of the three concurrent lbug builds per PR and reducing cache eviction pressure. This issue and the sibling are independently valuable but compose well.
