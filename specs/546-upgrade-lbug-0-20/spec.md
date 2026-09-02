# Feature Specification: Upgrade lbug 0.20.1 -> 0.20.2 (stale rows from re-executed parameterized queries on the cached-plan path)

**Feature Branch**: `fabrik/issue-546`
**Created**: 2026-09-02
**Status**: Specified
**Input**: User description: "Cargo.toml pins lbug = \"=0.20.1\" and .cargo/config.toml pins LBUG_VERSION = \"0.20.1\" (both moved together by #529). Upstream shipped 0.20.2 on 2026-09-01, one day after 0.20.1. It fixes a silent-wrong-answer bug on a path this project uses everywhere: 'fix: re-executed parameterized queries returned stale rows on the cached-plan fast path' (LadybugDB/ladybug#877, fixed by #878). crates/core/src/db.rs runs essentially all of its reads and writes through query_params/exec_params, i.e. re-executed parameterized queries — this is not a hypothetical exposure, it is the normal execution path. 0.20.2 also carries 'Fix several planner bugs' (#894) and an ArrowResultCollector downcast fix (#884). Storage version is unchanged: 0.20.1 and 0.20.2 both map to STORAGE_VERSION_47 per src/include/storage/storage_version_info.h, so this is a drop-in bump with no migration implication. This came out of investigating a CI run on chore/release-0.14.0 where four jobs hung or timed out inside lbug database calls on four separate runner VMs; it did not reproduce on a re-run or across 40 local iterations of the 531-test lcg-core suite. A rollback to 0.19.x was considered and rejected: ladybug#898 (segfault+deadlock on DROP/rebuild with an orphaned index holder) is present identically across 0.19.0/0.19.1/0.20.0/0.20.1/0.20.2/main, so rolling back does not avoid it; 0.19.1 still carries the FTS heap-corruption bug fixed by ladybug#845, trading an unreproduced hang for a proven memory-safety bug; and rolling back reinstates the two-migration problem 0.14.0 was scoped to avoid. The enable_cached_prepared_statement='NONE' workaround from ladybug#883 is not available on this pin — dumping the pinned liblbug.a shows no such setting; it was added to main after 0.20.1. 0.20.2 does not fix ladybug#883 (SIGSEGV in the cached-prepared-statement path), which is against main after #878 and requires hundreds of parameterized queries in one session; no evidence of hitting it, but worth tracking. Required: move both pins to 0.20.2 in the same commit; .cargo/config.toml must be staged and cp'd into place since Edit/Write are refused on it by design; re-run the macOS static-OpenSSL verification by hand per ADR-0398 since CI is ubuntu-only and the native bundle changes with version. Acceptance: both pins read 0.20.2 changed together; full CI green including all five real-corpus e2e suites; a pre-existing storage-v41 database still opens in place and migrates (storage_v41_migration test from #529 covers this); macOS arm64 release build passes the static-OpenSSL guard and shows only system frameworks under otool -L; CHANGELOG's lbug entry updated to say 0.20.2 with the 41 -> 47 migration story unchanged. Follows #529. Blocks the v0.14.0 tag."

## Background

