# Feature Specification: Upgrade lbug 0.18.1 -> 0.20.2 (storage 42 -> 47): stale-row cached-plan fix and the fixes forgone by the rollback

**Feature Branch**: `fabrik/issue-561`
**Created**: 2026-09-06
**Status**: Specified
**Input**: User description: "Upgrade lbug 0.18.1 -> 0.20.2 (storage 42 -> 47): stale-row cached-plan fix and the fixes forgone by the rollback. Supersedes #546, which specified 0.20.1 -> 0.20.2; that starting point no longer exists because 73d8c0cc rolled the pin back to =0.18.1 and v0.14.0 shipped on it. Motivated by: (1) LadybugDB/ladybug#877/#878, shipped in 0.20.2, fixing stale rows returned by re-executed parameterized queries on the cached-plan fast path — the normal path for crates/core/src/db.rs's query_params/exec_params, not an edge case; (2) fixes forgone by the 0.18.1 rollback (#845 FTS heap corruption under concurrent scan/write, #837 primary-key-lookup alignment which #221 depends on, #864 silent row loss in LOAD FROM/UNWIND feeding a MATCH primary-key predicate, #894 planner bugs, #884 an ArrowResultCollector downcast fix); (3) the 0.20.1 deadlock that forced the rollback does not reproduce under 0.20.2, per a retest in the same container shape that wedged deterministically before; (4) the ladybug#883 workaround (enable_cached_prepared_statement) is now present and settable in the 0.20.2 bundle, though #883 itself remains open and unfixed upstream."

## Background

This repository currently ships on lbug 0.18.1 (storage version 42), pinned by `73d8c0cc`, which rolled back an earlier attempt to reach 0.20.1 after that version deadlocked deterministically under concurrent use in this repo's test container shape (`linux/amd64`, `nproc` 28). That rollback was a correct, conservative call, but it left three things outstanding:

1. **A correctness bug on the repo's primary execution path.** `LadybugDB/ladybug#877`, fixed by `#878` and shipped in 0.20.2, is a stale-row bug on the cached-plan fast path for re-executed parameterized queries. `crates/core/src/db.rs` runs essentially every read and write through `query_params`/`exec_params`, and re-execution of the same parameterized query is the normal calling pattern for a long-running service, not a rare edge case. A knowledge graph returning stale-but-plausible rows with no crash and no error is close to the worst failure mode available — it would not be caught by a health check or an error log, only by a query returning something quietly wrong.

