# Feature Specification: Capture and Log the Sender PID of Received SIGTERM

**Feature Branch**: `fabrik/issue-247`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "diag(service): capture and log the sender PID of received SIGTERM"

## Background

The context-graph service intermittently enters a restart loop on app startup. It receives `SIGTERM` from something **outside** the Electron lifecycle manager's own stop paths — the giveaway is that no `"Stopping context graph service"` log precedes the service's `"received SIGTERM"`. It then exceeds its shutdown drain while mid-ingestion, exits, and is auto-restarted. Each restart drops the six pooled socket connections, which surfaces to users as **"MCP Server knowledge-writer failed"** toasts, because the in-process `knowledge-writer` / `knowledge-reader` providers can't connect during the teardown window.

**The bug is currently undiagnosable.** POSIX exposes the *sending* process's PID only via `SA_SIGINFO`, and tokio's signal stream does not surface it. So the logs can say a `SIGTERM` arrived but never who sent it, leaving the actual culprit unidentified. The standing suspicion is a second Liminis-family instance performing a stale-PID reclaim, but that has never been confirmed or refuted.

This also matters beyond the original report: unexplained process death has cost real debugging time elsewhere in this repo (a sidecar became unreachable repeatedly during the #217 corpus capture with no attributable cause), and "something killed my service" is not a fixable class of bug without sender attribution.

The fix is diagnostic only: register an observe-only `SA_SIGINFO` signal handler that records the sending process's PID, and include it in the shutdown log line, so the next occurrence names the culprit via `ps -p <pid>`.

**This replaces PR #194**, which implemented the same idea on 2026-07-19 but is now 143 commits behind `main` and conflicts (`mergeable_state: dirty`) on all three files it touches. The shutdown path has been redesigned since that PR was written — ADR-0035 §7 replaced `#[tokio::main]` with a manually-owned runtime plus `shutdown_timeout`, specifically to bound the blocking-pool drain without breaking ADR-0017's WAL-checkpoint-before-exit guarantee — so this is a fresh implementation against the current shutdown path, not a rebase of #194.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator identifies who sent an unexpected SIGTERM (Priority: P1)

An operator (or the app's own crash-report pipeline) observes the service exiting unexpectedly. They check the service's stderr log and find the shutdown line now names the PID of the process that sent the `SIGTERM`. They run `ps -p <pid>` (or check it against a known PID, e.g. a second Liminis-family instance) to confirm or refute the sender's identity, closing the previously-undiagnosable gap.

**Why this priority**: This is the entire purpose of the issue — without sender attribution, the restart-loop bug (and any future "something killed my service" report) cannot be diagnosed at all. There is no smaller useful slice.

**Independent Test**: Start the service, send `SIGTERM` from a known process (e.g. `kill -TERM <service_pid>` run from a shell with a known PID), and confirm the shutdown log line includes that exact PID.

**Acceptance Scenarios**:

1. **Given** the service is running in socket-service mode, **When** it receives `SIGTERM` from a process with a known PID, **Then** the shutdown log line emitted for that signal includes the sender's PID, and it matches the actual sending process.
2. **Given** the service is running in standalone MCP-over-stdio mode, **When** it receives `SIGTERM` from a process with a known PID, **Then** the shutdown log line emitted for that signal includes the sender's PID, and it matches the actual sending process.
3. **Given** the service receives `SIGINT` (Ctrl+C) instead of `SIGTERM`, **When** it shuts down, **Then** behavior is unchanged from today — sender-PID capture is `SIGTERM`-specific and does not apply to `SIGINT`.

---

### User Story 2 - Existing shutdown guarantees remain intact (Priority: P1)

A contributor runs the existing shutdown regression suite after this change lands. The WAL-checkpoint-before-exit guarantee, the bounded drain, and the exit-code-0 clean-shutdown behavior all continue to hold exactly as before — this change is purely additive to the log output.

**Why this priority**: The shutdown path is safety-critical (WAL integrity on process exit, per ADR-0017) and was recently redesigned (ADR-0035 §7) specifically to bound blocking-pool drain correctly. A diagnostic feature that destabilizes shutdown timing or ordering would trade an undiagnosable bug for a worse, corruption-risking one.

**Independent Test**: Run `crates/service/tests/clean_shutdown.rs` and `crates/service/tests/mcp_clean_shutdown.rs` unmodified after the change; both must continue to pass.

**Acceptance Scenarios**:

1. **Given** the sender-PID capture handler is registered, **When** the existing `clean_shutdown.rs` and `mcp_clean_shutdown.rs` regression tests run, **Then** both pass with their existing assertions (exit code 0, WAL re-opens without corruption) unchanged.
2. **Given** the new `SA_SIGINFO` handler is installed, **When** the service runs its normal tokio-based signal handling (the async stream that actually drives the shutdown sequence), **Then** that handling is unaffected in timing or behavior — the new handler only records a PID and does not participate in triggering or sequencing shutdown.

---

### Edge Cases

- **Signal captured before tokio's async handler drains it**: `SA_SIGINFO` handlers run synchronously on receipt, potentially before tokio's own signal-handling task is scheduled. The recorded PID must be visible by the time the shutdown log line is emitted, without introducing a race (e.g., reading the atomic before the store completes). An atomic `Ordering` that guarantees visibility across the store (from the signal handler context) and the load (from the async task that logs it) is required.
- **No SIGTERM received (SIGINT-only shutdown, or process exits via other means)**: The sender-PID field must have a well-defined "no sender recorded" state distinguishable from an actual PID of 0 or 1, since those are valid low PIDs the atomic could theoretically alias with an unset default of `0`.
- **Multiple SIGTERM deliveries before shutdown completes**: only the identity of the delivery that matters for diagnosis is the one that actually reached the handler; if the OS coalesces repeated `SIGTERM`s (as is standard POSIX behavior for non-realtime signals), only the most recent sender before the handler fires is captured — this is acceptable since the goal is diagnosis, not a complete audit trail.
- **Two call sites**: the service has two independent places where `SIGTERM` triggers shutdown — `run_socket_service` (default Unix-socket mode) and `run_mcp_standalone` (MCP-over-stdio mode, added by #195/ADR-0035). Both currently have their own `"received SIGTERM, shutting down"` log line; both must include the sender PID after this change, consistent with the acceptance criteria referencing both test files.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The service MUST register an `SA_SIGINFO`-based signal handler for `SIGTERM` (via `signal-hook-registry`, already present transitively through tokio's own signal-handling dependency) that records the sending process's PID (`si_pid` from `siginfo_t`) into an atomic, in addition to — not instead of — tokio's existing async `SIGTERM` handling that drives the shutdown sequence.
- **FR-002**: The `SA_SIGINFO` handler MUST be observe-only and async-signal-safe: a single atomic store, no heap allocation, no locking, no I/O, and no other side effects. This MUST be verifiable by inspection and noted in an accompanying code comment.
- **FR-003**: The `SA_SIGINFO` handler MUST NOT alter, delay, or interfere with tokio's existing `SIGTERM`/`SIGINT` handling or with the normal shutdown sequence (cancellation, connection drain, WAL checkpoint via `Arc<Db>` drop, telemetry emission).
- **FR-004**: Both existing `"received SIGTERM, shutting down"` shutdown log lines (in `run_socket_service` and in `run_mcp_standalone`) MUST include the recorded sender PID.
- **FR-005**: When no sender PID has been recorded at the time a `SIGTERM` shutdown log line is emitted (e.g., a hypothetical race or an unexpected code path), the log line MUST make this explicit (e.g., an "unknown" marker) rather than printing a misleading placeholder value like `0`.
- **FR-006**: This feature MUST be `cfg(unix)`-gated in its entirety (handler registration, atomic, and any additional dependency), consistent with the existing `#[cfg(unix)]` gating already present around `SIGTERM` handling in `main.rs`. Windows behavior is unaffected.
- **FR-007**: This change MUST introduce zero behavioral change to shutdown semantics: signal-triggered cancellation, the bounded drain via `shutdown_timeout_ms`, the WAL-checkpoint-before-exit guarantee (ADR-0017), and the manually-owned-runtime `shutdown_timeout` drain (ADR-0035 §7) all continue to behave exactly as before this change.
- **FR-008**: A test MUST assert that the sender PID recorded for a `SIGTERM` delivered by a known process during a controlled test scenario matches that process's actual PID. This assertion MAY be added to `clean_shutdown.rs` / `mcp_clean_shutdown.rs` or introduced as a new test, provided the existing assertions in both files (exit code 0, WAL-checkpoint integrity) continue to pass without modification.

### Key Entities

- **Sender PID atomic**: process-wide storage (e.g. `AtomicI32` or `AtomicU32`) holding the PID of the most recent `SIGTERM` sender, with a distinguishable "unset" sentinel, written only from the `SA_SIGINFO` handler and read only from the async task that logs the shutdown line.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On receiving `SIGTERM` from a process with a known PID, the emitted shutdown log line names that PID, verified by an automated test.
- **SC-002**: `crates/service/tests/clean_shutdown.rs` and `crates/service/tests/mcp_clean_shutdown.rs` both pass with their pre-existing assertions unchanged, including the WAL-checkpoint-before-exit guarantee.
- **SC-003**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` are all green.
- **SC-004**: The signal-handling code path added is entirely absent on non-Unix targets (verified by `cfg(unix)` gating — no new non-`cfg(unix)` dependency is introduced).

## Assumptions

- `signal-hook-registry` is already present in the dependency tree transitively through tokio's signal handling (confirmed present in `Cargo.lock`), so no new external crate needs to be added to `Cargo.toml` for the handler registration itself.
- The "known process" referenced in the acceptance criteria is whatever process the test harness uses to deliver `SIGTERM` to the service under test (both existing tests currently shell out to the `kill` command); the test only needs to confirm the recorded PID matches that sender, not any particular sender identity.
- Reacting to the sender's identity (e.g. ignoring signals from an unrecognized PID, or rejecting a stale-PID reclaim attempt) is explicitly out of scope — this issue is diagnosis only.
- Fixing whatever process is actually sending the unexpected `SIGTERM` is out of scope and will be a follow-up issue once the sender is identified via this diagnostic.
- Windows support is out of scope; the feature is `cfg(unix)`-gated entirely.

## Out of Scope

- Reacting to the sender PID in any way (ignoring, rejecting, or reclaiming based on sender identity).
- Fixing or investigating the actual source of the unexpected `SIGTERM` (e.g., confirming or refuting the "second Liminis-family instance performing a stale-PID reclaim" hypothesis) — that is a follow-up once this diagnostic ships and the bug recurs.
- Windows signal handling.
- Rebasing or resolving conflicts on PR #194 — this issue supersedes it with a fresh implementation against the current (ADR-0035 §7) shutdown path.
- Capturing sender PID for `SIGINT` or any signal other than `SIGTERM`.

## Source References

- `crates/service/src/main.rs` — the two existing `SIGTERM` handling sites (`run_socket_service`, `run_mcp_standalone`) and their `"received SIGTERM, shutting down"` log lines.
- `crates/service/tests/clean_shutdown.rs`, `crates/service/tests/mcp_clean_shutdown.rs` — existing shutdown regression tests, both of which send `SIGTERM` via an external `kill` process.
- `docs/adr/0035-mcp-stdio-transport.md` §7 — the manually-owned-runtime / `shutdown_timeout` design this change must integrate with.
- `docs/adr/0017-replace-process-exit-with-normal-return.md` — the WAL-checkpoint-before-exit guarantee that must remain intact.
- Issue #217 — prior unexplained sidecar unreachability during corpus capture, cited as further motivation for sender attribution.
- Issues #229/#230 — UDS connection-churn fixes that may already have resolved the downstream symptom (restart loop), independent of this diagnostic's value.
- PR #194 — prior implementation of this idea, now superseded due to staleness and the ADR-0035 shutdown-path redesign.
