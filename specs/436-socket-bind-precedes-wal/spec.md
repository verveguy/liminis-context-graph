# Feature Specification: Socket bind precedes #378 WAL-root migration, so readiness can be observed before migration completes

**Feature Branch**: `fabrik/issue-436`
**Created**: 2026-08-21
**Status**: Specified
**Input**: User description: "`crates/service/tests/migration_binary.rs::binary_migrates_legacy_workspace_on_startup` failed on CI with an ordering bug: `UnixListener::bind()` in `crates/service/src/main.rs`'s `CliMode::Socket` startup path runs before `bootstrap_app_state()`, which is what performs the #378 WAL-root migration. Because a Unix-domain `bind()` implicitly queues connections at the kernel level before the process's own `accept()` loop starts, a readiness check based on socket-connectability alone can observe the service as 'ready' before WAL-root migration has actually completed."

## Background

The test `binary_migrates_legacy_workspace_on_startup` failed in CI (run [32129112344](https://github.com/verveguy/liminis-context-graph/actions/runs/32129112344)) asserting that WAL files had not been migrated to `.lcg/wal/liminis/` as required by the #378 WAL-root layout. The failure was initially misdiagnosed as a reproduction of the #429 embedder-timing race (stray stderr from a concurrently running test binary was mistaken for the causal error), but re-investigation of the actual assertion failure identified a genuine ordering bug rather than test infrastructure flakiness:

- `crates/service/src/main.rs` — the legacy `.graphiti/` → `.lcg/` workspace migration (`migration::migrate_workspace()`) runs early in `async_main`, before the socket is bound. This alone produces `.lcg/db/`, `.lcg/db/liminis.db`, and `.lcg/wal/` — the paths the test's filesystem assertions check for existence.
- In the `CliMode::Socket` arm, `UnixListener::bind(&socket_path)` runs next, with an explicit in-code rationale (citing ADR-0009): binding early lets `health_check` and recovery IPC work even while the DB is in a degraded state. A Unix-domain `bind()` implicitly calls `listen()`, so the kernel queues incoming connections immediately — before the process ever calls `accept()`.
- `bootstrap_app_state()` runs after the bind call. Only inside it does the #378 per-group WAL-root relocation (`lcg_core::wal_group::migrate_wal_root_if_needed()`, `main.rs:543`) happen — the exact migration step the failing assertion checks for — followed by `Db::open()` (`main.rs:565`).
- The process's own accept loop does not start until `run_socket_service()`, which runs after `bootstrap_app_state()` has resolved.

A readiness check that only confirms "the socket accepts a connection" (e.g. `socket_path.exists() && UnixStream::connect(...).is_ok()`) can therefore succeed in the window between `bind()` and the completion of `bootstrap_app_state()`, before WAL-root migration has necessarily finished — because kernel-level connection queuing at `bind()`/`listen()` time is independent of whether the application has started serving requests. On a loaded CI runner this window is wide enough to be observed; locally it was not.

**This has changed since the issue was first filed.** Commit `068f93a` ("Fix race in `binary_migrates_legacy_workspace_on_startup`"), which landed on `main` via issue #437's branch on 2026-08-19 (one day after the corrected-diagnosis comment below was posted), already fixes the specific CI failure this issue was filed for: it changes the test's own readiness check from bare socket-connectability to a blocking `knowledge_status` IPC round-trip, which can only return a response once `bootstrap_app_state()` — and therefore WAL-root migration — has resolved. That fix does **not** change `main.rs`'s startup ordering itself: `UnixListener::bind()` still precedes `bootstrap_app_state()`/WAL-root migration in the `CliMode::Socket` path, unchanged from the description above.

**Resolution (2026-08-21): this issue scopes to documentation, not a production behavior change.** Investigation confirmed that a health *request/response* round-trip is already a sufficient readiness signal today, by construction: `handle_health_check` (`handlers.rs:196-215`) reports `{"healthy": true, "state": "healthy"}` only on the `Some(db)` branch, and the DB does not open until after both `migrate_workspace()` and `migrate_wal_root_if_needed()` have run. It is bare socket-connectability — not a health round-trip — that is an unsafe readiness signal, and that gap is intentional: the socket binds early specifically so `knowledge_recover` stays reachable while the DB is in a legitimately degraded state (ADR-0009). An audit of the consumer that matters most, the Electron app, found it already does this correctly: `context-graph-lifecycle.ts` treats a successful `connect()` as insufficient evidence of readiness and instead performs an RPC health round-trip via `waitForServiceReady()` (`liminis-app/src/main/context-graph-lifecycle.ts:345,609`), holding `serviceState.initializing` true until that round-trip reports healthy.

What was previously undocumented, and is the actual gap this issue closes, is the invariant itself: **socket-connectability is not a readiness signal for this service; a `health_check` round-trip is.** This is non-obvious precisely because the early bind is intentional and correct, and is exactly the mistake a new consumer could make. The fix is to write this invariant down where a new integration (this repo's own tooling, or another service client) would find it, not to change working startup code.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Readiness is documented as a health round-trip, not a bare connect (Priority: P1)

As a developer integrating a new client against the `liminis-context-graph` service (e.g. a new internal tool, or another consumer alongside the existing Electron app), I need the documentation to tell me that a successful socket connection does not mean the service has finished startup migration, and that a `health_check` round-trip reporting `healthy` is the correct readiness signal, so that I don't write a client that acts on stale or incomplete on-disk state by treating `connect()` succeeding as sufficient, without waiting for that round-trip.

**Why this priority**: This is the entire scope of the issue — the readiness signal's correct usage is what must be made discoverable.

**Independent Test**: Can be verified by reading the updated documentation and confirming it accurately states (a) that the socket binds before the database opens and before WAL-root migration completes, (b) that this is deliberate, to keep `health_check`/recovery IPC reachable in degraded mode per ADR-0009, and (c) that a `health_check` response reporting `healthy` — not a successful connect — is the correct readiness signal, with the consequence spelled out for a client that skips this check.

**Acceptance Scenarios**:

1. **Given** a developer reading the project's IPC/startup documentation, **When** they look for how to determine the service is ready after startup, **Then** the documentation states that a `health_check` round-trip reporting `healthy` is the readiness signal, and that bare socket-connectability is not.
2. **Given** the existing (unchanged) startup ordering in `main.rs`, **When** a client connects and sends a `health_check` request during startup, **Then** the request queues at the kernel level and is not read by the service until its accept loop starts (which only happens after `bootstrap_app_state()` — and therefore WAL-root migration and `Db::open()` — has resolved), so the response is never observed mid-migration: it is `healthy` if startup succeeded or `degraded` if it did not — this is existing, verified behavior that the documentation must describe accurately, not new behavior to build.

---

### Edge Cases

- A workspace requiring no migration at all (already on the current layout) — the documented readiness mechanism must not imply any added startup latency for the common case where there is nothing to migrate; the health round-trip is the same call regardless of whether migration occurred.
- A workspace where migration fails and the service starts in degraded mode (per ADR-0009) — the documentation must describe `degraded` health status as the expected, persistent outcome in this case. There is no separate transient "mid-migration" `degraded` response a client can observe: the accept loop itself does not start until startup work has already resolved one way or the other (see User Story 1's Acceptance Scenario 2). The documentation must not suggest a settled `degraded` response is something the client should block on or treat as an error to retry indefinitely.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Documentation (`docs/ipc-mcp-reference.md`, and any other location describing service startup or readiness) MUST state that the Unix-domain socket binds before the database opens — and therefore before #378 WAL-root migration (`migrate_wal_root_if_needed()`, invoked from `bootstrap_app_state()`) completes — and that this ordering is deliberate, to keep `health_check`/recovery IPC reachable per ADR-0009.
- **FR-002**: Documentation MUST state that a successful `health_check` response reporting `healthy` is the correct readiness signal, that bare socket-connectability is not, and MUST spell out the consequence: a client that treats `connect()` succeeding as readiness by itself (e.g. acting on assumed-ready on-disk state) can act before WAL-root migration has run.
- **FR-003**: This issue MUST NOT change `crates/service/src/main.rs`'s startup ordering, introduce a new readiness signal, or change the semantics of what `healthy` means in `handle_health_check` — the existing behavior is already correct; only its documentation is missing. (What `healthy` means is constrained independently by FR-004 of issue #456.)
- **FR-004**: The fix MUST NOT remove or weaken ADR-0009's existing degraded-mode behavior, in which `health_check` and recovery IPC remain reachable while the DB is in a legitimately degraded state (e.g. migration failure, not migration-in-progress).

### Key Entities *(if applicable)*

- **Readiness signal**: A `health_check` request/response round-trip reporting `healthy` — the mechanism this issue documents as correct, as distinct from bare socket-connectability, which this issue documents as insufficient.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `binary_migrates_legacy_workspace_on_startup` passes reliably in CI (already true as of commit `068f93a`; retained here as a regression guard for this issue's scope).
- **SC-002**: `docs/ipc-mcp-reference.md` (and any other location describing service startup/readiness) documents the bind-before-migration ordering, its ADR-0009 rationale, and the `health_check`-round-trip readiness signal, such that a developer reading it would not write a client relying on bare socket-connectability.

## Assumptions

- Commit `068f93a` (landed 2026-08-19 via issue #437) already resolves the specific CI failure this issue was originally filed for, by changing the test's readiness check from bare socket-connectability to a blocking `knowledge_status` IPC round-trip. That fix is a change to the test only; `main.rs`'s startup ordering (bind before `bootstrap_app_state()`/WAL-root migration) is unchanged and stays unchanged under this issue too.
- `handle_health_check`'s existing `Some(db)`/`None` branching (`handlers.rs:196-215`) already guarantees a `healthy` response cannot occur before the DB opens, which is after both `migrate_workspace()` and `migrate_wal_root_if_needed()` have run. This issue documents that guarantee; it does not need to create it.
- The Electron app (`liminis-app/src/main/context-graph-lifecycle.ts`) already implements the correct pattern (`waitForServiceReady()` performing a health round-trip, not treating `connect()` as readiness), confirmed by inspection of `context-graph-lifecycle.ts:345,609`. No change to that consumer is in scope.

## Out of Scope

- Changes to ADR-0009's degraded-mode design itself (bind-early for `health_check`/recovery IPC when the DB is in a legitimately degraded state, as opposed to still migrating).
- Any change to `main.rs`'s startup/bind ordering.
- Any new or modified readiness signal, or change to `handle_health_check`'s response semantics (see FR-003; `healthy` semantics are constrained separately by FR-004 of #456).
- Changes to the Electron app or any other existing consumer — the audit performed during specification found no consumer in scope relying on bare socket-connectability.

## Source References

- Failing run: https://github.com/verveguy/liminis-context-graph/actions/runs/32129112344 (job: `test (ubuntu-latest)`, step: `cargo test --release (restored build; captured for recompile-regression check)`)
- Related: #429 (original fix for the embedder-timing failure mode, closed), #430 (the gate-arming issue that surfaced this flake for the first time since #429 landed), #437 (startup migration-ordering issue whose branch landed commit `068f93a`, which fixes this issue's specific test failure), #456 (constrains `handle_health_check`'s `healthy` semantics independently via its FR-004)
- `docs/adr/0009-degraded-mode-startup-recovery.md` — the degraded-mode bind-early rationale this issue's fix must not weaken
- `crates/service/src/main.rs:543` (`migrate_wal_root_if_needed`), `crates/service/src/main.rs:565` (`Db::open`), `crates/service/src/handlers.rs:196-215` (`handle_health_check`)
- `liminis-app/src/main/context-graph-lifecycle.ts:345,609` (`waitForServiceReady`) — existing correct consumer pattern, confirmed during specification
