# ADR-0035: MCP-over-stdio Transport Architecture

**Status**: Accepted
**Date**: 2026-07-21
**Issues**: #195 (executes the `--mcp-stdio`/`rmcp` item deferred by `specs/120-oss-launch-scaffolding-license`)

## Context

The OSS `liminis-context-graph` binary exposed its graph only over a Unix-socket JSON-RPC 2.0
protocol. MCP wrapping lived only in the closed Liminis Electron app, as TypeScript proxy
servers reachable only from inside that app. This issue adds a native Rust MCP-over-stdio
transport (`rmcp`, the official Rust MCP SDK) directly to the binary, so any MCP client can use
the graph with no Electron/Node dependency. The core dispatch in `crates/core/src/handlers.rs`
is explicitly out of scope — this is new transport code in `crates/service` only.

## Decisions

### 1. Hand-rolled `rmcp::ServerHandler`, not the `#[tool_router]` macro

`rmcp`'s idiomatic tool registration is a macro pair (`#[tool_router]`/`#[tool_handler]`) that
generates a **static** tool list at compile time. The issue's scope-gating requirement — a
`--scope` flag that changes what `tools/list` advertises at runtime, plus a mode-dependent rule
for `knowledge_close`'s visibility — cannot be expressed through a compile-time-fixed list.
`crates/service/src/mcp/server.rs` implements `ServerHandler::list_tools`/`call_tool`/`get_tool`
directly against a data-driven registry instead, filtering it by the active scopes and by
attached-mode/`--allow-remote-close` state on every call. This costs nothing extra: none of the
33 existing `knowledge_*` handlers have typed argument structs today, so the macro's main
benefit (deriving a schema from a Rust type) wasn't available for free anyway.

### 2. Hand-authored `json!` schemas in a static table, not 33 `schemars` structs

`crates/service/src/mcp/tools.rs` defines a `ToolSpec { name, description, scope, input_schema:
fn() -> Value }` registry — one entry per `knowledge_*` method, with the JSON Schema written as a
plain `serde_json::json!` literal. This is the single source of truth the tool surface is derived
from; there is no second, hand-maintained schema anywhere else. The alternative — one
`schemars::JsonSchema`-deriving struct per tool — would add a `schemars` dependency and 33 new
types that nothing else in the codebase would ever construct, since tool-call arguments flow
straight through to `handlers::dispatch` as a raw `serde_json::Value` (see Decision 3); there is
no typed deserialization step to justify the structs.

### 3. MCP tool name = IPC method name, verbatim; no duplicated graph logic

Every tool in the registry is named identically to the `knowledge_*` dispatch method it invokes
(e.g. the `knowledge_find_entities` tool calls the `knowledge_find_entities` IPC method). A
`tools/call` becomes `IpcRequest { method: <tool name>, params: <tool arguments>, .. }`, passed
directly to `lcg_core::handlers::dispatch`. This keeps the registry trivially auditable against
`handlers.rs`'s `match` arms and makes literal (not just aspirational) the requirement that no
graph logic is duplicated in the transport shell. `health_check` is a socket-protocol convenience,
not a `knowledge_*` method, and is deliberately not part of the MCP tool surface.

### 4. One `McpBackend` trait, two implementations

`crates/service/src/mcp/backend.rs` defines:

```rust
pub trait McpBackend: Send + Sync + 'static {
    async fn call(&self, method: &str, params: Value, progress: Option<UnboundedSender<Value>>) -> IpcResponse;
    fn is_attached(&self) -> bool;
}
```

- **`StandaloneBackend`** (same file): wraps `Arc<AppState>` and calls
  `handlers::dispatch(req, state, progress_tx)` in-process — no socket is bound. This process
  opened the `.lcg` database itself, via the same bootstrap sequence the socket service uses
  (see Decision 6).
- **`AttachedBackend`** (`crates/service/src/mcp/attached.rs`): a new async Unix-socket JSON-RPC
  client. It never opens the database; it forwards each call as a line of JSON over a
  `--connect <path>` socket to an already-running service, so an MCP client can coexist with the
  Liminis app (or any other socket-service instance) without contending for lbug's
  single-writer lock.

`crates/service/src/mcp/server.rs`'s `LcgMcpServer<B: McpBackend>` is generic over the trait and
written once; `main.rs` constructs `LcgMcpServer<StandaloneBackend>` or
`LcgMcpServer<AttachedBackend>` depending on whether `--connect` was passed, and each branch is
independently monomorphized — no `dyn` trait object or boxed futures were needed.