2. **Fixes intentionally deferred by the rollback commit.** `73d8c0cc` recorded, as known trade-offs of retreating to 0.18.1, that the repo would forgo `ladybug#845` (FTS heap corruption when scans race committing writers — this repo runs `CREATE_FTS_INDEX` and queries it concurrently as a live service), `#837` (alignment in the primary-key-lookup path that issue #221 depends on), `#864` (silent row loss in `LOAD FROM`/`UNWIND` feeding a `MATCH` primary-key predicate), `#894` (several planner bugs), and `#884` (an `ArrowResultCollector` downcast fix).

3. **An unresolved question about whether 0.18.1 itself already has the stale-row bug.** 0.18.1 has an active cached-plan reuse path (`canReuseCachedPlanWith` -> `useCachedPlan`, `client_context.cpp:377`) and no `enable_cached_prepared_statement` setting to disable it. `canReuseCachedPlanWith` is byte-identical between 0.18.1 and 0.20.2, which means `#878`'s fix landed elsewhere in the execution path — source inspection alone does not establish whether the shipped 0.18.1 binary already exhibits the stale-row bug. **Determining this is explicitly a Research-stage task, not something this spec resolves**: if the answer is yes, this upgrade is a correctness fix against the currently shipping release rather than a forward-looking enhancement, which should be reflected in how the fix is communicated (e.g. release notes framing), though it does not change the upgrade's scope.

A retest in the same container shape that wedged deterministically on 0.20.1 (`linux/amd64`, `nproc` 28, this repo's `lcg-core` suite, prebuilt bundle via `LBUG_PRECOMPILED_SOURCE=release:LadybugDB/ladybug/v0.20.2`) no longer deadlocks: 0.20.1 hung permanently (117 threads in `futex_wait`, never recovering); 0.20.2 completed in 5.65–7.44s across 20/20 iterations, with the full integration suite (50 binaries / 1,186 tests) showing zero lbug-attributable failures. This has been reported upstream as `LadybugDB/ladybug#911`. That evidence is necessarily limited — the host is Apple Silicon, so the test ran under emulation (equally true of the original 0.20.1 repro, so the two are comparable to each other), and a 20-second suite says nothing about macOS, about a release build, or about long-session behavior, since `ladybug#883` (a separate, still-open SIGSEGV in the cached-prepared-statement path) needs hundreds of parameterized queries in one session to surface. Those gaps are exactly what this issue's acceptance criteria are designed to close before the upgrade is considered safe to ship.

Separately, 0.20.2 exposes `enable_cached_prepared_statement` (`src/include/main/settings.h:170`), which 0.20.1 lacked. This gives operators a way to disable the cached-plan path entirely if `ladybug#883` is ever hit in production. This issue confirms the setting exists and is wired up for use if needed; it does not enable it by default (see Out of Scope).

**Sequencing**: #559 (bundling the lbug vector/fts extensions so `Db::open` never contacts the CDN) is a prerequisite and has already merged to `main`; this issue's branch is based on top of it. Changing the lbug pin before that extension-download removal would have confounded the deadlock investigation, so the ordering matters even though the two changes are otherwise independent.

**Storage migration**: unlike the superseded #546 (0.20.1 -> 0.20.2, storage-neutral), this upgrade advances the on-disk storage format from version 42 (what 0.18.1 / the shipped v0.14.0 writes) to version 47. This is a one-way migration, structurally the same shape as the 41 -> 47 migration #529 already covers for 0.17.0-era databases, but for a *different* source version that has no existing test coverage.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Both version pins move together and the workspace builds and tests clean (Priority: P1)

As a maintainer of this repository's dependency footprint, I want the `lbug` crate pin (`Cargo.toml`) and the native prebuilt-bundle version pin (`.cargo/config.toml`'s `LBUG_VERSION`) bumped together from `0.18.1` to `0.20.2` in the same commit, with any resulting API differences absorbed, so that the crate pin and the native bundle never skew apart and the workspace compiles and passes its full test surface.

**Why this priority**: nothing else in this issue can be evaluated until both pins move together and the workspace compiles. A skewed pin (crate at one version, native bundle at another) is the exact failure mode that broke the v0.9.0 release.

**Independent Test**: Bump `lbug = "=0.20.2"` in `Cargo.toml` and `LBUG_VERSION = "0.20.2"` in `.cargo/config.toml` in the same commit, run a local build, and run the full CI suite including all five real-corpus e2e jobs.

**Acceptance Scenarios**:

1. **Given** `lbug = "=0.18.1"` in `Cargo.toml` and `LBUG_VERSION = "0.18.1"` in `.cargo/config.toml`, **When** both are bumped to `0.20.2`, **Then** both changes land in the same commit.
2. **Given** `.cargo/config.toml` is protected against direct `Edit`/`Write` by a built-in guard (it can set `linker`, `rustflags`, `[target.*.runner]`), **When** its `LBUG_VERSION` pin is changed, **Then** the intended content is staged elsewhere, confirmed `[env]`-only with no `runner`/`linker`/`rustflags` additions, and copied into place with `Bash`.
3. **Given** the bumped pins, **When** the workspace is built, **Then** it compiles cleanly, with any lbug Rust/C API differences between 0.18.1 and 0.20.2 absorbed at call sites.
4. **Given** a clean compile, **When** the full CI suite runs, **Then** it is fully green, including all five real-corpus e2e jobs — a clean compile alone is not sufficient acceptance evidence.

---

### User Story 2 - Re-executed parameterized queries never return stale rows (Priority: P1)

As a maintainer relying on this service's correctness, I want tests that re-execute the same parameterized query multiple times within one session — the normal calling pattern for `query_params`/`exec_params` in `crates/core/src/db.rs` — to pass under 0.20.2 and demonstrate fresh, non-stale results after an intervening write, so that the cached-plan fast path (`ladybug#877`, fixed by `#878`) cannot silently return outdated data to any caller of this service.

**Why this priority**: this is the headline motivation for the upgrade — a silent-wrong-answer bug on a path this repo's every read and write already traverses. It is the correctness property the whole issue exists to establish, not a secondary check.

**Independent Test**: Construct a test that executes a parameterized query, performs a write that changes the result set, re-executes the identical parameterized query in the same session, and asserts the second execution reflects the write.

**Acceptance Scenarios**:

1. **Given** a session with an active cached plan for a parameterized query, **When** a write changes the data that query would return, **Then** re-executing the same parameterized query in that session returns the post-write, non-stale result.
2. **Given** the existing test suite's queries that already re-execute parameterized statements within a session, **When** they run under 0.20.2, **Then** they continue to pass with no stale-row regressions.

---

### User Story 3 - A pre-existing storage-v42 database migrates in place with no manual operator step (Priority: P1)

As an operator of a running lcg service with a database created under the shipped v0.14.0 binary (storage version 42, lbug 0.18.1), I want that database to open under the new 0.20.2-based binary, migrate automatically to storage version 47, and serve correct reads, so that upgrading past v0.14.0 does not require a manual recovery step and does not silently corrupt or lose data.

**Why this priority**: data loss, corruption, or a forced manual migration step is the highest-severity failure mode for a storage-format-advancing dependency bump, and this specific source version (42) has no existing regression coverage — unlike storage version 41, which #529 already covers.

**Independent Test**: Obtain or generate a database at storage version 42 (e.g. by creating one under the currently-pinned 0.18.1 binary before the pins are bumped), open it under the new 0.20.2-based binary, and confirm it migrates and serves correct reads. The existing `storage_v41_migration` test (from #529) continues to pass unmodified, demonstrating the older migration path is not broken by this change.

**Acceptance Scenarios**:

1. **Given** a pre-existing database at storage version 42, **When** it is opened under the 0.20.2-based binary, **Then** it opens successfully and migrates to storage version 47 without any manual operator step.
2. **Given** the migrated storage-v42 database, **When** reads are issued against it, **Then** results are correct and consistent with the pre-migration data.
3. **Given** the existing storage-v41 fixture and its test, **When** the suite runs under the bumped pins, **Then** `storage_v41_migration` continues to pass unmodified.

---

### User Story 4 - macOS arm64 release build and OpenSSL linkage verified by hand (Priority: P1)

As a maintainer responsible for the release build, I want the macOS arm64 release binary re-checked by hand against **ADR-0550**'s dynamic-OpenSSL-via-rpath linkage model after the bump, so that a change to the native bundle's OpenSSL handling does not silently ship a binary that only loads on one specific machine's package-manager layout.

**Why this priority**: CI is ubuntu-only, so no automated signal exists for this platform-specific risk, and the native bundle changes with every lbug version. The applicable model has changed since the last time this was checked by hand (#529 verified static linkage under ADR-0398; v0.14.0 already moved to dynamic linkage via Homebrew, per ADR-0550) — the old "otool -L shows only system frameworks" criterion is obsolete for this bump and must not be reapplied.

**Independent Test**: In a worktree built from the feature branch, run `scripts/stage-openssl-rpath.sh`, build the release binary, and run `scripts/assert-openssl-linkage.sh` against it.

**Acceptance Scenarios**:

1. **Given** a worktree built from the feature branch, **When** `scripts/stage-openssl-rpath.sh` runs followed by `cargo build --release --bin liminis-context-graph`, **Then** the build succeeds.
2. **Given** the release binary, **When** `scripts/assert-openssl-linkage.sh` runs against it, **Then** it passes — accepting an `@rpath`-relative reference or a stable package-manager prefix, and rejecting any absolute path naming one specific machine's OpenSSL install.
3. **Given** the release binary, **When** it is started against a temporary workspace configured with an OpenAI-compatible embedder, **Then** `knowledge_status` succeeds and a write/read round trip completes correctly.

---

### User Story 5 - Long-session behavior is exercised past the point a short suite reaches (Priority: P2)

As a maintainer aware that `ladybug#883` (a SIGSEGV in the cached-prepared-statement path) remains open and unfixed upstream, I want a test or manual run that issues a substantially larger number of parameterized queries in one session than the existing suite does, so that this upgrade has direct evidence about long-session stability rather than relying solely on the short suite's clean pass.

**Why this priority**: `#883` is explicitly not fixed by 0.20.2 per the issue's own framing; this story is about characterizing the risk this upgrade knowingly still carries, not about achieving a fix. It is P2 relative to User Stories 1–4, which are prerequisites for shipping at all.

**Independent Test**: Run (or write, if none exists) a test or script that issues at least 1,000 parameterized query executions against one open `Database`/session, and confirm it completes without a crash, hang, or stale result.

**Acceptance Scenarios**:

1. **Given** a single open session, **When** at least 1,000 parameterized queries are executed against it (an order of magnitude beyond the ~20-iteration suite the deadlock retest used), **Then** the run completes without crashing, hanging, or returning a stale result.
2. **Given** that `ladybug#883` remains open upstream, **When** this story's run passes, **Then** the PR description states plainly that long-session risk is reduced, not eliminated, and that `enable_cached_prepared_statement` (see Background) is the documented mitigation if `#883` is hit in production.

---

### User Story 6 - CHANGELOG and release notes document the migration (Priority: P2)

As a downstream integrator or operator planning an upgrade, I want `CHANGELOG.md`'s `[Unreleased]` section and `docs/releases/0.14.0.md` to describe the 42 -> 47 storage migration and its rollback procedure, so that I know what to expect before upgrading and how to recover if something goes wrong.

**Why this priority**: this is documentation of already-completed work, sequenced after the technical stories that establish what actually happened, but it is required acceptance evidence per the issue, not optional cleanup.

**Independent Test**: Read `CHANGELOG.md`'s `[Unreleased]` section and `docs/releases/0.14.0.md` after the change and confirm both describe the 42 -> 47 migration, the lbug version bump, and the rollback procedure.

**Acceptance Scenarios**:

1. **Given** the completed upgrade, **When** `CHANGELOG.md` is inspected, **Then** its `[Unreleased]` section records the lbug 0.18.1 -> 0.20.2 bump and the 42 -> 47 storage migration in one entry.
2. **Given** the completed upgrade, **When** `docs/releases/0.14.0.md` is inspected, **Then** it is updated consistently with the CHANGELOG entry, including the rollback procedure (stop the service, move `.lcg/db/` aside, start the old binary, it rebuilds from the WAL).

---

### Edge Cases

- What happens if research determines the shipped 0.18.1 already exhibits the stale-row bug from `ladybug#877`? Per Background, this reframes the upgrade's urgency (correctness fix vs. enhancement) but does not change its scope or acceptance criteria; the finding should be reflected in the PR description and release notes framing.
- What happens if the roughly-two-minor-versions of upstream changes between 0.18.1 and 0.20.2 include a breaking Rust/C API change with no mechanical fix? A clean compile is necessary but not sufficient acceptance evidence (User Story 1); the full CI suite, including e2e jobs, is the real gate, and a non-mechanical resolution is a Research/Plan-stage decision, not a spec change.
- What happens if a storage-v42 fixture is not readily available? Per Assumptions, one can be generated by creating a database under the currently-pinned 0.18.1 binary before the pins move; this is a Research-stage detail, not a spec blocker.
- What happens if a storage-v42 database fails to open or migrate cleanly under 0.20.2? This blocks the issue — User Story 3 requires an explicit, automatic, no-manual-step migration, not a documented workaround.
- What happens if `scripts/assert-openssl-linkage.sh` fails against the freshly-built macOS release binary? This blocks the issue; User Story 4 requires it to pass by hand, since CI provides no automated signal for this platform.
- What happens if the long-session run (User Story 5) surfaces `ladybug#883` (the SIGSEGV it is known not to fix)? This is treated as a discovered risk to document, not a blocker invalidating the rest of the upgrade — `enable_cached_prepared_statement` exists specifically as the documented mitigation; a hit here would justify a follow-up issue to evaluate enabling it, per Out of Scope.
- What happens if the 0.20.1 deadlock (the reason for the original rollback) reappears under 0.20.2 despite the retest evidence in Background? This blocks the issue; the retest evidence is preliminary confirmation, not a substitute for the full CI suite (User Story 1) and long-session run (User Story 5) passing on this branch.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `Cargo.toml`'s `lbug` pin and `.cargo/config.toml`'s `LBUG_VERSION` pin MUST both move from `0.18.1` to `0.20.2`, changed in the same commit.
- **FR-002**: `.cargo/config.toml` MUST be edited by staging its intended content and copying it into place with `Bash` (not `Edit`/`Write`, which are refused on this file by design), and the staged content MUST be confirmed `[env]`-only with no `runner`/`linker`/`rustflags` additions before being placed.
- **FR-003**: A pre-existing database at storage version 42 MUST open under the new binary, migrate in place (42 -> 47) automatically, and serve correct reads, with no manual operator step required. Test coverage for this MUST be added (there is none today), analogous in structure to the existing `storage_v41_migration` test.
- **FR-004**: The existing `storage_v41_migration` test (from #529) MUST continue to pass unmodified, confirming the 41 -> 47 in-place open path is not broken by this change.
- **FR-005**: Tests that re-execute the same parameterized query multiple times within one session, with an intervening write that changes the result, MUST demonstrate the second execution returns fresh, non-stale results — exercising the `ladybug#877`/`#878` fix directly, not merely relying on the full suite passing incidentally.
- **FR-006**: A test or documented manual run MUST exercise at least 1,000 parameterized query executions in a single session and confirm no crash, hang, or stale result — evidence for long-session behavior beyond what the existing short suite reaches, given `ladybug#883` remains open upstream.
- **FR-007**: The macOS arm64 release build MUST be verified by hand against **ADR-0550** (not the superseded ADR-0398): `scripts/stage-openssl-rpath.sh` MUST succeed, and `scripts/assert-openssl-linkage.sh` MUST pass against the resulting release binary.
- **FR-008**: The macOS release binary MUST be exercised with a live write/read round trip against a temporary workspace configured with an OpenAI-compatible embedder, confirming `knowledge_status` succeeds and both the write and the read complete correctly.
- **FR-009**: Version-specific comments in `Cargo.toml` and `.cargo/config.toml` that describe 0.18.1-specific facts (e.g. OpenSSL linkage behavior, pkg-config discovery notes) MUST be reviewed against 0.20.2 and corrected wherever the underlying fact has changed; facts that still hold MUST be left as-is.
- **FR-010**: Any lbug Rust/C API differences between 0.18.1 and 0.20.2 MUST be absorbed so the workspace compiles cleanly; compiling cleanly alone is not sufficient acceptance evidence — full CI, including all five real-corpus e2e jobs, MUST also pass.
- **FR-011**: `CHANGELOG.md`'s `[Unreleased]` section MUST record the lbug 0.18.1 -> 0.20.2 bump and the single 42 -> 47 storage migration, including the rollback procedure (stop the service, move `.lcg/db/` aside, start the old binary, it rebuilds from the WAL).
- **FR-012**: `docs/releases/0.14.0.md` MUST be updated consistently with the `CHANGELOG.md` entry from FR-011, since v0.14.0 has not yet shipped and this upgrade lands within it.
- **FR-013**: The PR description or release notes MUST state whether Research determined the shipped 0.18.1 already exhibits the `ladybug#877` stale-row bug, per the open question recorded in Background.
- **FR-014**: No new lbug 0.20 capability (partitioning, CSR projection, GQL extension, `AI EXTRACT`) MAY be adopted as part of this change, and `enable_cached_prepared_statement` MUST NOT be enabled by default — this is a version bump for correctness-fix and migration-consolidation purposes only (see Out of Scope).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `Cargo.toml` pins `lbug = "=0.20.2"` and `.cargo/config.toml` pins `LBUG_VERSION = "0.20.2"`, both changed in the same commit.
- **SC-002**: Full CI is green, including all five real-corpus e2e jobs.
- **SC-003**: A pre-existing storage-v42 database opens in place under the new binary, migrates to storage version 47, and serves correct reads with no manual operator step; the pre-existing storage-v41 test continues to pass unmodified.
- **SC-004**: A test demonstrates that re-executing the same parameterized query after an intervening write returns fresh, non-stale results.
- **SC-005**: A test or documented manual run of at least 1,000 parameterized queries in one session completes without a crash, hang, or stale result.
- **SC-006**: The macOS arm64 release build passes `scripts/assert-openssl-linkage.sh` (per ADR-0550) and serves a live write/read round trip.
- **SC-007**: `CHANGELOG.md` and `docs/releases/0.14.0.md` both describe the lbug 0.18.1 -> 0.20.2 bump, the 42 -> 47 migration, and the rollback procedure.

## Assumptions

- A storage-v42 database fixture either already exists in the repo's test fixtures, or can be generated by creating a database under the currently-pinned 0.18.1 binary before the pins are bumped. If neither holds, that is a Research-stage finding, not a spec blocker.
- The WAL format itself (`.lcg/wal/`, project-owned JSONL) is unaffected by lbug's storage-version advance, consistent with #398 and #529 — this issue does not need to independently re-verify WAL replay compatibility.
- Whether the currently-shipped 0.18.1 already exhibits the `ladybug#877` stale-row bug is a Research-stage determination (per Background); this spec's acceptance criteria (FR-005, SC-004) hold regardless of that finding, since the fix must be demonstrated under the new pin either way.
- The fixes forgone by the rollback (`ladybug#845`, `#837`, `#864`, `#894`, `#884`) cannot be independently regression-tested from within this repo without a standalone upstream repro beyond what FR-003–FR-006 already exercise (storage migration, cached-plan correctness, and long-session stability); confirming the pinned version postdates each fix and that existing FTS/PK-lookup tests continue to pass is sufficient evidence for their inclusion.
- The macOS OpenSSL-linkage re-verification (User Story 4) requires a machine capable of building for `aarch64-apple-darwin`; this is a manual, by-hand check as the issue itself specifies, not something to attempt to automate as part of this issue's scope.
- lbug's public Rust/C API surface used by this repo (`crates/core`, `crates/service`) has no breaking changes between 0.18.1 and 0.20.2 beyond what can be resolved mechanically at call sites; if a resolution is not mechanical, FR-010 still governs the outcome (compile-clean and full CI green), and the resolution approach is a Research/Plan-stage decision, not a spec change.
- 1,000 parameterized query executions (FR-006) is a concrete, testable floor derived from the issue's own qualitative framing ("hundreds of parameterized queries in one session"); Research/Plan MAY increase this figure if investigation into `ladybug#883`'s upstream repro suggests a higher threshold is needed to be meaningful evidence, but MUST NOT reduce it below this floor without recording why in the Plan stage.

## Out of Scope

- Enabling `enable_cached_prepared_statement` by default. This issue confirms the setting exists and is available as a documented mitigation lever; actually flipping it (which would disable the cached-plan path's performance benefit entirely) is a separate decision to be made only if `ladybug#883` is actually hit, and would warrant its own follow-up issue.
- Adopting any new lbug 0.20 capability: partitioning (RANGE/HASH/LIST), CSR projection, the GQL extension, or `AI EXTRACT`. This issue is a version bump for correctness-fix and migration-consolidation purposes only; new capabilities get their own issues.
- Fixing `ladybug#883` (the open, unfixed SIGSEGV in the cached-prepared-statement path) itself — it is not fixed by 0.20.2. This issue characterizes and documents the residual risk (User Story 5); a fix must come from upstream or a separate mitigation issue.
- Root-causing or fixing anything beyond what upstream's 0.19.x/0.20.x releases already address; if the macOS linkage re-verification (User Story 4) surfaces a new, unrelated problem, that is a follow-up issue, not an expansion of this one.
- Re-litigating the #559 extension-bundling change or its sequencing ahead of this issue; #559 has already merged to `main` and this branch is based on top of it.

## Source References

- `Cargo.toml` (workspace `lbug` pin), `.cargo/config.toml` (`LBUG_VERSION` env pin and its OpenSSL/pkg-config-linkage comments)
- `crates/core/src/db.rs` — `query_params`/`exec_params`, the cached-plan fast path this issue's core motivation concerns
- `crates/core/tests/storage_v41_migration.rs` and `crates/core/tests/fixtures/storage_v41_db/` — the existing migration-test pattern this issue's new storage-v42 coverage should follow
- `scripts/stage-openssl-rpath.sh`, `scripts/assert-openssl-linkage.sh` — the dynamic-OpenSSL-via-rpath staging and assertion scripts from ADR-0550
- ADR-0550 — OpenSSL dynamic linkage via rpath (supersedes ADR-0398, which #529 verified against and which no longer applies)
- `CHANGELOG.md`, `docs/releases/0.14.0.md` — where the 42 -> 47 migration and rollback procedure must be documented
- `73d8c0cc` — the rollback to 0.18.1, recording the deadlock diagnosis and the list of forgone fixes this issue re-attempts to pick up
- Issue #546 (closed, superseded) — the original 0.20.1 -> 0.20.2 spec, at `specs/546-upgrade-lbug-0-20/` (removed on close; see git history)
- Issue #555 — the 0.20.1 deadlock investigation
- Issue #529 and `specs/529-upgrade-lbug-0-19/spec.md` — the 0.19.1 -> 0.20.1 upgrade and the storage-v41 migration-test precedent
- Issue #221 — blocked on the primary-key-lookup path that `ladybug#837`/`#864` fix
- Issue #559 — the prerequisite bundled-extensions change this branch is based on top of
- Upstream `LadybugDB/ladybug` issues/PRs `#877`, `#878`, `#845`, `#837`, `#864`, `#894`, `#884`, `#883`, `#911` (this repo's upstream deadlock report), and releases 0.19.1 through 0.20.2
- `src/include/main/settings.h:170` (upstream `LadybugDB/ladybug`) — `enable_cached_prepared_statement`
