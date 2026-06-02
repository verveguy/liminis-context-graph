# Feature Specification: Bump lbug Pin From 0.16.1 to 0.17.0

**Feature Branch**: `fabrik/issue-126`
**Created**: 2026-06-01
**Status**: Draft
**Input**: 2026-06-01 — `lbug` (LadybugDB Rust bindings) shipped 0.17.0 on crates.io ~2026-05-28. We are pinned to `=0.16.1` (exact). This is a single-version minor bump pre-1.0 — by convention pre-1.0 minor versions *can* introduce breaking changes, so the bump needs validation rather than a mechanical `cargo update`. The bump itself is one line; the Research is reading what changed and confirming our integration still works. Filing as a Fabrik issue rather than hand-driving so the changelog audit, integration verification, and any required adaptations get explicit Research and Plan stages on the record.

## Background

**Current state:**

- `Cargo.toml` workspace declares `lbug = "=0.16.1"`
- `liminis-graph-core/Cargo.toml` consumes it via `lbug = { workspace = true }`
- `Cargo.lock`: `lbug 0.16.1`, checksum `4c8b41c60b6c498f4743d2762a6d0bfef34f584c16300282adc239fd60db3dd9`
- One consumer crate; no other pinned lbug-sys-style shim
- `LBUG_BUILD_FROM_SOURCE=1` set in `.github/workflows/ci.yml` and `.cargo/config.toml` (prebuilt lbug archive lacks third-party static deps)
- `#115`'s lbug-cache keys on the lbug-sys version in `Cargo.lock`. **This bump invalidates the cache exactly once** — the first PR after the bump pays full ~1h CI cost, subsequent PRs cache-hit again at ~7 min. Known and acceptable.

**Why a Fabrik issue and not just a worktree-and-PR bump:**