`AttachedBackend` serializes every call on one persistent connection, behind a
`tokio::sync::Mutex` held for the full write-then-read-until-terminal-response cycle of a single
call. The socket wire protocol has no request-ID demultiplexing for interleaved progress/response
lines (see `main.rs`'s `handle_connection`), so with only one call ever in flight on a given
connection, any interleaved `{"type":"progress"}` line unambiguously belongs to the current call.
This matches the protocol's own existing single-flight-per-connection behavior — not a
regression, just a client-side mirror of it.

### 5. `knowledge_close` semantics differ by mode, and the difference is load-bearing

- **Standalone**: `knowledge_close` is always advertised under `admin` scope, regardless of
  `--allow-remote-close`. Calling it shuts down only this MCP process's own DB connection — it
  is a `Some(CancellationToken)` field on `LcgMcpServer` (`shutdown_ct`) that `call_tool` cancels
  on a successful close, which unwinds the MCP serve loop so `main.rs` can run the same
  cancel/drain/`drop(AppState)` sequence the socket service's tail uses (WAL checkpoint via
  `Db`'s destructor, telemetry drain).
- **Attached**: without `--allow-remote-close`, `knowledge_close` is omitted from `tools/list`
  **entirely** — not merely rejected when called — because calling it would shut down the
  *remote* service (potentially the Liminis app's), which is a real footgun to leave silently
  reachable. With `--allow-remote-close`, it is advertised and forwards the close to the remote;
  the attached MCP process itself does not exit (`shutdown_ct: None` in this mode) — a further
  call after that surfaces a normal FR-008 connection-failure tool error, not a crash.

### 6. `main.rs`'s bootstrap is now a shared, reusable function

`bootstrap_app_state` (`main.rs`) extracts the pre-existing socket-service startup sequence
(embedder transport resolution/probe → `Db::open` + self-recovery per ADR-0009 →
`AppState::from_env`) into a function called identically by the socket path and by standalone
MCP mode. This was the highest-blast-radius change in this issue: it's a pure extraction with no
logic change, guarded by running the full existing test suite (`cargo test`) to confirm no
regression to the socket-service path. Attached mode never calls this function at all — it skips
migration, embedder resolution, and DB open entirely, since it never touches the workspace
filesystem.

### 7. `main()` owns the `Runtime` directly and bounds its blocking-pool drain, instead of relying on `#[tokio::main]`'s indefinite default

`tokio::io::stdin()` is implemented as a `spawn_blocking` thread performing a raw, uncancellable
blocking `read()` syscall; it only unblocks on EOF (the MCP client closing its end of the pipe)
or process exit. When shutdown is triggered by `knowledge_close` or a signal (SIGTERM/SIGINT)
rather than the client closing stdin, that thread is typically still parked in `read()`.

ADR-0017 already established (for a different reason — a DB-corruption race in
`knowledge_build_indices`) that this binary must **not** call `std::process::exit` right after
its async shutdown logic finishes: doing so can race ahead of an in-flight `spawn_blocking` task
that still holds an `Arc<Db>` clone, skipping its WAL checkpoint. ADR-0017's fix was to return
normally from `#[tokio::main]`'s `async fn main()` and let the tokio runtime's default `Drop`
impl wait — indefinitely — for the blocking pool to fully drain before the process exits. That
default behavior is exactly what makes standalone MCP mode hang forever after `knowledge_close`:
the runtime drop waits just as patiently for the permanently-parked, uncancellable stdin reader
thread as it does for a legitimate short-lived index-build task, and the former never finishes.

Neither "call `process::exit` immediately" (unsafe per ADR-0017) nor "let `Drop` wait forever"
(hangs on stdin) is acceptable, so `main()` no longer uses the `#[tokio::main]` macro. It builds
and owns a `tokio::runtime::Runtime` directly, calls `block_on(async_main(..))`, and then calls
`runtime.shutdown_timeout(Duration::from_millis(shutdown_timeout_ms))` — the exact mechanism
ADR-0017's own "Consequences" section names as the sanctioned way to add a bounded exit later
("use `Runtime::shutdown_timeout`... not an immediate `exit(0)` that bypasses the drain").
`shutdown_timeout` still gives any real in-flight blocking work its existing
`LCG_SHUTDOWN_TIMEOUT_MS` window (default 5s) to finish and release its `Arc<Db>` clone — so
ADR-0017's fix is preserved — but it no longer waits unboundedly past that window for a thread
that will never return on its own. This applies uniformly to both `run_socket_service` and
`run_mcp_standalone`/`run_mcp_attached`; the socket path's behavior is unchanged in the normal
case (its blocking work already finishes in milliseconds, well inside the bound) and gains the
same formerly-theoretical unbounded-hang protection ADR-0017's own risk callout accepted as a
future improvement.

`run_mcp_standalone` also installs its own SIGTERM/SIGINT handlers (mirroring
`run_socket_service`'s) that cancel the same `shutdown_ct` used by the `knowledge_close` path —
without this, an external kill/supervisor stop would hit the default signal disposition and skip
the WAL checkpoint entirely, which is exactly the corruption risk `run_socket_service`'s existing
signal handling exists to prevent. `stdin` EOF (the client disconnecting normally) needs no
special handling — `rmcp`'s serve loop already treats it as a normal `QuitReason::Closed`.

### 8. No `clap` adoption

`crates/service/src/cli.rs` extends the pre-existing hand-rolled argv scan into a small, pure,
directly-unit-tested `parse_args(&[String]) -> Result<CliMode, String>`. Six flags total
(`--mcp-stdio`, `--connect`, `--scope`, `--allow-remote-close`, plus the pre-existing
`--embedder-uds`/`--embedder-http`) didn't clear the bar for a new dependency, and the existing
pattern already had a test-friendly seam once extracted into its own function.

## Adding a new `knowledge_*` method later

Adding a new dispatch method to `handlers.rs` also requires a new `ToolSpec` entry in
`crates/service/src/mcp/tools.rs` with the correct scope bucket (see the table in `CLAUDE.md`
and the README's MCP section) — the MCP surface is not automatically derived by reflection.

## Consequences

- New workspace dependency: `rmcp = { version = "2.2.0", features = ["server", "transport-io"] }`
  (default features disabled to skip `macros`/`base64`, which the hand-rolled `ServerHandler`
  doesn't need).
- Nine new files under `crates/service/src/{cli.rs,mcp/}`; `main.rs`'s bootstrap sequence was
  refactored (not just extended) — reviewed against the full existing test suite for regressions.
- SC-006 ("verified against the app's zod tool defs") is a manual, human-driven verification
  step, not something this pipeline automated: the referenced TypeScript files
  (`liminis-app/src/main/mcp-providers/*.ts`) live in a separate, closed-source repository not
  reachable from this environment, per the issue's own Assumptions fallback. Tool descriptions
  and schemas here were authored from the issue's FRs and from `handlers.rs`'s actual parameter
  extraction.
