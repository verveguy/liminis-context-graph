# Feature Specification: Upgrade lbug dependency from 0.17.0 to 0.19.1

**Feature Branch**: `fabrik/issue-398`
**Created**: 2026-08-14
**Status**: Specified
**Input**: User description: "Upgrade the graph engine dependency from `lbug 0.17.0` (2026-05-28) to `0.19.1` (2026-08-04). This is deliberately not a one-line dependency bump: 0.18.0 changed the native prebuilt bundle's macOS TLS backend from self-contained bundled mbedtls to OpenSSL, which is exactly what broke the first v0.9.0 release build. The upgrade must re-solve external SSL linkage across the release platforms while preserving the IPC contract, WAL compatibility, and the prebuilt-bundle build path. Supersedes #190 (targeted 0.18.1, closed unimplemented — five upstream releases shipped since, its branch predates the multi-stream rework) and #220 (closed as a duplicate of #190)."

## Background

`lbug` is pinned in two places that must move together: `lbug = "=0.17.0"` in the workspace `Cargo.toml`, and `LBUG_VERSION = "0.17.0"` in `.cargo/config.toml [env]` (which pins the native prebuilt bundle to match the crate). This issue moves both to `0.19.1`.

**Why 0.19.1 rather than a smaller bump to 0.18.x.** The hard part — re-solving external SSL linkage — is identical at either target: it arrived in 0.18.0 (upstream `LadybugDB/ladybug#590` "Link against OpenSSL3", plus `#579`/`#581` on precompiled-binary OpenSSL dependencies) and is equally required whether the target is 0.18.3 or 0.19.1. Choosing 0.19.1 adds the 0.19 minor-bump surface but no additional linkage work, and 0.19.0 carries fixes this repo has independent reason to want:

| version | released | |
|---|---|---|
| **0.19.1** | 2026-08-04 | target |
| 0.19.0 | 2026-07-30 | |
| 0.18.3 | 2026-07-21 | no upstream release notes |
| 0.18.2 | 2026-07-15 | no upstream release notes |
| 0.18.1 | 2026-07-10 | #190's original target |
| 0.18.0 | 2026-06-30 | OpenSSL 3 linkage change lands here |
| 0.17.1 | 2026-06-02 | never picked up |
| 0.17.0 | 2026-05-28 | current pin |

Note 0.19.1 is young relative to 0.18.1: roughly 975 downloads at time of writing against 0.18.1's roughly 4,141.

