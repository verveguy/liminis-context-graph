# Feature Specification: Upgrade lbug 0.19.1 -> 0.20.1 (storage 43 -> 47) so 0.14.0 carries a single migration

**Feature Branch**: `fabrik/issue-529`
**Created**: 2026-09-01
**Status**: Specified
**Input**: User description: "Upgrade lbug 0.19.1 -> 0.20.1 (storage 43 -> 47) so 0.14.0 carries a single migration. `Cargo.toml` pins `lbug = \"=0.19.1\"` and `.cargo/config.toml` pins `LBUG_VERSION = \"0.19.1\"` (both moved together by #398). Upstream has since shipped 0.20.0 (2026-08-29) and 0.20.1 (2026-08-31), which advance the on-disk storage format from version 43 to 47 in one release. Shipping 0.14.0 on 0.19.1 guarantees a second one-way migration later; going straight to 0.20.1 makes it one migration, 41 -> 47, while preserving the in-place open path for 0.17.0-era (v41) databases. Three upstream fixes land on code 0.14.0 introduces: a heap-corruption fix in the FTS query path (`ladybug#845`), an alignment fix in the primary-key-lookup path (`ladybug#837`), and a silent-row-loss fix in `LOAD FROM`/`UNWIND` feeding a `MATCH` primary-key predicate (`ladybug#864`, 0.20.1) — all three touch code paths this repo actively exercises (`CREATE_FTS_INDEX` on `Entity`, and the PK-lookup path #221 depends on). The OpenSSL/mbedtls static-linkage story fixed by #398/ADR-0398 must be re-verified by hand on macOS, since upstream `ladybug#830` namespace-wraps vendored mbedtls and CI is ubuntu-only. Both version pins must move together in the same commit, as in #398."

## Background

