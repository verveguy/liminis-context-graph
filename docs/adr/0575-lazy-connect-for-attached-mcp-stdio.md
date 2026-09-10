# ADR-0575: Lazy-Dial the Attached `--connect` Socket by Default, With `--connect-eager` Opt-Out

**Date**: 2026-09-09
**Status**: Accepted

## Context

`liminis-context-graph --mcp-stdio --connect <sock>` runs as an attached MCP-over-stdio front end
(ADR-0035): rather than opening the graph database itself, it forwards every `tools/call` over a
Unix domain socket to an already-running `liminis-context-graph` socket-service process. Before
this issue, `run_mcp_attached` (`crates/service/src/main.rs`) called
`AttachedBackend::connect(&socket_path).await?` unconditionally at startup — the `?` exits the
process before the MCP server loop, and therefore before `initialize`, ever runs, if the daemon's
socket is missing or refuses the connection.

Issue #574 (a consumer report from operating zen's `knowledge-reader` as a native MCP server in
`.mcp.json`) surfaced why this coupling is expensive, not merely inconvenient: MCP clients don't
fail a bad attach cheaply.

- **Cached-failure stranding.** When the daemon isn't up yet when the client launches the front
  (first install, post-reboot, mid-restart of whatever brings the daemon up), the attach fails and
  the observed client caches that failure for roughly 15 minutes — the reader shows *disconnected*
  even after the daemon comes up in the meantime, with no remedy short of a manual `/mcp`
  reconnect or a new client session.
- **Orphaned live connections.** A daemon restart that rebinds the socket (version bump, engine
  upgrade) permanently orphans the client's existing attached connection until a manual reconnect,
  even though nothing about the MCP front process itself failed.

Both failure modes trace to the same root cause: MCP-front liveness was coupled to daemon
reachability at exactly one moment — process startup — that a client has no way to retry cheaply.

**The reconnect machinery to fix this already existed**, just not reachable from startup. Issue
#213 (ADR-0040) gave `AttachedBackend::call` a lazy-redial branch: a cached connection of `None` is
treated as "known-dead, dial before use," with a write-phase retry-once and a read-phase
invalidate-on-any-failure so the *next* call redials fresh. That branch does not distinguish "never
dialed yet" from "known-dead after a prior failure" — it was already structurally able to serve
this issue's "first dial deferred to first use" requirement. The only obstacle was that
`AttachedBackend`'s sole constructor, `connect()`, dialed unconditionally and failed via `?` before
a `Self` in the `None` state could ever exist outside the module.

## Decision

### A second, infallible constructor — `new_lazy` — alongside the unmodified `connect`

`AttachedBackend::new_lazy(socket_path: &str) -> Self` constructs the backend with
`stream: Mutex::new(None)` and performs no I/O. `run_mcp_attached` now takes an `eager: bool`
parameter and branches between the two constructors:

```
eager  → AttachedBackend::connect(&socket_path).await?   // byte-for-byte unchanged
!eager → AttachedBackend::new_lazy(&socket_path)          // new default; infallible
```

`connect()` itself is untouched (only a `read_call_timeout()` helper was extracted out of it and
reused by both constructors, a no-op refactor of its own behavior) — the eager path is not a
refactor of the lazy path with a flag flipped, it is the literal pre-#575 code, so the
`--connect-eager` parity guarantee (below) doesn't depend on two paths staying in sync by
convention.

Nothing downstream needed to change. `server.rs`'s `list_tools`/`get_info` never touch
`self.backend`'s connection state at all — `initialize` and `tools/list` were already served
entirely from the compiled-in `ToolSpec` registry (`crates/service/src/mcp/tools.rs`) and
`self.scopes`, independent of whether a socket had ever been dialed. `call()`'s existing
`guard.is_none()` branch (issue #213) already dials before use and already produces the
`isError: true` shape (`ipc_response_to_call_tool_result` in `server.rs`) via `dial()`'s existing
error message, which already names the socket path and suggests starting the daemon. Scope
enforcement (`is_tool_visible`) never consults connection state either. This is why the change is
concentrated in three files (`attached.rs`, `main.rs`, `cli.rs`) with no new subsystem and no
change to the wire protocol or `AppState`/`handlers::dispatch`.

### `--connect-eager`: an opt-out for operators using fail-fast-at-startup as a health gate

Some operators (e.g. a supervisor that restarts the front until the daemon is ready) rely on
today's immediate-exit-on-unreachable-socket behavior as an external health check. `--connect-eager`
restores it exactly: `run_mcp_attached(eager: true)` calls the unmodified `connect()` and the `?`
still exits before `initialize`.

Validation mirrors the existing `--allow-remote-close` flag rather than introducing a second shape
in the same hand-rolled parser (`cli.rs`): silently accepted (inert) without `--mcp-stdio`
(consistent with how `--connect`/`--scope` are validated only against `--mcp-stdio`, not against
each other), and a stderr notice (not a hard error) when present alongside `--mcp-stdio` but without
`--connect` — there is nothing for it to be eager about. Two flags that are both
"attached-mode-only, no-op boolean elsewhere" getting two different validation shapes would be an
unforced inconsistency for the next person reading `cli.rs`.

### What stays default: `tools/list` remains static

`tools/list` continues to be served from the compiled-in registry rather than refreshed after a
successful connect. The read-scope tool set has been stable across the 0.13/0.14 line; revisiting
this is deferred until a future change makes the tool set genuinely dynamic, not bundled into this
issue's connection-timing change.

## Consequences

- `initialize` and `tools/list` succeed against `--connect <unreachable-socket>` with no dial
  attempted (FR-001/FR-002) — the exact #574/#575 repro (`--connect
  /tmp/does-not-exist.sock --scope=read`) now completes the MCP handshake instead of exiting.
- A `tools/call` against a socket that has never been reachable and one against a socket that went
  dead mid-session are handled by the identical code path (`call()`'s `guard.is_none()` branch) —
  by construction, not by a new special case, satisfying the issue's edge case that a "first ever
  call" and a "call after a known-dead connection" must be indistinguishable.
- The daemon-restart-mid-session case (User Story 3, already exercised by
  `attached_mode_reconnects_after_remote_service_restart`, issue #213) required no code change and
  continues to pass unmodified — it was already going through the same reconnect branch this issue
  extends to the first dial.
- `--connect-eager` gives up the new default's client-friendliness in exchange for the old
  fail-fast startup signal; an operator must opt into it explicitly, and it has no effect without
  `--connect` (inert in standalone MCP mode, matching `--allow-remote-close`'s precedent).
- No wire-protocol change, no `AppState`/`handlers::dispatch` change, and no change to standalone
  (non-`--connect`) `--mcp-stdio` — that mode's own startup-degrade path for an unreachable
  *embedder* is ADR-0499's concern, a different subsystem (daemon-socket reachability vs. embedder
  reachability) solving an analogous but distinct problem.