**Motivation 1 — suspected cause of CI hangs (new since #190 was filed).** CI on this repo hung three times on 2026-08-13, each at `cargo test --release`, for 2–5 hours with no failure output and no test-result line, while the same suite completed locally in roughly 5 minutes in both debug and release profiles, and was never reproduced off CI. Two upstream fix clusters match that shape:

- **0.18.1** — "Fix buffer manager causing huge core dumps and hung processes on SIGSEGV" (`LadybugDB/ladybug#665`). A test binary faulting during teardown and then hanging rather than dying matches the observed behavior exactly: the step sits `in_progress` for hours with no output and no crash report. The suite opens a temp database per test, so a shutdown fault has many chances per run, more under CI concurrency — consistent with per-target runs passing while full runs wedged.
- **0.19.0** — "cleanup checkpoint intent/apply lock files after checkpoint" (`#687`), "stop creating checkpoint lock files in StorageManager constructor" (`#689`), and "Fix read-only open during checkpoint from reading inconsistent state" (`#615`). Stale lock files are a classic hang source, and `#615` bears directly on ADR-0002, under which lcg readers deliberately do not take the write lock. **The lock-file fixes exist only in 0.19.0+** — this is the strongest single argument for 0.19.1 over a smaller bump.

This is a hypothesis to be tested, not a diagnosis to be assumed — see User Story 4 and Success Criteria below.

**Motivation 2 — secondary ART indexes (carried from #220).** `Support secondary ART indexes` (`LadybugDB/ladybug#582`, 0.18.0). 0.17.0 has only a per-node-table primary-key index plus the FTS and vector extensions: `CREATE INDEX` on a table that already has a PK produces a catalog entry with no physical structure, and the optimizer's index-scan rewrite is PK-only. This is why `get_entity_by_name_ci` is backed by an in-process `HashMap` today, and it is what #221 is blocked on. **Using** this capability is a follow-up, out of scope here — see Out of Scope.

**Motivation 3 — cost-based planning (carried from #220).** `Add stats-aware query optimization` (`#577`) and `Optimize planner stats and functional recursive joins` (`#574`), both 0.18.0. As with secondary indexes, changing lcg's query plans to exploit this is out of scope here.

**Motivation 4 — other changes of direct relevance.** `Batch detached node relationship deletes` (`#569`, 0.18.0) — `knowledge_delete_by_group` (#361) is `DETACH DELETE`-heavy. `Scalar Quantization in HNSW` (`#673`, 0.19.0) — lcg uses HNSW for vector indexes. `Implement WAL group commits` (`#547`, 0.18.0). `fix(capi): preserve error message on unsuccessful QueryResult` (`#762`, 0.19.1).

**Notes carried from the issue:**
- 0.18.2 and 0.18.3 publish no upstream release notes (empty release bodies). If anything in this upgrade turns on what changed there, it needs the commit log rather than the releases page.
- 0.17.1 (2026-06-02) was never picked up. It was considered as a cheap experiment to test the hang hypothesis without touching the TLS linkage problem, but this issue commits directly to the full 0.19.1 upgrade instead — see Out of Scope.
- #190's branch (`fabrik/issue-190`) carries an implementation through Review against 0.18.1, on a branch that is three weeks stale and predates the multi-stream rework — not a useful starting point. Its linkage analysis is reproduced above.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Dependency pin moves cleanly across the workspace and every release platform (Priority: P1)

As a maintainer of this repository's dependency footprint, I want the `lbug` crate pin and the native prebuilt-bundle version pin bumped together from `0.17.0` to `0.19.1`, and the resulting external SSL linkage (introduced in 0.18.0) resolved on every release target, so that the workspace builds and the release workflow produces working binaries on macOS, x86_64 Linux, and arm64 Linux.

**Why this priority**: without a build that links on every release platform, nothing else in this issue can be evaluated — this is the mechanical prerequisite everything else depends on. The v0.9.0 release regression referenced in this issue's own history (macOS `ld: symbol(s) not found` for OpenSSL symbols, caused by an unpinned floating-`latest` bundle skew) is the concrete failure mode to avoid repeating.

**Independent Test**: Bump both pins, run a local build, then run the release workflow (or a dry run of it) for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu`, and confirm all three link successfully with no unresolved OpenSSL symbols.

**Acceptance Scenarios**:

1. **Given** `lbug = "=0.17.0"` in `Cargo.toml` and `LBUG_VERSION = "0.17.0"` in `.cargo/config.toml`, **When** both are bumped to `0.19.1`, **Then** `cargo build` succeeds locally without needing `LBUG_BUILD_FROM_SOURCE` (documented as broken and must remain unused).
2. **Given** the bumped pins, **When** the release workflow builds for `aarch64-apple-darwin`, **Then** the build links successfully with no missing-OpenSSL-symbol linker errors.
3. **Given** the bumped pins, **When** the release workflow builds for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, **Then** both link successfully.
4. **Given** a successful build, **When** the prebuilt-bundle no-recompile assertion (#341/FR-006) runs, **Then** it still passes — the upgrade does not reintroduce workspace recompilation of lbug.

---

### User Story 2 - IPC/MCP contract and existing behavior are unaffected (Priority: P1)

As a consumer of the `liminis-context-graph` service over its Unix-socket IPC (the liminis Electron app) and over MCP-stdio, I want the upgrade to change no tool schema, response shape, or dispatch method, so that upgrading the graph engine underneath is invisible to every existing integration.

**Why this priority**: the IPC/MCP surface is a hard contract with an external consumer (the Electron app) and with MCP clients; breaking it silently is a correctness failure with far greater blast radius than the dependency bump itself.

**Independent Test**: Run the Tier 1a/1b/1c parity tests in `crates/core/tests/ipc_parity.rs` and the MCP tool-registry tests in `crates/service/src/mcp/tools.rs` before and after the bump and confirm no diff in behavior, counts, or schema.

**Acceptance Scenarios**:

1. **Given** the IPC parity test suite passing under 0.17.0, **When** the same suite runs under 0.19.1, **Then** it passes unchanged.
2. **Given** any API breakage between 0.17.0 and 0.19.1 in lbug's Rust or C API, **When** it is resolved, **Then** no `knowledge_*` tool schema, response shape, or dispatch method changes as an observable side effect.

---

### User Story 3 - Existing WAL and database data survive the upgrade (Priority: P1)

As an operator of a running lcg service with data already persisted under 0.17.0, I want a clear, explicit answer to whether my existing database opens directly under 0.19.1 or requires a documented, automated migration, and I want a WAL written under 0.17.0 to replay to identical graph state under 0.19.1, so that upgrading does not silently corrupt or lose data.

**Why this priority**: data loss or silent corruption is the highest-severity failure mode for a persistence-layer dependency bump; the issue is explicit that this decision "must be explicit, not discovered."

**Independent Test**: Take a WAL written under 0.17.0, replay it under 0.19.1 via `knowledge_rebuild_from_wal`, and diff the resulting graph state against the same replay under 0.17.0.

**Acceptance Scenarios**:

1. **Given** a WAL file written and checkpointed under 0.17.0, **When** it is replayed under 0.19.1, **Then** the resulting graph state is byte-identical to replaying it under 0.17.0.
2. **Given** a database created under 0.17.0, **When** it is opened under 0.19.1, **Then** it opens successfully, or a documented and automated migration path is provided and exercised.

---

### User Story 4 - Verify, rather than assume, whether the upgrade addresses the CI hangs (Priority: P2)

As a maintainer who has observed three unexplained multi-hour CI hangs in `cargo test --release` with no failure output, I want to test — after the upgrade — whether the upstream buffer-manager/SIGSEGV and checkpoint-lock-file fixes (0.18.1, 0.19.0) actually resolve the hangs, so that the hypothesis is confirmed or ruled out rather than assumed.

**Why this priority**: this is the strongest motivating reason for choosing 0.19.1 over a smaller bump, but the issue itself frames it as a hypothesis to test, not a guaranteed outcome. It is P2 because the P1 stories (linkage, IPC parity, WAL compatibility) stand on their own merit regardless of whether this resolves the hangs.

**Independent Test**: Run `cargo test --release` in CI multiple times post-upgrade and observe completion time.

**Acceptance Scenarios**:

1. **Given** the upgraded dependency, **When** `cargo test --release` runs in CI several times, **Then** each run completes in a time consistent with the roughly 5-minute local baseline rather than hanging for hours.
2. **Given** hangs persist after the upgrade, **When** investigating further, **Then** the Linux-vs-macOS and #378 case-sensitivity leads noted in this issue are the next things to check, not a reason to expand this issue's scope.

---

### Edge Cases

- What happens if 0.18.2/0.18.3 (no upstream release notes) contain an undocumented breaking change between 0.18.1 and 0.19.0? Only the commit history, not the releases page, would surface it — this needs Research-stage investigation rather than an assumption that nothing changed.
- What happens if the lbug Rust or C API has breaking signature changes that lcg's code depends on? FR-007 requires this be resolved without changing IPC-observable behavior; Research/Plan decide the mechanism.
- What happens if a 0.17.0-created database does not open directly under 0.19.1 because the on-disk format changed? FR-005 requires an explicit, documented, automated migration rather than silent failure or ad hoc manual recovery.
- What happens if `cargo test --release` still hangs after the upgrade? Per User Story 4 and SC-001, this is investigated (Linux-vs-macOS, #378) rather than treated as blocking this issue — the other requirements (linkage, IPC parity, WAL compatibility) are independently valuable and not contingent on the hangs being fixed.
- What happens if resolving OpenSSL 3 linkage requires a new system dependency (e.g. `libssl-dev`) on a release runner that doesn't currently have it? This falls under FR-002 and must be resolved in the release workflow's environment, not worked around by avoiding the OpenSSL-linked path.
- What happens to existing code comments and docs describing 0.17.0-specific facts (e.g. the fat-bundle model, the GCC/`<format>`-header requirement, mbedtls-vs-OpenSSL) in `Cargo.toml`, `.cargo/config.toml`, and `CLAUDE.md`? Each needs to be checked against 0.19.1 reality and corrected where it has gone stale — see FR-008.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Both pins MUST move together to `0.19.1` — `lbug = "=0.19.1"` in the workspace `Cargo.toml` and `LBUG_VERSION = "0.19.1"` in `.cargo/config.toml [env]`.
- **FR-002**: External SSL linkage MUST be re-solved across every release platform. 0.18.0 replaced the bundled mbedtls TLS backend with OpenSSL on macOS; the release build must link successfully on all platforms the release workflow targets, not only on a developer machine.
- **FR-003**: The IPC/MCP contract MUST be unchanged. No tool schema, response shape, or dispatch method may change as a result of this upgrade.
- **FR-004**: WAL compatibility MUST be preserved — a WAL written under 0.17.0 must replay correctly under 0.19.1, and `knowledge_rebuild_from_wal` must reproduce identical graph state.
- **FR-005**: An existing 0.17.0-created database MUST open under 0.19.1, or the required migration MUST be documented and automated. This decision must be explicit, not discovered.
- **FR-006**: The prebuilt-bundle build path MUST keep working, including the CI job that downloads the release artifact and asserts no workspace recompilation (#341/FR-006).
- **FR-007**: Any API breakage between 0.17.0 and 0.19.1 MUST be resolved without changing lcg behaviour observable through the IPC surface.
- **FR-008**: Code comments and documentation that assert version-specific facts about lbug 0.17.0 (e.g. the fat-bundle/no-source-build model in `.cargo/config.toml`, the GCC/`<format>`-header runner pin in `Cargo.toml`, the mbedtls-vs-OpenSSL bundling notes in `CLAUDE.md`) MUST be reviewed against 0.19.1 and updated wherever the underlying fact has changed; facts that still hold MUST be left as-is rather than rewritten without cause.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `cargo test --release` passes in CI, and its observed completion time across several post-upgrade runs is recorded. **The gate is that the suite passes and the outcome is reported, not that the hangs are gone** — whether completion times land near the roughly 5-minute local baseline is the result of User Story 4's hypothesis test, and either answer satisfies this criterion as long as it is stated explicitly. If hangs persist, that is a recorded negative result and a follow-up lead (Linux-vs-macOS, #378), not a failure of this issue; see Edge Cases and Out of Scope, which this wording is written to agree with.
- **SC-002**: The release workflow builds and links on every target platform.
- **SC-003**: A WAL written under 0.17.0 replays under 0.19.1 to byte-identical graph state.
- **SC-004**: `~/dev/liminis-project/multistream_test.py` still passes 9/10 (the tenth being #392) — covering per-group streams, cross-group pointers, purge, checkpoint restore and rebind.
- **SC-005**: No change to any `knowledge_*` tool schema or response shape.

## Assumptions

- The lbug crate's public Rust/C API surface used by this repo (`crates/core`, `crates/service`) has no breaking changes between 0.17.0 and 0.19.1 beyond what can be resolved mechanically at call sites; if a resolution is not mechanical, FR-007 still governs the outcome (no IPC-observable change), and the resolution approach is a Research/Plan-stage decision, not a spec change.
- A 0.17.0-format WAL sample sufficient to exercise SC-003 either already exists in the repo's test fixtures or can be generated by running the existing test/eval suite under the current pin before bumping it. If neither is true, that is a Research-stage finding, not a blocker to writing this spec.
- CI hang reproduction is inherently probabilistic — it was observed 3 times, not on every run — so "several" CI runs post-upgrade (per the issue's own Verification guidance) is treated as sufficient evidence for User Story 4, not a guarantee of full elimination.
- `~/dev/liminis-project/multistream_test.py` remains available and runnable in the environment used to verify SC-004; it lives in the sibling `liminis-project` checkout, outside this repository.
- Local, non-CI reproduction of the OpenSSL 3 linkage problem may not be possible on every developer's machine (it may only manifest in the release build path); the release workflow itself (SC-002) is the authoritative check, not a developer's local build.

## Out of Scope

- Using any of the new lbug capabilities beyond what upgrading itself requires: adopting secondary ART indexes (e.g. migrating `get_entity_by_name_ci` off its in-process `HashMap` — tracked separately by #221), exploiting cost-based query planning, tuning HNSW scalar quantization, or relying on WAL group commits. These are motivations for the upgrade's value, not requirements of this issue — Requirements FR-001 through FR-008 cover the dependency bump only.
- Filing or implementing the "per-test CI timeout" improvement the issue notes as worth doing — the issue explicitly defers this to a separate issue.
- Root-causing the CI hangs further if they persist after the upgrade. SC-001/User Story 4 verify whether the upgrade resolves them; if not, the Linux-vs-macOS and #378 leads the issue names are follow-up investigation, not additional scope here.
- Trying 0.17.1 as a cheaper preliminary experiment before committing to the full upgrade — the issue considers and declines this path by being filed directly against 0.19.1.

## Source References

- `Cargo.toml` (workspace `lbug` pin, release target matrix, GCC/`<format>`-header runner pin comment), `.cargo/config.toml` (`LBUG_VERSION` env pin, fat-bundle/no-source-build comments)
- `crates/core/tests/ipc_parity.rs` — Tier 1a/1b/1c IPC parity tests
- `crates/service/src/mcp/tools.rs` — MCP tool registry and its scope-bucket count tests
- ADR-0002 — readers do not take the write lock (relevant to upstream `#615`'s read-only-open-during-checkpoint fix)
- ADR-0009, ADR-0025, ADR-0026 — degraded-mode startup/recovery, auto-heal index build, episode-cursor WAL resume
- Issue #341 / its FR-006 — the no-recompile CI assertion for the prebuilt lbug bundle
- Issue #221 — blocked on secondary ART indexes; explicitly out of scope here
- Issue #378 — `check_no_case_insensitive_collision`, one of the leads if hangs persist post-upgrade
- Issues #190 and #220 — superseded by this issue; #190's linkage analysis is reproduced in Background
- `~/dev/liminis-project/multistream_test.py` — SC-004's 9/10 regression check
- Upstream `LadybugDB/ladybug` releases 0.17.1 through 0.19.1, and PRs `#590`, `#579`, `#581`, `#665`, `#687`, `#689`, `#615`, `#582`, `#577`, `#574`, `#569`, `#673`, `#547`, `#762`