`Cargo.toml` pins `lbug = "=0.19.1"` and `.cargo/config.toml` pins `LBUG_VERSION = "0.19.1"` (both moved together by #398, see `specs/398-upgrade-lbug-0-17/spec.md` and ADR-0398). Upstream has since shipped 0.20.0 (2026-08-29) and 0.20.1 (2026-08-31).

**Why this belongs in 0.14.0, not later.** Storage versions, from `src/include/storage/storage_version_info.h`:

| lbug | storage version |
|---|---|
| 0.17.0 | 41 |
| **0.19.1** (current pin) | **43** |
| **0.20.0 / 0.20.1** | **47** |

0.20.x advances four storage versions in one release (44 CSR flag, 45 rel-table CSR sorted-by-dest, 46 RANGE/HASH partitioning metadata, 47 LIST partitioning).

Shipping 0.14.0 on 0.19.1 guarantees a second one-way migration later: 41 -> 43 now, 43 -> 47 whenever 0.20 is eventually taken. 0.14.0 was deliberately scoped so users adapt to one breaking change; a stale version pin defeats that. Going straight to 0.20.1 makes it **one** migration, 41 -> 47. The in-place open path is preserved: 0.20.1's `canReadStorageVersion` accepts storage versions 40 through 45 plus the current version (47), so a 0.17.0-era (v41) database still opens directly, exactly as it does under 0.19.1. As with #398, the WAL is untouched — `.lcg/wal/` is project-owned JSONL, independent of lbug's storage format, so WAL replay compatibility is not a concern this issue needs to re-verify.

**Three upstream fixes land on code 0.14.0 introduces**, motivating the bump beyond pure migration consolidation:

- **`LadybugDB/ladybug#845`** — heap corruption in the FTS query path when scans race committing writers. lcg creates `CREATE_FTS_INDEX('Entity', 'node_name_and_summary', ['name', 'summary'])` and runs as a concurrent reader/writer service, so this is memory-unsafety on an actively exercised path.
- **`LadybugDB/ladybug#837`** — align `QueryPrimaryKeyLookup` rows with the input chunk selection. #221's success criterion is literally `PRIMARY_KEY_SCAN_NODE_TABLE ... Index: ART`; this is that code path.
- **`LadybugDB/ladybug#864`** (0.20.1) — silent row loss when `LOAD FROM`/`UNWIND` feeds a `MATCH` primary key predicate. Again the PK-lookup path #221 now depends on.

**Re-verifying OpenSSL/mbedtls linkage is required, not assumed.** `LadybugDB/ladybug#830` namespace-wraps vendored mbedtls to avoid clashes with statically-linked duckdb. mbedtls-versus-OpenSSL linkage is exactly what broke the v0.9.0 release (macOS `ld: symbol(s) not found`) and what #398/ADR-0398 fixed by forcing a static OpenSSL link. A vendoring change upstream may perturb that, and CI is ubuntu-only, so this bump is not complete until the macOS static-link check has been re-run by hand — see User Story 2.

`.cargo/config.toml` is protected by a built-in Claude Code guard: `Edit`/`Write` on it are refused regardless of permission settings, because the file can set `linker`, `rustflags`, and `[target.*.runner]`. This is expected behavior, not a misconfiguration to work around — the implementer stages the intended content elsewhere and uses `Bash` (`cp`) to place it, confirming first that the staged content is `[env]`-only with no `runner`/`linker`/`rustflags` additions.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Both version pins move together and the workspace builds and tests clean (Priority: P1)

As a maintainer of this repository's dependency footprint, I want the `lbug` crate pin and the native prebuilt-bundle version pin bumped together from `0.19.1` to `0.20.1` in the same commit, with any resulting API differences absorbed, so that the crate pin and the native bundle never skew apart and the workspace compiles and passes its full test surface.

**Why this priority**: without both pins moving together and the workspace compiling, nothing else in this issue can be evaluated. A skewed pin (crate pinned to one version, native bundle downloaded at another) is the exact failure mode that broke the v0.9.0 release, and is called out explicitly as a required constraint.

**Independent Test**: Bump `lbug = "=0.20.1"` in `Cargo.toml` and `LBUG_VERSION = "0.20.1"` in `.cargo/config.toml` in the same commit, run a local build, and run the full CI suite (including the five real-corpus e2e jobs).

**Acceptance Scenarios**:

1. **Given** `lbug = "=0.19.1"` in `Cargo.toml` and `LBUG_VERSION = "0.19.1"` in `.cargo/config.toml`, **When** both are bumped to `0.20.1`, **Then** both changes land in the same commit.
2. **Given** the bumped pins, **When** the workspace is built, **Then** it compiles cleanly, with any lbug API differences between 0.19.1 and 0.20.1 absorbed at call sites.
3. **Given** a clean compile, **When** the full CI suite runs, **Then** it is fully green, including all five real-corpus e2e jobs — compiling cleanly is necessary but is explicitly not sufficient acceptance evidence on its own, given roughly 300 upstream files changed between these versions.

---

### User Story 2 - macOS static OpenSSL linkage is re-verified by hand, not assumed (Priority: P1)

As a maintainer responsible for the release build, I want the macOS arm64 release binary re-checked by hand for static OpenSSL linkage after the bump, so that a namespace-wrapping change to vendored mbedtls upstream (`ladybug#830`) does not silently reintroduce the dynamic-linkage regression that broke the v0.9.0 release.

**Why this priority**: CI is ubuntu-only and `release.yml` builds nothing until a tag exists, so no automated signal currently exists for this platform-specific risk. This exact failure mode (mbedtls-vs-OpenSSL skew causing `ld: symbol(s) not found` on macOS) has happened twice before (v0.9.0, and the motivating case for #398/ADR-0398).

**Independent Test**: In a detached worktree from the feature branch, run `scripts/stage-openssl-static.sh`, build the release binary, run `scripts/assert-static-openssl.sh` against it, and inspect `otool -L` output by hand.

**Acceptance Scenarios**:

1. **Given** a detached worktree built from the feature branch, **When** `eval "$(bash scripts/stage-openssl-static.sh)"` followed by `cargo build --release --bin liminis-context-graph` runs, **Then** the build succeeds.
2. **Given** the release binary, **When** `scripts/assert-static-openssl.sh target/release/liminis-context-graph` runs, **Then** it passes.
3. **Given** the release binary, **When** `otool -L target/release/liminis-context-graph` runs, **Then** it shows only system frameworks — no `libssl`/`libcrypto`, no Homebrew paths.
4. **Given** the release binary, **When** it is started against a temporary workspace configured with an OpenAI-compatible embedder, **Then** `knowledge_status` succeeds and a write/read round trip completes correctly.

---

### User Story 3 - A pre-existing storage-v41 database migrates in place with no manual operator step (Priority: P1)

As an operator of a running lcg service with a database created under a 0.17.0-era binary (storage version 41), I want that database to open under the new 0.20.1-based binary, migrate automatically, and serve correct reads, so that upgrading does not require any manual recovery step and does not silently corrupt or lose data.

**Why this priority**: data loss, corruption, or an operator-facing manual migration step is the highest-severity failure mode for a storage-format-advancing dependency bump. The issue is explicit that the in-place open path (`canReadStorageVersion` accepting 40–45 plus current) must be exercised, not merely assumed from release notes.

**Independent Test**: Obtain or generate a database at storage version 41 (e.g. by creating one under the pre-upgrade 0.19.1 binary or an earlier one, if version 41 is not already reachable from a repo fixture), open it under the new 0.20.1-based binary, and confirm it migrates and serves correct reads.

**Acceptance Scenarios**:

1. **Given** a pre-existing database at storage version 41, **When** it is opened under the 0.20.1-based binary, **Then** it opens successfully and migrates without any manual operator step.
2. **Given** the migrated database, **When** reads are issued against it, **Then** results are correct and consistent with the pre-migration data.

---

### User Story 4 - Three upstream correctness fixes on actively exercised paths are picked up (Priority: P2)

As a maintainer relying on lbug's FTS index and primary-key-lookup query paths, I want the fixes shipped in `ladybug#845` (FTS heap corruption under concurrent scan/write), `ladybug#837` (primary-key-lookup row/chunk alignment), and `ladybug#864` (silent row loss in `LOAD FROM`/`UNWIND` feeding a `MATCH` primary-key predicate) included in the version this repo ships, so that code paths already exercised by lcg (`CREATE_FTS_INDEX`, and the PK-lookup path #221 depends on) are not running on known-defective upstream code.

**Why this priority**: these are upstream-internal fixes without a standalone repro available in this repo, so they cannot be independently verified beyond confirming the version pin includes them and that the existing test surface (which exercises both paths) continues to pass. It is P2 relative to User Stories 1–3, which are the mechanical and safety-critical prerequisites; this story records the fixes as motivation and confirms they ship, not as a targeted regression test.

**Independent Test**: Confirm the pinned lbug version (0.20.1) postdates each fix's landing release, and confirm existing tests exercising FTS queries and primary-key lookups continue to pass under the new pin.

**Acceptance Scenarios**:

1. **Given** the 0.20.1 pin, **When** the pinned version is checked against upstream's fix history, **Then** all three fixes (`#845`, `#837`, `#864`) are included.
2. **Given** the bumped pin, **When** existing tests that exercise `CREATE_FTS_INDEX` queries and primary-key lookups run, **Then** they continue to pass.

---

### Edge Cases

- What happens if the roughly 300 upstream files changed between 0.19.1 and 0.20.1 include a breaking Rust/C API change with no mechanical fix? A compile-clean result is necessary but not sufficient acceptance evidence (User Story 1); the e2e suites are the real gate, and a non-mechanical resolution is a Research/Plan-stage decision, not a spec change.
- What happens if `ladybug#830`'s mbedtls namespace-wrapping perturbs the static-OpenSSL-link approach established by #398/ADR-0398? User Story 2 requires this be caught by hand on macOS before the bump is considered complete, since CI provides no automated signal for it.
- What happens if a storage-v41 database fails to open or migrate cleanly under 0.20.1, contrary to the documented `canReadStorageVersion` range? This blocks the issue — FR-003 requires an explicit, automatic, no-manual-step migration, not a documented workaround.
- What happens if no storage-v41 fixture is readily available to exercise User Story 3? Per Assumptions, one can be generated under an earlier pinned binary; this is a Research-stage detail, not a spec blocker.
- What happens if the .cargo/config.toml comments describing 0.19.1-specific OpenSSL-linkage facts (e.g. "Neither OPENSSL_DIR nor OPENSSL_ROOT_DIR is read by the published 0.19.1 crate") no longer hold under 0.20.1, given `ladybug#830`'s vendoring change? These MUST be reviewed and corrected where the underlying fact has changed (FR-006), consistent with the precedent set in #398.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `Cargo.toml`'s `lbug` pin and `.cargo/config.toml`'s `LBUG_VERSION` pin MUST both move to `0.20.1`, changed in the same commit.
- **FR-002**: `.cargo/config.toml` MUST be edited by staging its intended content and copying it into place with `Bash` (not `Edit`/`Write`, which are refused on this file by design), and the staged content MUST be confirmed `[env]`-only with no `runner`/`linker`/`rustflags` additions before being placed.
- **FR-003**: A pre-existing database at storage version 41 MUST open under the new binary, migrate in place (41 -> 47) automatically, and serve correct reads, with no manual operator step required.
- **FR-004**: The macOS arm64 release build MUST be verified by hand (CI is ubuntu-only): it MUST pass `scripts/assert-static-openssl.sh`, and `otool -L` MUST show only system frameworks — no `libssl`/`libcrypto`, no Homebrew paths.
- **FR-005**: The macOS release binary MUST be exercised with a live write/read round trip against a temporary workspace configured with an OpenAI-compatible embedder, confirming `knowledge_status` succeeds and both the write and the read complete correctly.
- **FR-006**: Version-specific comments in `.cargo/config.toml` and `Cargo.toml` that describe 0.19.1-specific facts (e.g. OpenSSL-vs-mbedtls linkage behavior) MUST be reviewed against 0.20.1 and corrected wherever the underlying fact has changed; facts that still hold MUST be left as-is.
- **FR-007**: Any lbug Rust/C API differences between 0.19.1 and 0.20.1 MUST be absorbed so the workspace compiles cleanly; compiling cleanly alone is not sufficient acceptance evidence — full CI, including all five real-corpus e2e jobs, MUST also pass.
- **FR-008**: `CHANGELOG.md` MUST record the single 41 -> 47 storage migration in one entry, and MUST state that rollback means removing `.lcg/db/` and rebuilding from the WAL.
- **FR-009**: No new lbug 0.20 capability (partitioning, CSR projection, GQL extension, `AI EXTRACT`) MAY be adopted as part of this change — this is a version bump for migration-consolidation and correctness fixes only.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `Cargo.toml` pins `lbug = "=0.20.1"` and `.cargo/config.toml` pins `LBUG_VERSION = "0.20.1"`, both changed in the same commit.
- **SC-002**: Full CI is green, including all five real-corpus e2e jobs.
- **SC-003**: A pre-existing storage-v41 database opens in place under the new binary, migrates, and serves correct reads with no manual operator step.
- **SC-004**: The macOS arm64 release build passes `scripts/assert-static-openssl.sh`, shows only system frameworks under `otool -L` (no `libssl`/`libcrypto`, no Homebrew paths), and serves a live write/read round trip.
- **SC-005**: `CHANGELOG.md` records the single 41 -> 47 migration and the rollback procedure (remove `.lcg/db/`, rebuild from WAL).

## Assumptions

- A storage-v41 database fixture either already exists in the repo's test fixtures, or can be generated by creating a database under an earlier pinned lbug binary (e.g. 0.17.0) before the pins are bumped. If neither holds, that is a Research-stage finding, not a spec blocker.
- The WAL format itself (`.lcg/wal/`, project-owned JSONL) is unaffected by lbug's storage-version advance, per the issue's own framing and consistent with #398 — this issue does not need to independently re-verify WAL replay compatibility.
- The three upstream fixes (`ladybug#845`, `#837`, `#864`) cannot be independently regression-tested from within this repo without a standalone upstream repro; User Story 4's verification is confirming inclusion and confirming existing FTS/PK-lookup tests still pass, not authoring a new targeted repro.
- The macOS static-OpenSSL-linkage re-verification (User Story 2) requires a machine capable of building for `aarch64-apple-darwin`; this is a manual, by-hand check as the issue itself specifies, not something to attempt to automate as part of this issue's scope.
- lbug's public Rust/C API surface used by this repo (`crates/core`, `crates/service`) has no breaking changes between 0.19.1 and 0.20.1 beyond what can be resolved mechanically at call sites; if a resolution is not mechanical, FR-007 still governs the outcome (compile-clean and full CI green), and the resolution approach is a Research/Plan-stage decision, not a spec change.

## Out of Scope

- Adopting any new lbug 0.20 capability: partitioning (RANGE/HASH/LIST), CSR projection, the GQL extension, or `AI EXTRACT`. This issue is a version bump for migration-consolidation and correctness-fix purposes only; new capabilities get their own issues.
- A separate 0.20 bump targeted at 0.15.0 — this issue supersedes that need entirely.
- Root-causing or fixing anything beyond what upstream's 0.20.0/0.20.1 releases already address; if the macOS linkage re-verification (User Story 2) surfaces a new, unrelated problem, that is a follow-up issue, not an expansion of this one.

## Source References

- `Cargo.toml` (workspace `lbug` pin), `.cargo/config.toml` (`LBUG_VERSION` env pin and its OpenSSL/mbedtls-linkage comments)
- `scripts/stage-openssl-static.sh`, `scripts/assert-static-openssl.sh` — the static-OpenSSL-linkage staging and assertion scripts from #398
- `CHANGELOG.md` — records the single 41 -> 47 migration and rollback procedure
- ADR-0398 — OpenSSL linkage for release artifacts (the precedent this issue's User Story 2 re-verifies)
- `specs/398-upgrade-lbug-0-17/spec.md` — the 0.17.0 -> 0.19.1 precedent this issue directly follows
- Issue #221 — blocked on the primary-key-lookup path that `ladybug#837`/`#864` fix
- `src/include/storage/storage_version_info.h` (upstream `LadybugDB/ladybug`) — storage version table (41, 43, 47)
- Upstream `LadybugDB/ladybug` PRs `#845`, `#837`, `#864`, `#830`, and releases 0.20.0, 0.20.1