- The LadybugDB Rust repo (<https://github.com/LadybugDB/ladybug-rust>) does NOT publish GitHub Releases or a CHANGELOG (verified via `gh api`). Identifying "what changed in 0.17.0" requires reading the repo's commit log between the tags or diffing the source. That's research-stage work worth doing explicitly.
- We have ~25 production call sites for lbug across `liminis-graph-core` and the integration tests; any subtle API or behavior change (e.g., new error variant, changed error message text, query-result iterator semantics, vector-index API tweak, WAL behavior, lock semantics) needs to be caught now rather than at runtime.
- We've previously fixed two lbug-specific bugs that interacted with subtle behaviors: `Db::open` mmap-8TB sizing on macOS (`.cargo/config.toml` thread cap), and the `QueryResult` destructor segfault (graphiti fork PR `c8ad76d`). Pre-1.0 minor bumps can re-surface this class of issue.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Production code paths still work under 0.17.0 (Priority: P1)

After the bump, all existing `liminis-graph-core` integration tests pass, all `liminis-graph` migration tests pass, and the binary continues to serve the same IPC contract it does today.

**Why this priority**: any regression here breaks the running liminis-app via the IPC contract, so it has to be caught on the bump PR, not after release.

**Independent Test**: Run `cargo test --release` (the CI-equivalent invocation) against the bumped tree on Ubuntu and on macOS Apple Silicon. All tests pass.

**Acceptance Scenarios**:

1. **Given** the bump is applied, **When** the full CI suite runs, **Then** all tests pass on `ubuntu-latest` and (locally, since macOS CI is currently disabled) on the maintainer's Apple Silicon machine.
2. **Given** the bumped binary, **When** the user starts it with a 0.16.1-created workspace `.lcg/db/liminis.db`, **Then** either it reads the database without error, or (if 0.17.0 changed the on-disk format) the bump still lands with the CHANGELOG noting the format change and the PR description including a one-line workspace-recreation note.

---

### User Story 2 — Behavior deltas between 0.16.1 and 0.17.0 are documented (Priority: P1)

A future engineer reading `CHANGELOG.md` or the PR body can see what changed in lbug 0.17.0 that matters to us, sourced from upstream rather than guessed.

**Why this priority**: without this, future debugging of "did this change in the lbug bump?" is open-ended. Pinning down the delta in writing now saves hours later.

**Acceptance Scenarios**:

1. **Given** the bump PR's description, **When** read, **Then** it lists (a) the source consulted (commit log between tags v0.16.1..v0.17.0 in `LadybugDB/ladybug-rust`), (b) a bulleted summary of changes relevant to us (API surface we use, behavior changes, error surface, performance hints), (c) explicit "no change" notes for the surfaces we care about that did NOT change.
2. **Given** `CHANGELOG.md`, **When** read after merge, **Then** a one-line `### Changed` entry under `[Unreleased]` records the bump and links to the PR (and calls out any on-disk format change if applicable).

---

### User Story 3 — Cache invalidation is expected, not a regression (Priority: P3)

The maintainer is not surprised when the bump PR's first CI run takes ~1h instead of the post-#115 ~7 min envelope.

**Acceptance Scenarios**:

1. **Given** the bump PR, **When** CI runs, **Then** the build-lbug-cache job misses cleanly, rebuilds the C++ source, and uploads new artifacts under the new cache key. (`#115` FR-006 explicitly designed for this.)
2. **Given** the bump merges to main, **When** the next unrelated PR opens, **Then** it hits the freshly-populated lbug cache and runs in ~7 min.

---

### Edge Cases

- **0.17.0 has a different on-disk DB file format.** The bump still lands. Research notes the format change; the CHANGELOG entry calls it out explicitly; the PR description includes a one-line note that the maintainer must recreate the workspace. No migration code, no separate issue — sole user is the maintainer.
- **0.17.0 changes the error text for "missing vector index"** (or similar). Our auto-heal logic in `liminis-graph-core/src/handlers.rs` greps on this text. If it changes, we update the grep in the same PR (small, focused; in scope per FR-004(a)).
- **0.17.0's `QueryResult::Drop` semantics regress** (we had to fix this in graphiti fork PR `c8ad76d`). If we see a segfault during the test run on the bump branch, that's a hard regression; file upstream issue, do NOT bump, keep `=0.16.1`.
- **0.17.0 changes the vector-extension cache layout** under `~/.lbdb/extension/`. Migration tests serialize `Db::open` to avoid races against this cache (per `#104`). New layout means a fresh cache directory on first run; no test failure expected, but worth a sanity check in Research.
- **0.17.0 bumps a transitive C++ dep** (yyjson, lz4, brotli, fastpfor, zstd, etc.) that affects the static-archive bundling. If our `LBUG_BUILD_FROM_SOURCE=1` workaround stops working, the bump is gated on either (a) lbug fixing their prebuilt to ship the dep archives, or (b) us updating our build invocation. Out of scope to fix here if it surfaces.
- **Research stage finds 0.17.0 was yanked** between filing and implementation. In that case, this issue closes WONTFIX and a fresh issue handles 0.17.1 (or whatever's current).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001.** `Cargo.toml`'s workspace dependency MUST change from `lbug = "=0.16.1"` to `lbug = "=0.17.0"` (preserving the exact-pin format).
- **FR-002.** `Cargo.lock` MUST be regenerated to match (via `cargo update -p lbug --precise 0.17.0` followed by `cargo build` to refresh transitive bindings).
- **FR-003.** Research stage MUST consult the LadybugDB/ladybug-rust repository (commit log between `v0.16.1` and `v0.17.0`, or whatever the upstream tagging convention is) and produce a written summary covering at minimum:
  - **Public API surface changes** affecting any method/type we use (`Db`, `Connection`, `QueryResult`, `PreparedStatement`, `Value`, `LogicalType`, the vector-extension calls, the FTS calls, error types)
  - **Error type/text changes** — we have integration tests that grep on lbug error text for the "vector index missing" / "FTS missing" cases that drive our auto-heal logic
  - **On-disk format changes** — note whether the DB file format changed. If it did, the bump still lands: the CHANGELOG entry calls it out, and the PR description includes a one-line note that the maintainer must recreate the workspace. No migration code required.
  - **Behavior changes** to: WAL semantics, write serialization, mmap sizing on macOS, vector extension cache layout (`~/.lbdb/extension/`), query-result iterator semantics, transactional semantics
  - **Build / linker changes** — if 0.17.0 changes the third-party static-archive bundling expectations, the `LBUG_BUILD_FROM_SOURCE=1` story may need re-verification
- **FR-004.** If FR-003 surfaces any incompatibility with our integration, the Plan stage MUST EITHER:
  - (a) Include the adaptations needed in this PR — small, focused changes only (e.g., updated error-text grep, on-disk format noted in CHANGELOG with a workspace-recreation note in the PR description).
  - (b) Abandon the bump and keep `=0.16.1` only for hard runtime regressions that cannot be worked around (e.g., a `QueryResult::Drop` segfault). File an upstream issue. On-disk format changes alone do NOT trigger this path.
- **FR-005.** All existing pre-commit gates MUST pass: `cargo fmt --all --check && cargo test --release && cargo clippy --all-targets -- -D warnings`. `cargo test --release` is the CI-mode invocation (debug-mode linking OOMs on Ubuntu).
- **FR-006.** Special verification: the migration tests in `liminis-graph/tests/migration_binary.rs` (which serialize `Db::open` to avoid the lbug vector-extension cache race per `#104`'s fix) MUST still pass. If lbug 0.17.0 changed the extension cache layout under `~/.lbdb/extension/<ver>/`, the cache version path changes and tests pick up a fresh cache on first run; that's expected and not a failure.
- **FR-007.** `CHANGELOG.md` under `[Unreleased]` MUST gain a `### Changed` line: `- bump lbug pin from 0.16.1 to 0.17.0 (see PR #NNN for delta summary)`. If Research found an on-disk format change, the line MUST call that out explicitly.
- **FR-008.** This PR MUST NOT touch any other dependency. Even if Cargo regenerates the lockfile and other transitive deps are slightly out of step, do NOT bundle unrelated bumps. Single-purpose PR.
- **FR-009.** The PR description MUST include the Research-stage summary verbatim (or link to a comment on the issue that contains it), so reviewers don't have to re-derive what changed. If the on-disk format changed, a one-line workspace-recreation note MUST appear in the PR description.
- **FR-010.** Remove `LBUG_BUILD_FROM_SOURCE` settings from `.cargo/config.toml` and CI workflows. As of lbug 0.17.0, the prebuilt is a self-contained fat bundle (all third-party archives merged into `liblbug.a` via `BundleStaticLibrary.cmake`); building from source causes 7399 duplicate-symbol linker errors because `link_bundled_deps=true` links both the fat archive and the individual archives. The lbug-cache key derivation and all other CI settings remain unchanged.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001.** `lbug = "=0.17.0"` is pinned in workspace `Cargo.toml`; `Cargo.lock` reflects the matching version and checksum.
- **SC-002.** All CI gates pass on the bump PR (`cargo fmt --check`, `cargo test --release`, `cargo clippy --all-targets -- -D warnings`).
- **SC-003.** The PR description contains a Research-derived summary of what changed in lbug 0.17.0 that touches our integration.
- **SC-004.** `CHANGELOG.md` `[Unreleased]` has a one-line `### Changed` entry recording the bump (and any on-disk format change, if applicable).
- **SC-005.** No unrelated dependency bumps in the same PR.
- **SC-006.** Migration tests (per `#104`'s serialized `Db::open` fix) still pass.
- **SC-007.** First post-bump PR on main (any unrelated PR) hits the freshly-populated lbug cache and runs in ~7 min, confirming the cache invalidation worked as designed.

## Assumptions

- **A1.** lbug 0.17.0 was published in good faith, is not yanked, and represents the upstream maintainer's intended next release. Verifiable via `cargo search lbug` or crates.io API at implementation time.
- **A2.** Most of our lbug call sites are stable across 0.16.1 → 0.17.0. Pre-1.0 minor bumps tend to add features rather than break existing ones; the breakages we worry about are the focused exceptions, not the default.
- **A3.** The lbug-cache key in `#115` correctly invalidates on lbug-sys version change. Verified by the SC-003 design of `#115`: "A PR bumping lbug-sys misses the cache cleanly, completes in roughly today's baseline, and populates the cache."
- **A4.** The maintainer is okay paying the one-time ~1h CI cost on this bump PR in exchange for fresh upstream features and bug fixes.
- **A5.** The on-disk DB format may or may not change; Research stage explicitly checks. If it changed, the bump still lands — the CHANGELOG entry notes it and the maintainer recreates the workspace. The sole user is the maintainer, so workspace recreation is an acceptable resolution.

## Out of Scope

- Unpinning to a caret range (e.g. `lbug = "0.17"`). Keep exact pinning — predictable rebuilds, controlled bump cadence, matches existing convention.
- Bumping any other crate.
- Performance benchmarking the delta. If 0.17.0 changes hot-path performance materially, that's a separate observation worth a follow-up, but we don't gate the bump on bench parity.
- Adopting new lbug 0.17.0 features (vector indices, query planner hints, etc.). Bumping the floor, not the ceiling.
- Updating CLAUDE.md to mention the new version (CLAUDE.md doesn't pin a version today; if Research finds a behavior change worth documenting for future engineers, add it; otherwise leave alone).
- Writing migration code for on-disk format changes. Sole user is the maintainer; workspace recreation is sufficient.

## Source References

- **`#115` (merged, PR #118)** — lbug C++ build cache. Cache key includes the lbug-sys version, so this bump invalidates it exactly once. Designed-for behavior per SC-003 of #115.
- **`#104` (merged)** — migration tests serialize `Db::open` to avoid the lbug vector-extension cache race. Relevant to FR-006: verify still passes.
- **graphiti fork PR `c8ad76d`** — fixed `QueryResult::Drop` segfault during WAL reload. Pre-1.0 lbug bug; relevant to the edge case where a bump regresses this.
- **lbug crate**: <https://crates.io/crates/lbug> — currently 0.17.0 (max stable, published 2026-05-28).
- **lbug source repo**: <https://github.com/LadybugDB/ladybug-rust> — no GitHub Releases or CHANGELOG.md found via `gh api` as of issue filing; Research must derive the delta from the commit log between tags.
- **`#124` (in flight)** — prebuilt-binary release via cargo-dist. Bumping lbug before that lands is fine: the first release artifact will simply embed 0.17.0 lbug. If the bump comes after, the next release picks it up automatically.
- **`Cargo.toml` lines 4-9** — captures the rationale for `[profile.dev] debug = "line-tables-only"` (Ubuntu CI OOM during linking). Unaffected by this bump.
- **`.cargo/config.toml`** — `RUST_TEST_THREADS=4` (to avoid macOS mmap exhaustion from lbug's 8TB-per-`Db::open`) remains unchanged. `LBUG_BUILD_FROM_SOURCE=1` was removed: the 0.17.0 prebuilt is a self-contained fat bundle that ships all third-party archives, making the source-build workaround both unnecessary and actively harmful. See LadybugDB/ladybug-rust#18.