`Cargo.toml` pins `lbug = "=0.20.1"` and `.cargo/config.toml` pins `LBUG_VERSION = "0.20.1"` (both moved together by #529, see `specs/529-upgrade-lbug-0-19/spec.md`). Upstream shipped **0.20.2 on 2026-09-01**, one day after 0.20.1.

**Why this belongs in 0.14.0: it fixes a silent-wrong-answer bug on a path this project uses everywhere.** `LadybugDB/ladybug#877`, fixed by `#878`, describes re-executed parameterized queries returning stale rows on the cached-plan fast path. `crates/core/src/db.rs` runs essentially all of its reads and writes through `query_params`/`exec_params` — i.e. re-executed parameterized queries. A stale-row result from a knowledge graph is the worst failure class available: no crash, no error, no log line, just wrong answers that look plausible. This is not a hypothetical exposure; it is the project's normal execution path. 0.20.2 also carries "Fix several planner bugs" (`#894`) and an `ArrowResultCollector` downcast fix (`#884`).

**Cost is essentially zero.** Storage version is unchanged: confirmed against `src/include/storage/storage_version_info.h`, both 0.20.1 and 0.20.2 map to `STORAGE_VERSION_47`. This is a drop-in bump with no migration implication, no change to 0.14.0's upgrade story, and no change to the release notes' migration section.

**Context: why now, and why not a rollback instead.** This came out of investigating a CI run on `chore/release-0.14.0` where four jobs hung or timed out inside lbug database calls — `Db::open` + `CREATE ART INDEX` (no embedder involved), an episode resolve, and a WAL rebuild — on four separate runner VMs. It did not reproduce: a re-run of the identical commit was fully green, and 40 local iterations of the 531-test `lcg-core` suite at `RUST_TEST_THREADS=4` produced no hangs. A rollback to 0.19.x was considered and rejected:

- **`LadybugDB/ladybug#898`** (segfault + deadlock on `DROP`/rebuild with an orphaned index holder, triggered by the FTS extension's non-atomic index creation — this project runs `CREATE_FTS_INDEX`) states the affected code is identical across v0.19.0 / v0.19.1 / v0.20.0 / v0.20.1 / v0.20.2 / main. Rolling back does not avoid it.
- 0.19.1 still carries the FTS heap-corruption bug that `ladybug#845` fixed — a memory-safety bug on a path queried concurrently as a live service. Rolling back trades an unreproduced hang for a proven bug.
- Rolling back also reinstates the two-migration problem 0.14.0 was scoped to avoid (41 -> 43 now, 43 -> 47 later).

**The `enable_cached_prepared_statement='NONE'` workaround from `ladybug#883` is not available on this pin and is not proposed here.** Dumping the pinned `liblbug.a` (84 MB, 144,763 strings) shows no such setting. The settings that exist are `enable_compression`, `enable_default_hash_index`, `enable_internal_catalog`, `enable_packed_path_extend`, `enable_plan_optimizer`, `enable_zone_map`, `checkpoint_threshold`, `threads`. That knob was added to `main` after 0.20.1. `CachedPreparedStatement` **is** present in the binary, so the cached-plan path is active on this pin with no way to disable it — part of why taking `#878`'s fix is worth doing now rather than deferring. Note 0.20.2 does **not** fix `ladybug#883` (SIGSEGV in the cached-prepared-statement path); that report is against `main` *after* `#878`. `#883` requires hundreds of parameterized queries in one session and there is no evidence of hitting it, but it is worth tracking as a follow-up if observed.

`.cargo/config.toml` is protected by a built-in Claude Code guard: `Edit`/`Write` on it are refused regardless of permission settings, because the file can set `linker`, `rustflags`, and `[target.*.runner]`. This is expected behavior, not a misconfiguration to work around — the implementer stages the intended content elsewhere and uses `Bash` (`cp`) to place it, confirming first that the staged content is `[env]`-only with no `runner`/`linker`/`rustflags` additions.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Both version pins move together and the workspace builds and tests clean (Priority: P1)

As a maintainer of this repository's dependency footprint, I want the `lbug` crate pin and the native prebuilt-bundle version pin bumped together from `0.20.1` to `0.20.2` in the same commit, with any resulting API differences absorbed, so that the crate pin and the native bundle never skew apart and the workspace compiles and passes its full test surface.

**Why this priority**: without both pins moving together and the workspace compiling, nothing else in this issue can be evaluated. A skewed pin (crate pinned to one version, native bundle downloaded at another) is the exact failure mode that broke the v0.9.0 release, and is called out explicitly as a required constraint.

**Independent Test**: Bump `lbug = "=0.20.2"` in `Cargo.toml` and `LBUG_VERSION = "0.20.2"` in `.cargo/config.toml` in the same commit, run a local build, and run the full CI suite (including the five real-corpus e2e jobs).

**Acceptance Scenarios**:

1. **Given** `lbug = "=0.20.1"` in `Cargo.toml` and `LBUG_VERSION = "0.20.1"` in `.cargo/config.toml`, **When** both are bumped to `0.20.2`, **Then** both changes land in the same commit.
2. **Given** the bumped pins, **When** the workspace is built, **Then** it compiles cleanly, with any lbug API differences between 0.20.1 and 0.20.2 absorbed at call sites.
3. **Given** a clean compile, **When** the full CI suite runs, **Then** it is fully green, including all five real-corpus e2e jobs.

---

### User Story 2 - The stale-row cached-plan-path fix is confirmed included and the project's primary query path keeps passing (Priority: P1)

As a maintainer relying on `query_params`/`exec_params` for essentially all reads and writes in `crates/core/src/db.rs`, I want the fix for `LadybugDB/ladybug#877` (re-executed parameterized queries returning stale rows on the cached-plan fast path) included in the version this repo ships, so that this project's normal execution path is not silently returning stale, plausible-looking wrong answers.

**Why this priority**: this is the primary motivation for the bump. A stale-row bug on the project's most heavily used code path is the highest-severity failure class available — worse than a crash, because it produces no error signal. There is no standalone in-repo repro for the upstream bug, so verification is necessarily by version-inclusion plus continued passage of the existing test surface that already exercises `query_params`/`exec_params` repeatedly within a session.

**Independent Test**: Confirm the pinned lbug version (0.20.2) postdates `#878`'s landing release, and confirm existing tests that re-execute parameterized queries against a cached plan (i.e. the same prepared query run more than once in a session) continue to pass and return fresh results under the new pin.

**Acceptance Scenarios**:

1. **Given** the 0.20.2 pin, **When** the pinned version is checked against upstream's fix history, **Then** the fix landing in `#878` (closing `#877`) is included.
2. **Given** the bumped pin, **When** existing tests that re-execute the same parameterized query multiple times in one session run, **Then** they continue to pass and return correct, non-stale results.

---

### User Story 3 - macOS static OpenSSL linkage is re-verified by hand, not assumed (Priority: P1)

As a maintainer responsible for the release build, I want the macOS arm64 release binary re-checked by hand for static OpenSSL linkage after the bump, so that a change to the native bundle between 0.20.1 and 0.20.2 does not silently reintroduce the dynamic-linkage regression that broke the v0.9.0 release.

**Why this priority**: CI is ubuntu-only and `release.yml` builds nothing until a tag exists, so no automated signal currently exists for this platform-specific risk. The native bundle changes with the version, so the 0.20.1 verification result does not carry over to 0.20.2.

**Independent Test**: In a detached worktree from the feature branch, run `scripts/stage-openssl-static.sh`, build the release binary, run `scripts/assert-static-openssl.sh` against it, and inspect `otool -L` output by hand.

**Acceptance Scenarios**:

1. **Given** a detached worktree built from the feature branch, **When** `eval "$(bash scripts/stage-openssl-static.sh)"` followed by `cargo build --release --bin liminis-context-graph` runs, **Then** the build succeeds.
2. **Given** the release binary, **When** `scripts/assert-static-openssl.sh target/release/liminis-context-graph` runs, **Then** it passes.
3. **Given** the release binary, **When** `otool -L target/release/liminis-context-graph` runs, **Then** it shows only system frameworks — no `libssl`/`libcrypto`, no Homebrew paths.

---

### User Story 4 - A pre-existing storage-v41 database still opens in place and migrates (Priority: P1)

As an operator of a running lcg service with a database created under a 0.17.0-era binary (storage version 41), I want that database to continue to open under the new 0.20.2-based binary, migrate automatically to storage version 47, and serve correct reads, so that this bump — despite being a drop-in version change — does not silently regress the migration path #529 established.

**Why this priority**: data loss, corruption, or a newly introduced manual migration step is the highest-severity failure mode for any lbug version bump, even one where the storage version itself is unchanged (0.20.1 and 0.20.2 both map to `STORAGE_VERSION_47`).

**Independent Test**: Run the existing `storage_v41_migration` test (`crates/core/tests/storage_v41_migration.rs`, added by #529) against the 0.20.2-based binary and confirm it still passes.

**Acceptance Scenarios**:

1. **Given** a pre-existing database at storage version 41, **When** it is opened under the 0.20.2-based binary, **Then** it opens successfully and migrates to storage version 47 without any manual operator step.
2. **Given** the migrated database, **When** reads are issued against it, **Then** results are correct and consistent with the pre-migration data.

---

### Edge Cases

- What happens if lbug's Rust/C API surface has a breaking change between 0.20.1 and 0.20.2 with no mechanical fix? A compile-clean result is necessary but not sufficient acceptance evidence (User Story 1); the e2e suites are the real gate, and a non-mechanical resolution is a Research/Plan-stage decision, not a spec change.
- What happens if the native bundle change between 0.20.1 and 0.20.2 perturbs the static-OpenSSL-link approach established by #398/ADR-0398 and re-verified by #529? User Story 3 requires this be caught by hand on macOS before the bump is considered complete, since CI provides no automated signal for it and the prior version's verification result does not carry over.
- What happens if a storage-v41 database fails to open or migrate cleanly under 0.20.2, contrary to the unchanged-storage-version expectation? This blocks the issue — User Story 4 requires the existing `storage_v41_migration` test to keep passing, not a documented assumption that it will.
- What happens if this project's own test suite (which runs many parameterized queries, though typically not "hundreds in one session" per test) triggers `ladybug#883` (the SIGSEGV in the cached-prepared-statement path, not fixed by 0.20.2)? This is not expected — the issue notes no evidence of hitting it — but if a segfault occurs during CI or local verification for this issue, it MUST be treated as a new discovery to track separately (see Out of Scope), not silently worked around, since no workaround is available on this pin.
- What happens if the CI hang investigated on `chore/release-0.14.0` recurs during this issue's own CI runs? It is unreproduced and this bump is not claimed to fix it (see Background); a recurrence is relevant evidence for the separate hang investigation but does not by itself block this issue, since the Background section explicitly frames the bump's justification independently of the hang.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `Cargo.toml`'s `lbug` pin and `.cargo/config.toml`'s `LBUG_VERSION` pin MUST both move to `0.20.2`, changed in the same commit.
- **FR-002**: `.cargo/config.toml` MUST be edited by staging its intended content and copying it into place with `Bash` (not `Edit`/`Write`, which are refused on this file by design), and the staged content MUST be confirmed `[env]`-only with no `runner`/`linker`/`rustflags` additions before being placed.
- **FR-003**: The fix for `LadybugDB/ladybug#877` (stale rows from re-executed parameterized queries on the cached-plan path), landed in `#878`, MUST be confirmed included in the pinned version, and existing tests that re-execute the same parameterized query multiple times in a session MUST continue to pass under the new pin.
- **FR-004**: A pre-existing database at storage version 41 MUST continue to open under the new binary, migrate in place to storage version 47 automatically, and serve correct reads, with no manual operator step required — verified via the existing `storage_v41_migration` test.
- **FR-005**: The macOS arm64 release build MUST be re-verified by hand (CI is ubuntu-only and `release.yml` builds nothing pre-tag; the native bundle changes with version, so the 0.20.1 result does not carry over): it MUST pass `scripts/assert-static-openssl.sh`, and `otool -L` MUST show only system frameworks — no `libssl`/`libcrypto`, no Homebrew paths.
- **FR-006**: Version-specific comments in `.cargo/config.toml` and `Cargo.toml` that describe 0.20.1-specific facts MUST be reviewed against 0.20.2 and corrected wherever the underlying fact has changed; facts that still hold MUST be left as-is.
- **FR-007**: Any lbug Rust/C API differences between 0.20.1 and 0.20.2 MUST be absorbed so the workspace compiles cleanly; compiling cleanly alone is not sufficient acceptance evidence — full CI, including all five real-corpus e2e jobs, MUST also pass.
- **FR-008**: `CHANGELOG.md`'s existing lbug version-bump entry (added by #529) MUST be updated to say `0.20.2` rather than adding a duplicate new entry, and the 41 -> 47 migration narrative MUST remain accurate and unchanged, since the storage version itself does not change in this bump.
- **FR-009**: No new lbug 0.20.2 capability MAY be adopted as part of this change — this is a version bump for correctness-fix purposes only.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `Cargo.toml` pins `lbug = "=0.20.2"` and `.cargo/config.toml` pins `LBUG_VERSION = "0.20.2"`, both changed in the same commit.
- **SC-002**: Full CI is green, including all five real-corpus e2e jobs.
- **SC-003**: The pinned version is confirmed to include the fixes for `ladybug#877`/`#878`, `#894`, and `#884`, and existing tests exercising repeated parameterized-query execution continue to pass.
- **SC-004**: A pre-existing storage-v41 database opens in place under the new binary, migrates to storage version 47, and serves correct reads with no manual operator step (`storage_v41_migration` test passes).
- **SC-005**: The macOS arm64 release build passes `scripts/assert-static-openssl.sh` and shows only system frameworks under `otool -L` (no `libssl`/`libcrypto`, no Homebrew paths).
- **SC-006**: `CHANGELOG.md`'s lbug entry says `0.20.2`, and the 41 -> 47 migration story remains accurate and unchanged.

## Assumptions

- Storage version is unchanged between 0.20.1 and 0.20.2 (both map to `STORAGE_VERSION_47` per `src/include/storage/storage_version_info.h`), so this is a drop-in bump with no new migration step; User Story 4 / FR-004 is regression confirmation of the existing 41 -> 47 path, not verification of a new migration.
- lbug's public Rust/C API surface used by this repo (`crates/core`, `crates/service`) has no breaking changes between 0.20.1 and 0.20.2 beyond what can be resolved mechanically at call sites; if a resolution is not mechanical, FR-007 still governs the outcome (compile-clean and full CI green), and the resolution approach is a Research/Plan-stage decision, not a spec change.
- The `enable_cached_prepared_statement='NONE'` workaround from `ladybug#883` remains unavailable on this pin (confirmed absent from the pinned `liblbug.a`'s strings); this issue does not attempt to introduce or emulate this setting.
- `LadybugDB/ladybug#883` (SIGSEGV in the cached-prepared-statement path, reported against `main` after `#878`) is not fixed by 0.20.2 and is out of scope for this issue; there is no current evidence this repo's usage pattern triggers it.
- The fix for `#877`/`#878` cannot be independently regression-tested from within this repo with a targeted, standalone repro beyond confirming inclusion and confirming existing tests that re-execute parameterized queries continue to pass — consistent with how #529 verified its upstream-fix inclusions.
- The macOS static-OpenSSL-linkage re-verification (User Story 3) requires a machine capable of building for `aarch64-apple-darwin`; this is a manual, by-hand check as the issue itself specifies, not something to attempt to automate as part of this issue's scope.
- The unreproduced CI hang investigated on `chore/release-0.14.0` is not itself resolved, explained, or claimed to be fixed by this bump; this issue proceeds on the "why not rollback" rationale in Background, independent of whether that hang's root cause is ever identified.

## Out of Scope

- Adopting any new lbug 0.20.2 capability — this issue is a version bump for correctness-fix purposes only; new capabilities get their own issues.
- Fixing, working around, or rolling back to avoid `LadybugDB/ladybug#898` (segfault + deadlock on `DROP`/rebuild with an orphaned index holder) — the issue's own rationale establishes this is present identically across 0.19.0–0.20.2/main and that rolling back does not avoid it; addressing it is a separate, upstream-tracked concern.
- Implementing or emulating the `enable_cached_prepared_statement='NONE'` workaround — confirmed unavailable on this pin.
- Root-causing the unreproduced CI hang investigated on `chore/release-0.14.0` (`Db::open` + `CREATE ART INDEX`, an episode resolve, a WAL rebuild) — this issue proceeds regardless of whether that hang's cause is ever identified.
- Tracking or fixing `LadybugDB/ladybug#883` (SIGSEGV in the cached-prepared-statement path) — noted as a risk to monitor, not fixed by 0.20.2; a discovery during this issue's work becomes a follow-up issue, not an expansion of this one.

## Source References

- `Cargo.toml` (workspace `lbug` pin), `.cargo/config.toml` (`LBUG_VERSION` env pin and its OpenSSL/mbedtls-linkage comments)
- `crates/core/src/db.rs` — the `query_params`/`exec_params` path this bump's primary fix (`#877`/`#878`) targets
- `crates/core/tests/storage_v41_migration.rs` — the storage-v41-in-place-migration test added by #529
- `scripts/stage-openssl-static.sh`, `scripts/assert-static-openssl.sh` — the static-OpenSSL-linkage staging and assertion scripts from #398
- `CHANGELOG.md` — records the lbug version bump and the 41 -> 47 migration story
- ADR-0398 — OpenSSL linkage for release artifacts (the precedent this issue's User Story 3 re-verifies)
- `specs/529-upgrade-lbug-0-19/spec.md` — the 0.19.1 -> 0.20.1 precedent this issue directly follows
- `src/include/storage/storage_version_info.h` (upstream `LadybugDB/ladybug`) — confirms `STORAGE_VERSION_47` is unchanged between 0.20.1 and 0.20.2
- Upstream `LadybugDB/ladybug` issues/PRs `#877`, `#878`, `#894`, `#884`, `#898`, `#883`, `#845`, and releases 0.20.1, 0.20.2
