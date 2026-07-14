# Feature Specification: Upgrade lbug 0.17.0 → 0.18.1

**Feature Branch**: `fabrik/issue-190`
**Created**: 2026-07-14
**Status**: Draft
**Input**: User description: "Upgrade the graph engine's LadybugDB dependency from 0.17.0 to 0.18.1 to pick up lbug 18's stability fixes (needed by a downstream Liminis project). This is deliberately not a one-line dependency bump: the 0.18.1 native prebuilt bundle changed its macOS TLS backend from the self-contained bundled mbedtls to OpenSSL, which is exactly what broke the first v0.9.0 release build. The upgrade must re-solve external SSL linkage across the release platforms while preserving the IPC contract, WAL compatibility, and the prebuilt-bundle build path."

## Background

`lbug` is pinned two ways today: the Rust crate (`lbug = "=0.17.0"` in the workspace `Cargo.toml`) and the native prebuilt bundle (`LBUG_VERSION = "0.17.0"` in `.cargo/config.toml [env]`, which pins lbug's `build.rs` to `release:LadybugDB/ladybug/v0.17.0` instead of a floating `latest`). A downstream Liminis project needs lbug 18's stability fixes, motivating this upgrade to lbug 0.18.1 (crate and native bundle, versioned in lockstep).

This is not a routine version bump. The v0.9.0 release failure (PR #188) proved that the 0.17.0 self-contained fat archive merges mbedtls into `liblbug.a` and needs no external SSL, whereas the 0.18.x bundle links httplib against OpenSSL and fails at link time with `ld: symbol(s) not found for architecture arm64` on `ssl_st` / `ssl_ctx_st` unless libssl/libcrypto are provided. Because there is no macOS Rust build in CI — the release workflow is the only macOS compile — a macOS regression would otherwise stay invisible until a release is cut, so local macOS validation is a mandatory pre-merge gate for this change.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Maintainer adopts lbug 18 stability fixes (Priority: P1)

As a maintainer, I upgrade the engine to lbug 0.18.1 so the codebase and downstream consumers benefit from lbug 18's stability fixes, without breaking the release pipeline or the JSON-RPC contract.

**Why this priority**: This is the core motivation for the issue — a downstream project depends on the stability fixes. Without a working upgrade path, downstream consumption is blocked.

**Independent Test**: Bump the crate and native bundle pins, build the workspace, and run the full test suite (including `ipc_parity`) against lbug 0.18.1. The upgrade is proven independently of any release-publishing concern.

**Acceptance Scenarios**:

1. **Given** the workspace pinned to lbug 0.18.1, **When** `cargo build` and `cargo test --release` are run, **Then** the build succeeds and all tests pass with zero regressions versus 0.17.0.
2. **Given** the upgraded workspace, **When** the release workflow runs, **Then** prebuilt binaries are published for all three targets (`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`).
3. **Given** the upgraded workspace, **When** the `ipc_parity` corpus tests run, **Then** they pass with no fixture changes required.

---

### User Story 2 - Downstream consumer installs the upgraded release (Priority: P1)

As a downstream consumer installing a tagged release, the prebuilt binaries for macOS arm64, Linux x86_64, and Linux arm64 continue to install and run after the upgrade.

**Why this priority**: The prebuilt-binary install path is the primary distribution mechanism (per the one-line install documented in the README and CHANGELOG). A broken platform archive is a hard release blocker, not a degraded experience.

**Independent Test**: Run (or dry-run) the cargo-dist release build for each of the three targets and confirm each produces a working archive whose binary reaches its normal startup/config path.

**Acceptance Scenarios**:

1. **Given** a tagged release built against lbug 0.18.1, **When** a user installs the `aarch64-apple-darwin` archive, **Then** the binary links and runs without missing-symbol or missing-library errors.
2. **Given** the same release, **When** a user installs either Linux archive (`x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`), **Then** the binary links and runs, pulling in any newly required system packages (e.g. `libssl-dev`) automatically via the published dependency metadata.

---

### User Story 3 - Existing workspace keeps working (Priority: P2)

As an operator with a `.lcg/` workspace created under lbug 0.17.0, my WAL replays and my graph reads/writes continue to work after the engine upgrades, or a documented rebuild path is provided.

**Why this priority**: Data continuity matters, but it is lower priority than the build/release path because a documented rebuild-from-WAL fallback exists if the on-disk format changes — it is not a hard blocker the way a broken release is.

**Independent Test**: Replay a 0.17.0-era WAL fixture under the 0.18.1 engine and confirm the resulting graph state matches, or (if the on-disk index format changed) confirm `knowledge_rebuild_from_wal` reconstructs the graph correctly.

**Acceptance Scenarios**:

1. **Given** a `.lcg/` workspace with a WAL written under lbug 0.17.0, **When** the 0.18.1 engine opens and replays it, **Then** graph reads/writes behave identically to 0.17.0.
2. **Given** an on-disk format incompatibility is discovered (e.g. HNSW or FTS index layout changed), **When** `knowledge_rebuild_from_wal` is run against the existing WAL, **Then** the graph is fully reconstructed, and the change is documented in CHANGELOG.md rather than surfacing as a silent break.

---

### Edge Cases

- **On-disk format drift**: if lbug 0.18 changes the vector (HNSW) or FTS index format, an existing `.lcg/db` built under 0.17.0 may fail to open — this must be handled via rebuild-from-WAL and documented, not a silent break.
- **macOS OpenSSL discovery**: GitHub's `macos-14` runner ships Homebrew OpenSSL, but its prefix is not on the default link path — discovery must be robust (e.g. `brew --prefix openssl@3`) and must not hard-code a version-specific path.
- **SystemConfig defaults**: 0.17.0 introduced `throw_on_wal_replay_failure=true` and `enable_checksums=true`; confirm 0.18 defaults don't regress degraded-mode recovery (per ADR-0009/0025/0027).
- **Static vs perf bundle variant**: ladybug ships `-compat` / `-perf` Linux static variants; confirm `build.rs` selects a working variant for 0.18.1 as it did for 0.17.0.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Bump the `lbug` crate dependency from `=0.17.0` to `=0.18.1` in `Cargo.toml` workspace dependencies; refresh `Cargo.lock`.
- **FR-002**: Update `LBUG_VERSION` in `.cargo/config.toml` from `0.17.0` to `0.18.1` so the prebuilt native bundle matches the crate. `LBUG_PRECOMPILED_SOURCE` MUST resolve to `release:LadybugDB/ladybug/v0.18.1`.
- **FR-003**: Resolve the macOS (`aarch64-apple-darwin`) link failure introduced by the 0.18.1 bundle's OpenSSL TLS backend so `ld` resolves the httplib SSL symbols. Research MUST first determine whether the lbug 0.18 crate's `build.rs` emits the SSL link directives itself — if so, little or no extra config is needed. Otherwise, provide OpenSSL discovery + link flags (e.g. Homebrew `openssl` via `brew --prefix openssl` → `OPENSSL_DIR` / `rustc-link-search` + `rustc-link-lib=ssl,crypto`), injected through the cargo-dist `github-build-setup` fragment (`.github/build-setup.yml`) so it applies to release `build-local-artifacts` jobs.
- **FR-004**: Ensure both Linux targets (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`) still build and link. If the 0.18.1 bundle requires external OpenSSL on Linux too, add `libssl-dev` (or equivalent) to `[workspace.metadata.dist.dependencies.apt]`.
- **FR-005**: Continue to use the prebuilt lbug bundle. Do NOT enable source builds — they produce 7399 duplicate-symbol linker errors (LadybugDB/ladybug-rust#18).
- **FR-006**: Bump `LBUG_CACHE_BUST` in `.github/workflows/ci.yml` so CI rebuilds lbug against the 0.18.1 bundle rather than a stale 0.17.0 cache.
- **FR-007**: Preserve the JSON-RPC wire contract (Constitution Principle I — *IPC Surface Is a Stable Contract*). The bump MUST NOT change any request/response shape; the `ipc_parity` corpus tests remain green with no fixture changes.
- **FR-008**: Accommodate any lbug 0.18 API / `SystemConfig` changes surfaced by the crate bump (renamed items, new defaults) so the crate compiles cleanly with `-D warnings` and WAL-replay / schema behavior is preserved.
- **FR-009**: Update `CHANGELOG.md` with the lbug bump under `[Unreleased]`, noting any behavior/format changes and (if applicable) a rebuild-from-WAL note.

### Key Entities

- **lbug crate pin**: the Rust dependency version constraint in `Cargo.toml` (`lbug = "=0.17.0"` → `"=0.18.1"`).
- **Native prebuilt bundle**: the compiled `liblbug.a` + third-party archives, resolved by lbug's `build.rs` via `LBUG_VERSION` / `LBUG_PRECOMPILED_SOURCE`, downloaded from `release:LadybugDB/ladybug/v<version>`.
- **cargo-dist release build**: the per-target build job (three targets) that produces the published GitHub Release archives, configured via `[workspace.metadata.dist]` and the `.github/build-setup.yml` fragment.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The cargo-dist release workflow succeeds on all three targets and publishes a GitHub Release (validated by a dry run of the release build, e.g. `dist build` per target in CI or a tagged pre-release, before final tagging).
- **SC-002**: A local `cargo build --release -p lcg-service` on `aarch64-apple-darwin` links cleanly and the resulting binary runs (reaches the embedder-config check) — this is the mandatory pre-merge macOS gate given no macOS CI.
- **SC-003**: `cargo test --release` passes in full, including the `ipc_parity` corpus and WAL-replay tests — zero regressions versus 0.17.0.
- **SC-004**: `cargo clippy --release --all-targets -- -D warnings` is clean.
- **SC-005**: The R-003 dedup decision-overlap gate remains green.
- **SC-006**: `LBUG_PRECOMPILED_SOURCE` in the lbug build output reports `release:LadybugDB/ladybug/v0.18.1` on every platform.

## Assumptions

- lbug crate 0.18.1 is the correct match for ladybug native v0.18.1 (both current latest; versioned in lockstep).
- The prebuilt bundle remains the only supported build path (source build is broken upstream per LadybugDB/ladybug-rust#18).
- The x86_64 `<format>` C++20 toolchain issue is already resolved (v0.9.0 pinned the x86_64 release runner to `ubuntu-24.04`); no further action needed there.
- No macOS Rust build runs in CI, so local macOS validation (SC-002) is a hard pre-merge requirement for whoever implements this issue.

## Out of Scope

- Cutting a new tagged release of `liminis-context-graph` itself — this issue only prepares the `[Unreleased]` CHANGELOG entry; release tagging is a separate action.
- Any lbug 0.18 feature adoption beyond what's needed for the stability fixes and to keep the build/test/release path green (e.g. no new indexing features, no new `SystemConfig` options are being turned on proactively).
- Changes to the JSON-RPC method surface or schema beyond what's strictly required to compile against the new crate (per FR-007/FR-008).

## Source References

- PR #188 (`fix(release): pin lbug native bundle + bump x86_64 runner`) — prior art for the mbedtls/OpenSSL link-failure class of problem.
- `.cargo/config.toml`, `Cargo.toml` (workspace deps + `[workspace.metadata.dist]`), `.github/build-setup.yml`, `.github/workflows/ci.yml` — files this upgrade touches.
- `crates/core/tests/ipc_parity.rs` — parity corpus that must stay green.
- ADR-0009, ADR-0025, ADR-0027 — degraded-mode startup/recovery and WAL-resume playbook relevant to `SystemConfig` default verification.
- LadybugDB/ladybug-rust#18 — upstream issue documenting the source-build duplicate-symbol failure.
