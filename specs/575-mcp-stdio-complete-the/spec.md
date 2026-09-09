# Feature Specification: `--mcp-stdio`: complete the handshake without dialling, then dial `--connect` lazily per call

**Feature Branch**: `fabrik/issue-575`
**Created**: 2026-09-09
**Status**: Specified
**Input**: User description: "Implements the consumer report in #574 (Liminis OSS — Triage, Accepted). `lcg-service --mcp-stdio --connect <sock>` dials the daemon socket at startup and exits if it is missing or dead. That couples MCP-front liveness to daemon reachability, and the coupling is expensive because of how MCP clients behave on failure: cached-failure stranding (client caches a failed attach for ~15 minutes even after the daemon comes up) and orphaned live connections (a daemon restart that rebinds the socket permanently orphans the client's existing connection until a manual reconnect). Both were hit operating zen's `knowledge-reader` as a native MCP server in `.mcp.json`."

## Background

`liminis-context-graph --mcp-stdio --connect <sock>` runs the binary as an **attached** MCP-over-stdio front end (ADR-0035): rather than opening the graph database itself, it forwards every `tools/call` over a Unix domain socket to an already-running `liminis-context-graph` socket-service process. Today, the attached backend dials that socket **once, at startup**, before the MCP server ever answers `initialize`. If the socket is missing (daemon not started yet) or refuses the connection (daemon crashed, not yet up), the process exits before completing the MCP handshake at all.

This is expensive specifically because of how MCP clients behave on a failed attach, not just because of the exit itself:

