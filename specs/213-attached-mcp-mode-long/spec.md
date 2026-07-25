# Feature Specification: Attached MCP Mode — Fix False-Timeout on Long Whole-Graph Ops and Add Reconnect After Service Restart

**Feature Branch**: `fabrik/issue-213`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "Attached MCP mode (`--mcp-stdio --connect <sock>`) has two distinct DX defects, reported in #206 Part B: (a) `AttachedBackend` applies `LCG_ATTACHED_CALL_TIMEOUT_MS` per `read_line`; streaming ops re-arm the timer on each progress line, but non-streaming whole-graph ops run for 40–60s+ with no output, trip the idle timeout, and report a false failure to the client while the server actually completes the work. (b) `AttachedBackend` connects one `UnixStream` once at startup and has no reconnect logic — after the remote service restarts, the same dead stream is reused for every subsequent call and the attached client is wedged until the process is restarted."

## Background

`AttachedBackend` (`crates/service/src/mcp/attached.rs`) is the attached-mode (`--connect <socket-path>`) MCP transport: instead of opening the `.lcg` database itself, the MCP process forwards every `tools/call` as JSON-RPC over a Unix socket to an already-running `liminis-context-graph` service (e.g. the Liminis app's own instance), so it never contends for lbug's single-writer lock. This mode is meant to be a transparent proxy — a caller shouldn't be able to tell whether they're attached to a remote service or running standalone.

Two defects break that transparency, both reported against real usage in issue #206 Part B:

**(a) Long whole-graph ops can false-timeout.** The attached connection applies an idle-read timeout (`LCG_ATTACHED_CALL_TIMEOUT_MS`, default 30s) per `read_line`, not per call — a call that keeps emitting `{"type":"progress"}` lines never trips it, because each line resets the timer. This works correctly today for the operations that already emit progress. But a whole-graph write method that runs long (tens of seconds to minutes on a large graph) **without** emitting any progress output looks, from the client's perspective, identical to a hung/crashed remote — the idle timer trips, the client is told the call failed, and the server silently finishes the work anyway. The result is a false failure report that can also mislead a caller into re-issuing the same (possibly non-idempotent) write believing it never ran.

**(b) No reconnect after a remote restart.** `AttachedBackend::connect()` opens exactly one `UnixStream` at process startup and never reopens it. If the remote service process restarts (deploy, crash-and-supervisor-restart, manual restart) while the attached MCP process keeps running, the next call's `write_all` fails with a broken-pipe error, is reported to the client as a normal call failure, and — critically — the same dead stream is left in place and reused for every call after that. The attached MCP process is now permanently wedged with respect to that remote until *it* is also restarted, even though a working socket is available again seconds later.

**Current-state correction (discovered during Specify-stage investigation, materially narrows the scope below).** The issue as originally filed named `canonicalize_relations`, `backfill_relation_types`, and `reprocess_entity_types` as the non-streaming ops at risk from (a), with `reprocess_relation_types` (then unimplemented) as a fourth to add once it existed. Since the issue was filed, its listed dependency **#210 has merged (PR #222)**: `knowledge_reprocess_relation_types` now exists, already emits progress, and is already registered in `is_streaming_method`. Investigation also found that `knowledge_canonicalize_relations` and `knowledge_backfill_relation_types` are **already** registered in `is_streaming_method` and already emit progress on `main` — this predates issue #213 and is not something this issue needs to (re-)do. The one remaining gap is `knowledge_reprocess_entity_types`: its handler (`handle_reprocess_entity_types` in `crates/core/src/handlers.rs`) does not accept or emit progress at all today, and it is absent from `is_streaming_method`. This issue's remaining scope for part (a) is narrowed accordingly: wire progress emission into `reprocess_entity_types` (mirroring the pattern just established for `reprocess_relation_types` in `crates/core/src/reprocess_relations.rs`) and register it as streaming. Part (b) — the reconnect logic — is untouched by any of this and remains fully in scope as originally described.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A long `reprocess_entity_types` run over attached mode does not false-timeout (Priority: P1)

An operator has an attached MCP client (`--connect <socket>`) pointed at a running service with a large graph. They call `knowledge_reprocess_entity_types` with a progress token, expecting a run that takes well over the default 30s idle timeout (e.g. reclassifying thousands of entities against a large ontology). Today this call is reported as a timeout failure to the client even though the server completes it successfully in the background — the operator sees a spurious error and cannot tell, from the client side alone, whether the reclassification actually happened.

**Why this priority**: This is the last of the four whole-graph write methods still missing progress-driven timeout protection; without it, attached-mode operators cannot safely run entity reclassification on any graph large enough to take longer than the idle timeout.

**Independent Test**: Start a socket service and an attached MCP client against it (per the existing `crates/service/tests/mcp_attached.rs` harness). Seed enough entities that `knowledge_reprocess_entity_types` takes longer than a short, test-scaled `LCG_ATTACHED_CALL_TIMEOUT_MS`. Call it with a progress token and assert the call succeeds (not a timeout error) and that progress notifications were observed during the run.

**Acceptance Scenarios**:

1. **Given** an attached MCP client with `LCG_ATTACHED_CALL_TIMEOUT_MS` set low enough that the run would exceed it if idle, **When** the client calls `knowledge_reprocess_entity_types` with a progress token against a graph whose reclassification takes longer than that timeout, **Then** the call completes successfully and is not reported as a timeout.
2. **Given** the same setup, **When** the run is in progress, **Then** the client observes one or more `{"type":"progress"}` notifications before the terminal response.
3. **Given** a caller that does not supply a progress token, **When** they call `knowledge_reprocess_entity_types`, **Then** behavior is unchanged from today (no progress notifications; the idle timeout still applies as it does for any other non-streaming call) — this issue does not change behavior for callers that opt out of progress tracking.

---

### User Story 2 - Attached client survives a remote service restart (Priority: P1)

An operator has an attached MCP client running against a socket service. The remote service process is restarted (deploy, crash recovery, manual bounce) while the attached client keeps running. Today, the next call after the restart fails with a broken-pipe-derived error, and *every* call after that also fails the same way — the attached client is effectively dead until someone notices and restarts it, even though the remote came back up cleanly.

**Why this priority**: This is the harder-to-recover defect of the two — without it, any remote restart during a long attached session requires manual intervention, which defeats the purpose of attaching to a long-lived shared service in the first place.

**Independent Test**: Start a socket service and an attached MCP client against it. Make a successful call. Kill the socket service and start a fresh one on the same socket path (simulating a restart). Make another call through the same, still-running attached client process and assert it succeeds without the client process being restarted.

**Acceptance Scenarios**:

1. **Given** an attached client with an established connection, **When** the remote service process is killed and a new instance is started on the same socket path, **Then** the next call made through the attached client (without restarting the client process) reconnects transparently and succeeds.
2. **Given** the remote service is killed and **no** replacement is listening yet, **When** a call is attempted, **Then** the call fails with a clear, descriptive error (not a hang, not a silent no-op) and the client remains usable for a later retry once the remote comes back.
3. **Given** a connection break is detected **before** the request bytes were fully written to the socket (e.g. `write_all`/`flush` fails against a now-dead stream), **When** the client reconnects, **Then** it retries that same request exactly once over the new connection, since the request provably never reached any server.
4. **Given** a connection break is detected **after** the request was already fully written (the break happens while waiting for/reading the response — e.g. the remote closes the connection or exits mid-call), **When** this happens, **Then** the client does **not** automatically retry that call (its execution status on the remote side is unknown, and blind retry risks double-applying a non-idempotent write such as `knowledge_add_episode`); it fails that call with a clear "connection lost mid-call" error, but marks the connection dead so the *next* call reconnects fresh rather than reusing the broken stream.

---

### Edge Cases

- **Caller doesn't request progress.** `_progress_token` is only added to the forwarded request when the downstream MCP client asked for progress tracking. A caller that doesn't ask for progress on `knowledge_reprocess_entity_types` (or any of the other three long ops) still gets today's plain blocking behavior, still subject to the idle timeout — this issue only fixes the case where progress *was* requested.
- **Repeated/flapping remote restarts.** If the remote is still down (or goes down again) right after the one automatic retry, the call must fail cleanly rather than retry indefinitely or hang.
- **Reconnect must not break request/response correlation.** The existing request-ID counter and stale-response discard logic (a prior fix for PR #196) must continue to work correctly across a reconnect — a retried request gets a fresh line write on the new connection; nothing from the old, now-discarded connection should be able to satisfy it.
- **Idle-restart case.** If the remote restarts while no call is in flight, the dead stream should surface the failure (and trigger reconnect) on the *next* call attempt, not require any background health-check.
- **Non-idempotent writes are not double-applied.** Per User Story 2's acceptance scenario 4, the automatic retry is only safe (and therefore only performed) when the failure is provably pre-execution (write-time). This must hold for every attached-mode method, not just read-only ones — a call to a mutating method like `knowledge_add_episode`, `knowledge_canonicalize_relations`, etc. must never be silently retried once its request bytes may already have reached the (now-restarting) remote.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types`, and `knowledge_reprocess_relation_types` already emit progress and are already registered in `is_streaming_method` as of #210/PR #222 — no change required for these three under this issue; they are called out here only so acceptance testing has a complete, accurate list of the "long whole-graph ops" this issue is concerned with.
- **FR-002**: `knowledge_reprocess_entity_types` MUST emit `{"type":"progress"}` notifications during its run when the caller supplies a progress token, mirroring the pattern already established for `knowledge_reprocess_relation_types`.
- **FR-003**: `knowledge_reprocess_entity_types` MUST be added to `is_streaming_method` (`crates/service/src/mcp/tools.rs`), so that when a progress token is present, `wants_progress` is true and attached mode's per-read idle timer re-arms on each progress line instead of tripping during a long, otherwise-silent run.
- **FR-004**: Callers of `knowledge_reprocess_entity_types` that do **not** supply a progress token MUST see unchanged behavior (a plain blocking call, still subject to the idle timeout like any other non-streaming call).
- **FR-005**: The README's attached-mode / MCP-over-stdio documentation MUST describe `LCG_ATTACHED_CALL_TIMEOUT_MS` precisely as a **per-read-line idle timeout**, not a whole-call timeout, and MUST state that progress-emitting calls re-arm it on every progress line so a long streaming call is not bounded by it. It MUST also list which methods currently emit progress (the four whole-graph ops covered by FR-001/FR-002), so operators can tell which long-running calls are protected.
- **FR-006**: The README's attached-mode documentation MUST describe the reconnect behavior added by FR-007–FR-010: that a broken/dead connection is transparently re-dialed, under what conditions a failed call is automatically retried once, and what a caller should expect (a clean error, not a hang) when a reconnect attempt itself fails.
- **FR-007**: `AttachedBackend` MUST detect a failure while writing the outgoing request (`write_all` and/or `flush` failing on the current persistent connection — e.g. broken pipe from a dead/restarted remote).
- **FR-008**: On a write-time failure (FR-007), `AttachedBackend` MUST re-dial the originally-configured socket path and, if the re-dial succeeds, retry sending that same request exactly once over the new connection before falling back to normal response handling.
- **FR-009**: If the re-dial itself fails, or the retried write also fails, `AttachedBackend` MUST return a clear, structured error describing the reconnect failure — it MUST NOT hang and MUST NOT silently drop the call.
- **FR-010**: If the connection breaks **after** the request was already fully written — i.e. the failure surfaces while waiting for or reading the response (EOF, read error, or the existing idle-read timeout) — `AttachedBackend` MUST NOT automatically retry that in-flight call (its execution status on the remote is unknown). It MUST fail that call with a clear error, and MUST mark the connection as no longer usable so that the *next* call transparently re-dials rather than reusing the broken stream.
- **FR-011**: A successful reconnect (whether triggered by FR-008's immediate retry path or lazily by FR-010's next-call path) MUST leave `AttachedBackend` in a normal working state — subsequent calls behave exactly as they would against a freshly-started attached client, including request-ID sequencing and the existing stale-response-discard behavior (PR #196).
- **FR-012**: The single-connection, one-call-in-flight-at-a-time model (the `Mutex`-guarded connection) MUST be preserved — reconnect logic must not introduce concurrent use of the connection or a race between a reconnecting call and a subsequent call.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A test-scaled `knowledge_reprocess_entity_types` call over attached mode, run with a progress token and an idle timeout shorter than the run, completes successfully (no false timeout), with at least one progress notification observed during the run.
- **SC-002**: Existing coverage of `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types`, and `knowledge_reprocess_relation_types` staying within their idle timeout over attached mode continues to pass, unchanged, as regression coverage (per FR-001, no new behavior needed for these three).
- **SC-003**: After the remote service process is killed and a fresh instance is started on the same socket path, the next call issued through the same, still-running attached MCP client process succeeds without that client process being restarted.
- **SC-004**: A simulated write-time connection failure (request never reached the remote) is retried automatically and succeeds against the reconnected socket, with no duplicate side effect on the remote.
- **SC-005**: A simulated post-write connection failure (break during response read) fails that one call with a descriptive error, and a subsequent call on the same attached client process succeeds via a fresh reconnect — demonstrating the dead stream is not reused and no automatic retry occurred for the ambiguous case.
- **SC-006**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` all pass.

## Assumptions

- **A1.** The retry boundary described in FR-007–FR-010 (auto-retry only on write-time failure; no auto-retry once the request may have reached the remote) is the safety-driven default this spec adopts to avoid double-applying non-idempotent writes (e.g. `knowledge_add_episode`, `knowledge_canonicalize_relations`) on retry. This is a deliberate, spec-level design decision made during the Specify stage (not deferred to Research/Plan) because it directly affects correctness guarantees, not just implementation approach.
- **A2.** "Retries the call once" (per the original issue text) means exactly one automatic retry attempt after one reconnect; a second consecutive failure is surfaced to the caller rather than retried again or retried indefinitely.
- **A3.** Issue #210 (the dependency this issue was originally blocked on) has merged as PR #222 and is present on `main` as of this spec. `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types`, and `knowledge_reprocess_relation_types` are confirmed already streaming-enabled; this issue's remaining functional work is `knowledge_reprocess_entity_types` (progress wiring + streaming registration) plus the reconnect logic — the original issue's broader framing ("mark several ops streaming") is narrower in practice than as originally written.
- **A4.** No change to the socket wire protocol, standalone mode, or the canonicalize-semantics documentation (tracked separately per the original issue's Scope/Out section) is needed to satisfy this issue.
- **A5.** The existing `crates/service/tests/mcp_attached.rs` integration-test harness (spawning a real socket service and a real attached MCP client process, per `attached_mode_call_times_out_on_hung_remote_instead_of_blocking_forever` and `attached_mode_stale_response_after_timeout_is_not_misdelivered_to_next_call`) is a suitable and expected basis for this issue's new tests — killing/restarting a stub or real socket service to simulate remote restart, and using `LCG_ATTACHED_CALL_TIMEOUT_MS` to scale run/timeout durations down to test-appropriate lengths.

## Out of Scope

- The socket wire protocol itself.
- Standalone (non-attached) MCP mode.
- The canonicalize-relations semantics and recovery-scope documentation (handled by the separate #206 docs PR referenced in the original issue).
- Any change to `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types`, or `knowledge_reprocess_relation_types`'s progress/streaming behavior — already correct on `main` (see A3).
- General connection-pooling, multiple concurrent connections, or in-flight-call cancellation on disconnect — the fix preserves the existing single-connection, single-call-in-flight model.
- Retrying calls beyond the one automatic attempt described in FR-008/A2 (e.g. exponential backoff, multiple retries) — a caller that needs more resilience than one retry re-issues the call itself.

## Source References

- `crates/service/src/mcp/attached.rs` — `AttachedBackend`: `connect()` (single-dial startup, no reconnect), `call()` (write at `:118-124`, per-read idle timeout at `:133-150`), the request-ID/stale-response-discard logic from PR #196 (`:180-189`) that reconnect logic must not break.
- `crates/service/src/mcp/tools.rs` — `is_streaming_method` (currently 4 entries after #210/PR #222: `knowledge_rebuild_from_wal`, `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types`, `knowledge_reprocess_relation_types`); `knowledge_reprocess_entity_types` needs to be added as the 5th. Registry count/scope-bucket assertions will need updating alongside.
- `crates/service/src/mcp/server.rs:177` — `wants_progress = progress_token.is_some() && tools::is_streaming_method(spec.name)`, the gate this issue's FR-002/FR-003 need to pass for `reprocess_entity_types`.
- `crates/core/src/handlers.rs` — `handle_reprocess_entity_types` (currently takes no `progress_tx`, emits nothing); `handle_reprocess_relation_types` (the just-landed sibling that does thread `progress_tx` through, at the function calling `reprocess_relations::reprocess_relation_types`) is the pattern to mirror.
- `crates/core/src/reprocess_relations.rs` — the newly-merged (#210/PR #222) reference implementation of progress emission (`progress_tx: Option<UnboundedSender<Value>>`, periodic `{"type":"progress", ...}` sends) that `reprocess_entity_types`'s progress wiring should follow structurally.
- `crates/core/src/corrections.rs` — `ReprocessScope`, entity-side reprocess candidate selection used by `handle_reprocess_entity_types` today; the module that will need progress-callback plumbing added.
- `crates/service/tests/mcp_attached.rs` — existing attached-mode integration test harness (`spawn_socket_service`, `spawn_hanging_remote`, `spawn_stale_response_remote`, `McpClient`) this issue's new tests build on; in particular `attached_mode_call_times_out_on_hung_remote_instead_of_blocking_forever` and `attached_mode_stale_response_after_timeout_is_not_misdelivered_to_next_call` show the established pattern for simulating remote-side failure conditions.
- `README.md` — MCP-over-stdio transport section (`### DB-access modes`, `~L397-408`, current `LCG_ATTACHED_CALL_TIMEOUT_MS` description to expand) and `### Progress notifications` (`~L470-480`, current list of streaming methods to extend to include `reprocess_entity_types`).
- Issue #206 (Part B) — original report of both defects.
- Issue #210 / PR #222 — the dependency this issue was blocked on; now merged, establishing the `reprocess_relation_types` progress-emission pattern and already covering 3 of the 4 originally-named "long whole-graph ops."
- PR #196 — prior fix establishing the per-read idle-timeout and stale-response-discard behavior this issue builds on without breaking.
