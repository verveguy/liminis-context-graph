# Feature Specification: Socket bind precedes #378 WAL-root migration, so readiness can be observed before migration completes

**Feature Branch**: `fabrik/issue-436`
**Created**: 2026-08-21
**Status**: Draft
**Input**: User description: "`crates/service/tests/migration_binary.rs::binary_migrates_legacy_workspace_on_startup` failed on CI with an ordering bug: `UnixListener::bind()` in `crates/service/src/main.rs`'s `CliMode::Socket` startup path runs before `bootstrap_app_state()`, which is what performs the #378 WAL-root migration. Because a Unix-domain `bind()` implicitly queues connections at the kernel level before the process's own `accept()` loop starts, a readiness check based on socket-connectability alone can observe the service as 'ready' before WAL-root migration has actually completed."

## Background

The test `binary_migrates_legacy_workspace_on_startup` failed in CI (run [32129112344](https://github.com/verveguy/liminis-context-graph/actions/runs/32129112344)) asserting that WAL files had not been migrated to `.lcg/wal/liminis/` as required by the #378 WAL-root layout. The failure was initially misdiagnosed as a reproduction of the #429 embedder-timing race (stray stderr from a concurrently running test binary was mistaken for the causal error), but re-investigation of the actual assertion failure identified a genuine ordering bug rather than test infrastructure flakiness:

- `crates/service/src/main.rs` — the legacy `.graphiti/` → `.lcg/` workspace migration (`migration::migrate_workspace()`) runs early in `async_main`, before the socket is bound. This alone produces `.lcg/db/`, `.lcg/db/liminis.db`, and `.lcg/wal/` — the paths the test's filesystem assertions check for existence.
- In the `CliMode::Socket` arm, `UnixListener::bind(&socket_path)` runs next, with an explicit in-code rationale (citing ADR-0009): binding early lets `health_check` and recovery IPC work even while the DB is in a degraded state. A Unix-domain `bind()` implicitly calls `listen()`, so the kernel queues incoming connections immediately — before the process ever calls `accept()`.
- `bootstrap_app_state()` runs after the bind call. Only inside it does the #378 per-group WAL-root relocation (`lcg_core::wal_group::migrate_wal_root_if_needed()`) happen — the exact migration step the failing assertion checks for.
- The process's own accept loop does not start until `run_socket_service()`, which runs after `bootstrap_app_state()` has resolved.

A readiness check that only confirms "the socket accepts a connection" (e.g. `socket_path.exists() && UnixStream::connect(...).is_ok()`) can therefore succeed in the window between `bind()` and the completion of `bootstrap_app_state()`, before WAL-root migration has necessarily finished — because kernel-level connection queuing at `bind()`/`listen()` time is independent of whether the application has started serving requests. On a loaded CI runner this window is wide enough to be observed; locally it was not.

**This has since changed since the issue was first filed.** Commit `068f93a` ("Fix race in `binary_migrates_legacy_workspace_on_startup`"), which landed on `main` via issue #437's branch on 2026-08-19 (one day after the corrected-diagnosis comment below was posted), already fixes the specific CI failure this issue was filed for: it changes the test's own readiness check from bare socket-connectability to a blocking `knowledge_status` IPC round-trip, which can only return a response once `bootstrap_app_state()` — and therefore WAL-root migration — has resolved. That fix does **not** change `main.rs`'s startup ordering itself: `UnixListener::bind()` still precedes `bootstrap_app_state()`/WAL-root migration in the `CliMode::Socket` path, unchanged from the description above.

What remains genuinely open is whether that unchanged ordering still constitutes a live correctness gap for any consumer other than this one test. Because the process's accept loop itself does not start until after `bootstrap_app_state()` resolves, a real IPC client's first *request* is queued at the kernel level and is not read or answered until after WAL-root migration completes — the client's own request/response round-trip is therefore already safe, in the same way the fixed test's `knowledge_status` round-trip is safe. The concern raised is narrower: whether any consumer (this project's own tooling, or an external client such as the Electron app's IPC layer) determines "the service is ready" using bare socket-connectability alone, without a subsequent request/response check, and could act on that false-positive readiness signal.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Readiness checks reflect completed startup migration (Priority: P1)

As a developer or CI job that starts the `liminis-context-graph` binary against a legacy `.graphiti/`-era workspace, I need any readiness/health check I perform against the service to reflect that startup migration (including #378 WAL-root relocation) has actually completed, so that I don't act on stale or incomplete on-disk state.

**Why this priority**: This is the entire scope of the issue — the readiness signal is the thing under investigation.

**Independent Test**: Can be tested by spawning the binary against a legacy workspace, using whichever readiness signal the fix settles on, and confirming that signal does not fire until the on-disk WAL-root layout is fully migrated.

**Acceptance Scenarios**:

1. **Given** a legacy `.graphiti`-era workspace requiring WAL-root migration, **When** the binary starts and a client determines readiness using the project's documented/intended readiness mechanism, **Then** that mechanism does not report the service ready until WAL-root migration has completed.

---

### Edge Cases

- A workspace requiring no migration at all (already on the current layout) — the readiness mechanism must not introduce added startup latency for the common case where there is nothing to migrate.
- A workspace where migration fails and the service starts in degraded mode (per ADR-0009) — the readiness mechanism's behavior in this case must remain consistent with ADR-0009's existing degraded-mode IPC access, not accidentally block it.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The documented/intended mechanism by which a client determines the service has finished startup migration MUST NOT report readiness until #378 WAL-root migration (`migrate_wal_root_if_needed()`, invoked from `bootstrap_app_state()`) has completed.
- **FR-002**: The fix MUST NOT remove or weaken ADR-0009's existing degraded-mode behavior, in which `health_check` and recovery IPC remain reachable while the DB is in a legitimately degraded state (e.g. migration failure, not migration-in-progress).

### Key Entities *(if applicable)*

- **Readiness signal**: Whatever mechanism (socket connectability, a request/response IPC call, or another explicit signal) a client uses to determine the service has finished starting up.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `binary_migrates_legacy_workspace_on_startup` passes reliably in CI (already true as of commit `068f93a`; retained here as a regression guard for this issue's scope).
- **SC-002**: No consumer of the service's readiness signal identified in scope for this issue can observe the service as ready before WAL-root migration has completed.

## Assumptions

- Commit `068f93a` (landed 2026-08-19 via issue #437) already resolves the specific CI failure this issue was originally filed for, by changing the test's readiness check from bare socket-connectability to a blocking `knowledge_status` IPC round-trip. That fix is a change to the test only; `main.rs`'s startup ordering (bind before `bootstrap_app_state()`/WAL-root migration) is unchanged.
- Because the process's accept loop does not start until after `bootstrap_app_state()` resolves, any client that performs an actual request/response round-trip (not just a bare connect) already receives a response only after WAL-root migration has completed — this is what makes the `068f93a` test fix sufficient for that one test.

## Out of Scope

- Changes to ADR-0009's degraded-mode design itself (bind-early for `health_check`/recovery IPC when the DB is in a legitimately degraded state, as opposed to still migrating).

## Source References

- Failing run: https://github.com/verveguy/liminis-context-graph/actions/runs/32129112344 (job: `test (ubuntu-latest)`, step: `cargo test --release (restored build; captured for recompile-regression check)`)
- Related: #429 (original fix for the embedder-timing failure mode, closed), #430 (the gate-arming issue that surfaced this flake for the first time since #429 landed), #437 (startup migration-ordering issue whose branch landed commit `068f93a`, which fixes this issue's specific test failure)
- `docs/adr/0009-degraded-mode-startup-recovery.md` — the degraded-mode bind-early rationale this issue's fix must not weaken