- **Cached-failure stranding.** When an MCP client (observed with Claude Code, operating zen's `knowledge-reader` in `.mcp.json`) launches the server before the daemon exists — first install, post-reboot before the graph is hydrated, mid-restart of a paired sync process — the attach fails and the client caches that failure for roughly 15 minutes. The reader shows *disconnected* even after the daemon comes up in the meantime. The only remedies are a manual `/mcp` reconnect or starting a new client session.
- **Orphaned live connections.** A daemon restart that rebinds the socket (version bump, engine upgrade) permanently orphans the client's existing attached connection until a manual reconnect, even though nothing about the MCP front process itself failed.

The failure mode is worse than the raw description suggests: the front is *unavailable for roughly 15 minutes after the underlying cause (the daemon being down) is already fixed*, and nothing about the client's cached-disconnected state tells the operator why, or that a fix already landed.

**Prior art already in this codebase.** `crates/service/src/mcp/attached.rs` (issue #213) already implements per-call reconnect for a connection that goes dead *after* the initial dial: `AttachedBackend::call` treats a cached connection as lazily re-dialable (`stream: Mutex<Option<..>>`, `None` meaning "known-dead, redial on next use"), retries a write-phase failure once after redialing, and always invalidates the connection on any read-phase failure so the *next* call redials fresh rather than reusing a stream in an unknown framing state. Errors surfaced by a failed call already reach the client as an MCP tool result with `isError: true` (`ipc_response_to_call_tool_result` in `crates/service/src/mcp/server.rs`), not a protocol-level error — this machinery was built for the "connection was fine, then broke" case.

What this existing machinery does **not** cover is the case this issue is actually about: the **very first** dial, performed unconditionally by `AttachedBackend::connect` at process startup (`run_mcp_attached` in `crates/service/src/main.rs`), whose failure is propagated with `?` and exits the process before the MCP server loop — and therefore before `initialize` — ever runs. Research should confirm the current behavior against this description before scoping the implementation, since a meaningful part of the redial/error-shaping behavior this issue asks for may already exist and only need to be reached from a startup path that no longer dials eagerly, rather than built from scratch.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - MCP client registers healthy before the daemon exists (Priority: P1)

An operator configures `liminis-context-graph --mcp-stdio --connect <sock> --scope=read` as a native MCP server entry (e.g. in `.mcp.json`) that their MCP client launches automatically. The daemon at `<sock>` is not yet running — first install, post-reboot, or mid-restart of whatever brings it up.

**Why this priority**: This is the core complaint in #574 — a client that caches a failed attach for ~15 minutes is unusable for that entire window even after the daemon comes up, and is the primary motivation for the whole issue.

**Independent Test**: Launch `--mcp-stdio --connect /path/to/nonexistent.sock --scope=read` and send `initialize` and `tools/list`. Both must succeed and return the read-scope tool set, with no dial to the socket attempted.

**Acceptance Scenarios**:

1. **Given** the daemon socket does not exist, **When** the MCP client sends `initialize`, **Then** the front responds success immediately, without attempting to connect to the socket.
2. **Given** the daemon socket does not exist, **When** the MCP client sends `tools/list`, **Then** the front returns the tool set for the active `--scope`, served from the compiled-in registry, without attempting to connect to the socket.
3. **Given** the daemon socket does not exist, **When** the MCP client sends `tools/call` for any advertised tool, **Then** the front attempts to dial the socket, the dial fails, and the front returns a tool result with `isError: true` and a message that names the socket path and suggests starting the daemon — the process does not exit and no protocol-level error is returned.

---

### User Story 2 - Daemon comes up after the client is already attached (Priority: P1)

Following on from User Story 1: the daemon starts up sometime after the MCP client's session began (and after at least one failed `tools/call`).

**Why this priority**: This is the actual recovery path #574 complains is broken — the client must not need a manual reconnect or a new session once the underlying cause is fixed. It is explicitly called out in the issue's Acceptance section as not exercised by the basic repro and needing its own test.

**Independent Test**: Start the MCP front with `--connect` pointed at a socket path with no listener; send `initialize`, `tools/list`, and one `tools/call` (expect `isError: true`); then start the daemon at that path; send another `tools/call` for the same tool and confirm it succeeds — with no client-side reconnect action (no new `initialize`, no process restart).

**Acceptance Scenarios**:

1. **Given** an MCP session that has already completed the handshake against an unreachable socket, **When** the daemon becomes reachable at that socket path and the client sends a subsequent `tools/call`, **Then** the call dials the now-live socket and succeeds, using the same MCP session throughout.

---

### User Story 3 - Daemon restarts mid-session and rebinds the socket (Priority: P2)

An MCP client has an established, working attached connection. The daemon restarts (version bump, engine upgrade) and rebinds the socket, invalidating the front's existing connection.

**Why this priority**: This is the second failure mode named in #574 ("orphaned live connections") and is already partially addressed by the existing per-call reconnect logic in `attached.rs` (issue #213); this story's job is to confirm that behavior holds end-to-end for the specific "socket rebound by a daemon restart" trigger, not only for a dropped-but-not-rebound connection. It is real but slightly less acute than Stories 1/2 — the connection was working, so there's no cached-failure window; the client just needs the next call to quietly succeed instead of erroring forever.

**Independent Test**: Establish a live attached connection, complete one successful `tools/call`, restart the daemon process so the socket is rebound, then send another `tools/call` for the same tool and confirm it succeeds — with no client-side reconnect action.

**Acceptance Scenarios**:

1. **Given** a live attached connection that was working, **When** the daemon restarts and rebinds the socket, **Then** the next `tools/call` transparently re-dials and succeeds within the same MCP session, with no client-side reconnect action.

---

### User Story 4 - Operator opts back into fail-fast startup as a health gate (Priority: P3)

An operator who relies on today's "exits immediately if the daemon isn't up" behavior as an explicit startup health check (e.g. a supervisor that restarts the front until the daemon is ready) wants to keep that behavior rather than get the new lazy default.

**Why this priority**: Lower priority than the default behavior change, but explicitly requested in the issue's "Open decisions for Specify" — some operators may have built tooling around the current fail-fast semantics, and removing it outright without an escape hatch would be a regression for them.

**Independent Test**: Launch `--mcp-stdio --connect /path/to/nonexistent.sock --scope=read --connect-eager` and confirm the process exits with today's connection-failure error before completing the MCP handshake, byte-for-byte matching current (pre-this-issue) behavior.

**Acceptance Scenarios**:

1. **Given** `--connect-eager` (or the equivalent opt-in flag settled in Plan) is passed alongside `--connect`, **When** the daemon socket is unreachable at startup, **Then** the process exits immediately with the current error, and `initialize` is never reached — unchanged from today's behavior.
2. **Given** `--connect-eager` is passed and the daemon socket is reachable at startup, **When** the front starts, **Then** behavior is unchanged from today (dial succeeds, MCP server proceeds normally).

---

### Edge Cases

- **Repeated calls while the daemon stays down**: each `tools/call` independently attempts a dial and independently returns `isError: true` — no crash, no growing backoff, no state that makes the second failed call behave differently from the first.
- **Socket path exists but nothing is listening** vs. **socket path's parent directory doesn't exist at all**: both must be treated identically (both are "daemon not running" from the front's point of view) — consistent with `UnixStream::connect`'s existing immediate-failure behavior for both ENOENT and ECONNREFUSED.
- **`--scope` enforcement is unaffected by connection state**: a tool not in the active scope's advertised set is never listed and never callable, regardless of whether the daemon is up, down, or was never dialled — lazy connect changes *when* a socket is dialled, never *what* a given `--scope` is permitted to reach (FR-006).
- **`--connect-eager` combined with a reachable daemon** behaves identically to today's only supported path — this flag changes nothing about steady-state behavior, only the startup failure mode.
- **A `tools/call` that arrives before any dial has ever been attempted** (the very first call of the session) is not distinguished from a call after a known-dead connection — both take the same lazy-dial code path.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `initialize` MUST succeed without dialling the socket named by `--connect`, regardless of whether that socket is reachable.
- **FR-002**: `tools/list` MUST return the scope-appropriate tool set without dialling the socket named by `--connect`, served from the existing compiled-in `ToolSpec` registry (`crates/service/src/mcp/tools.rs`) — this issue does not add a runtime-refreshed tool list (see Out of Scope).
- **FR-003**: `tools/call` MUST dial the socket lazily on first use within a session, and MUST reuse a cached connection across subsequent calls while it remains live.
- **FR-004**: A `tools/call` against an unreachable socket MUST produce a tool result with `isError: true` and a message that names the configured socket path and suggests a remedy (e.g. starting the daemon). It MUST NOT exit the process and MUST NOT return a protocol-level (JSON-RPC) error.
- **FR-005**: A dropped or broken upstream connection MUST be transparently re-dialled on the next `tools/call`, within the same MCP session, with no action required from the client (no reconnect, no new `initialize`).
- **FR-006**: `--scope` MUST be enforced exactly as today. Lazy connect MUST NOT widen access — a given `--scope` value must advertise and permit exactly the same tool set it does today, independent of whether the socket has ever been successfully dialled.
- **FR-007**: The lazy-dial default (FR-001–FR-005) applies uniformly across all `--scope` values (`read`, `write`, `cypher`, `admin`) — connection timing is not scope-dependent. Only which tools are advertised/callable is scope-dependent (per FR-006), not when the underlying socket is dialled.
- **FR-008**: An opt-in flag MUST be available (suggested: `--connect-eager`; exact name may be finalized in Plan if it conflicts with an existing naming convention) that restores today's behavior exactly: dial the socket at startup, and exit with today's error if it is unreachable, without completing the MCP handshake. Absent this flag, lazy connect (FR-001–FR-005) is the default for `--mcp-stdio --connect`.
- **FR-009**: This issue does not change the compiled-in tool schema itself (names, descriptions, input schemas) — only when the attached socket is dialled and how an unreachable/dropped connection is surfaced to the MCP client.

### Key Entities

- **Attached connection cache**: the per-process cached Unix domain socket connection used by the attached MCP backend, with three effective states — never yet dialled, live, and known-dead-pending-redial. FR-001–FR-005 describe the required behavior of this cache's lifecycle; its concrete representation is a Research/Plan decision.
- **`--connect-eager` (name TBD)**: an opt-in CLI flag, valid only alongside `--mcp-stdio --connect`, that reverts to today's eager-dial-at-startup behavior (FR-008).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The exact repro in #574/#575 changes behavior: `lcg-service --mcp-stdio --connect /tmp/does-not-exist.sock --scope=read` followed by `initialize` succeeds (today it errors and exits); `tools/list` returns the read tool set; a subsequent `tools/call` returns `isError: true` with a message naming the socket path.
- **SC-002**: The recovery path is covered by an automated test: start with no daemon, complete the MCP handshake, bring the daemon up, and confirm a subsequent `tools/call` succeeds without any client-side reconnect (User Story 2).
- **SC-003**: The daemon-restart path is covered by an automated test: with a live attached connection, restart the daemon so the socket is rebound, and confirm the next `tools/call` re-dials and succeeds rather than failing for the rest of the session (User Story 3).
- **SC-004**: `--connect-eager` is covered by an automated test confirming byte-for-byte parity with today's pre-issue behavior when the daemon is unreachable at startup (User Story 4).
- **SC-005**: No regression in `--scope` enforcement: existing scope-gating tests continue to pass unmodified, and a read-scope front's advertised/callable tool set is unchanged by this issue.

## Assumptions

- This issue is scoped to the **attached** MCP front (`--mcp-stdio --connect <sock>`) only. Standalone `--mcp-stdio` (no `--connect`, opening the database in-process) already has its own startup-degrade handling for an unreachable *embedder* (ADR-0499) and is unaffected by this issue — the coupling being removed here is MCP-front-liveness-to-*daemon*-reachability, which only exists in attached mode.
- The existing per-call reconnect/retry machinery in `crates/service/src/mcp/attached.rs` (issue #213: lazy redial when a cached connection is `None`, write-phase retry-once, read-phase invalidate-on-failure) is assumed to already satisfy FR-003 and FR-005 for a connection that goes dead *after* an initial successful dial. Research must confirm this and identify precisely what changes are needed to also cover the *initial* dial (today performed eagerly by `AttachedBackend::connect` from `run_mcp_attached` in `main.rs`, whose failure exits the process before the MCP handshake), rather than assuming the whole feature is unbuilt.
- The exact wording of the FR-004 error message is illustrative, not contractual — it must name the socket path and be actionable, but the precise phrasing is an implementation/UX detail for Plan.
- The exact name of the FR-008 opt-in flag (`--connect-eager` suggested) is not locked; Plan may choose a different name if it better matches existing CLI conventions in `crates/service/src/cli.rs`, provided the behavior matches FR-008.
- No change to the socket wire protocol, the `AppState`/`handlers::dispatch` core, or standalone (non-`--connect`) MCP mode is anticipated. If Research finds otherwise, that is a signal to revisit scope, not silently expand it.

## Out of Scope

- Refreshing `tools/list` dynamically after a successful connect (raised as an open decision in the issue; resolved here as: keep the existing static compiled-in registry, since the read-scope tool set has been stable across the 0.13/0.14 line — revisit only if a future change makes the tool set genuinely dynamic).
- Any change to standalone `--mcp-stdio` (no `--connect`) behavior, including the embedder-unreachable degrade path (ADR-0499).
- The orac-sync daemon-reaping bug referenced in #574 — filed separately against GES/orac; this issue does not touch daemon lifecycle management.
- Any change to the socket-service (non-MCP) protocol or its own startup/health-check behavior.

## Source References

- `crates/service/src/mcp/attached.rs` — existing `AttachedBackend`, including issue #213's per-call reconnect/retry machinery.
- `crates/service/src/mcp/server.rs` — `ipc_response_to_call_tool_result`, which already maps a failed call to a `CallToolResult` with `isError: true`.
- `crates/service/src/main.rs` — `run_mcp_attached`, which currently performs the eager startup dial via `AttachedBackend::connect(..)?`.
- `crates/service/src/mcp/tools.rs` — the compiled-in `ToolSpec` registry backing `tools/list`.
- `crates/service/src/cli.rs` — existing flag parsing (`--mcp-stdio`, `--connect`, `--scope`, `--allow-remote-close`) that FR-008's new flag extends.
- ADR-0035 (`docs/adr/0035-mcp-stdio-transport.md`) — MCP-over-stdio transport architecture, including the `McpBackend` trait and attached-vs-standalone split.
- ADR-0499 (`docs/adr/0499-embedder-unreachable-retry-and-mcp-stdio-degrade.md`) — the analogous but distinct standalone-mode embedder-degrade decision; explicitly out of scope here but useful precedent for how a similar "don't strand the MCP client" problem was solved for a different subsystem.
- Issue #574 — the original consumer report this issue implements.
