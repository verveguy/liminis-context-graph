# Research Findings: Issue #6 — Telemetry and Operator Visibility

*Branch: fabrik/issue-6 | Surveyed: 2026-05-19*

---

## 1. Codebase State Summary

### Current telemetry: none

The codebase has no logging framework, no tracing, no metrics. The only stderr output is a single startup line in `liminis-graph/src/main.rs:45`:

```rust
eprintln!("liminis-graph: listening on {socket_path}");
```

No timing, no structured events, no token tracking anywhere.

### Workspace layout

Two crates:
- `liminis-graph-core` — library (all domain logic, will own `TelemetrySink` trait per Principle II)
- `liminis-graph` — binary (thin IPC wrapper, will wire the concrete sink)

---

## 2. Integration Points

### 2.1 IPC timing (`ipc_call` event)

**File**: `liminis-graph-core/src/handlers.rs:18–28`

The natural hook is wrapping the `handle()` call in `dispatch()`:

```rust
pub async fn dispatch(req: IpcRequest, db: Arc<Db>, embedder: Arc<Embedder>, extractor: Arc<Extractor>) -> IpcResponse {
    // ← insert Instant::now() here
    match handle(&req, db, embedder, extractor).await {
        Ok(result) => IpcResponse::ok(req.id, result),  // ← emit ipc_call event here
        Err(e) => IpcResponse::err(req.id, -32000, e.to_string()),  // ← or here on error
    }
}
```

This is `[HOT]` — fires on every IPC request. The sink must be non-blocking.

**Signature change required**: `dispatch()` needs a `sink: Arc<dyn TelemetrySink + Send + Sync>` 5th parameter. The binary passes it down from the accept loop.

### 2.2 Token usage (`token_usage` event)

**File**: `liminis-graph-core/src/extractor.rs:57–79`

The Anthropic API response already contains `usage` metadata that is currently discarded:

```rust
let resp: Value = self.client.post(ANTHROPIC_API_URL)
    // ...
    .json().await?;

// CURRENTLY DISCARDED — available as:
// resp["usage"]["input_tokens"]
// resp["usage"]["output_tokens"]
// resp["usage"]["cache_read_input_tokens"]     ← caching-audit field
// resp["usage"]["cache_creation_input_tokens"] ← caching-audit field
```

The `extract()` method needs to accept a sink parameter and emit a `token_usage` event after a successful API call. The `role` field value should be `"extraction"`.

There is no dedup LLM yet (FR-009 is unimplemented) — when it lands, its token emission role will be `"dedup"`. The telemetry types should be defined to accommodate it.

**Pricing calculation**: Must compute estimated cost from token counts × rate table. The Specify stage decided on a `LIMINIS_LLM_COST_TABLE_PATH` env-var override with a compiled-in default table. The default table must cover at minimum `claude-haiku-4-5-20251001` (the extraction default) with separate rates for input, output, cache-read, and cache-creation tokens.

### 2.3 Fallback events (`llm_fallback` event)

**Current state**: No fallback chain exists. FR-009 (primary→fallback LLM) is unimplemented.

The `llm_fallback` event type should be defined now so the Plan stage can document its shape, but there is no code to hook into. The Planner should note this as a stub: define the type, leave `emit()` calls as `// TODO: hook from FR-009 fallback chain`.

### 2.4 WAL instrumentation (`wal_append`, `wal_replay_complete` events)

**File**: `liminis-graph-core/src/episode.rs:152`

```rust
// Step 7: TODO issue #3: append WAL line
```

WAL is entirely unimplemented. The event types can be defined and documented now, but no hook exists until issue #3 lands. The Plan should define the types and mark the hook points as `// TODO: issue #3`.

### 2.5 Binary wiring

**File**: `liminis-graph/src/main.rs:47–89`

The accept loop already holds `Arc<Db>`, `Arc<Embedder>`, `Arc<Extractor>`. Adding `Arc<dyn TelemetrySink + Send + Sync>` follows the same pattern. Construction happens before `loop { ... }` at line 47, using env vars `LIMINIS_TELEMETRY_SOCKET` and stderr as fallback.

---

## 3. Architecture Decision: Sink Design

### Recommended: channel-backed Arc<dyn TelemetrySink>

```rust
// In liminis-graph-core/src/telemetry.rs
pub trait TelemetrySink: Send + Sync {
    fn emit(&self, event: TelemetryEvent);
}

pub enum TelemetryEvent {
    IpcCall { method: String, duration_ms: u64, success: bool, request_id: serde_json::Value },
    TokenUsage { role: String, model: String, input_tokens: u64, output_tokens: u64,
                 cache_read_tokens: u64, cache_creation_tokens: u64, estimated_cost_usd: f64 },
    LlmFallback { role: String, primary_model: String, fallback_model: String, error_reason: String },
    WalAppend { duration_us: u64, bytes: usize },
    WalReplayComplete { episodes_replayed: u64, duration_ms: u64, throughput_eps: f64 },
}
```

The binary implements `TelemetrySink` using a `tokio::sync::mpsc::UnboundedSender<TelemetryEvent>`. `emit()` calls `try_send()` — non-blocking, drops on overflow. A background task drains the receiver and serializes to JSONL on stderr (or a UNIX socket if `LIMINIS_TELEMETRY_SOCKET` is set).

A `NoopSink` implementation (all `emit()` calls are empty) enables zero-cost opt-out in library-only embeddings and simplifies testing.

### Why not `tracing`

The spec requires JSON Lines format with a fixed event taxonomy. `tracing` adds macro indirection, subscriber machinery, and would require a custom layer to produce the exact JSONL format specified. A thin trait is simpler, more readable, and produces the exact output the caching-audit consumer expects.

---

## 4. Signature Changes Required

| File | Change |
|------|--------|
| `handlers.rs` `dispatch()` | Add `sink: Arc<dyn TelemetrySink + Send + Sync>` parameter |
| `extractor.rs` `extract()` | Add `sink: Arc<dyn TelemetrySink + Send + Sync>` parameter (or store on `Extractor` struct) |
| `main.rs` accept loop | Construct sink, clone into each spawned task |
| `episode.rs` `add_episode()` | Thread sink through for future WAL hook (can leave stub) |

**Storing sink on struct vs. passing as parameter**: Storing on `Extractor` is cleaner for multi-call scenarios and avoids polluting every call site. `Embedder` does not need token tracking (no Anthropic token counting) but storing a no-op sink there maintains API symmetry if future adapters need it.

---

## 5. Performance Considerations (HOT PATH)

Two events fire on every `knowledge_add_episode` call:
- `ipc_call` (once, in `dispatch()`)
- `token_usage` (once, from `extractor.extract()`)

The `UnboundedSender::send()` is effectively a heap allocation + atomic push, microseconds at most. This is well within the 500 ms p95 budget for search latency. No synchronous I/O on the hot path.

Avoid using `std::sync::Mutex` in the sink — use the mpsc channel instead.

---

## 6. Missing Specify Artifact

The Specify stage summary references `specs/001-rust-knowledge-graph/issues/006-telemetry-spec.md` but this file does not exist in the worktree. The spec decisions are described in the Specify stage summary comment (see `.fabrik-context/stage-Specify.md`). The Plan stage should either request the file be recreated or proceed from the Specify summary directly.

---

## 7. Open Questions for Planner

1. **Should `Extractor` store the sink on the struct**, or should `extract()` accept it as a parameter? Storing on the struct avoids signature churn but couples the Extractor to telemetry.

2. **What is the compiled-in pricing table source?** Anthropic pricing changes. Should this be a static `include_str!()` from a TOML/JSON file in the repo, or a hardcoded `match` on model name? The `LIMINIS_LLM_COST_TABLE_PATH` override handles updates without recompile.

3. **How should tests verify telemetry?** A test `CaptureSink` (stores events in a `Vec<TelemetryEvent>` behind a `Mutex`) enables assertions. IPC parity tests (`tests/ipc_parity.rs`) can be extended to inject a `CaptureSink` and assert event counts/shapes.

4. **UNIX socket for telemetry**: The `LIMINIS_TELEMETRY_SOCKET` path implies a second listener task in the binary. This is additive complexity — confirm the Planner wants to implement it in this issue or defer to a follow-up.

---

## 8. Files to Create/Modify

| Action | Path |
|--------|------|
| **Create** | `liminis-graph-core/src/telemetry.rs` — trait + event enum + `NoopSink` |
| **Modify** | `liminis-graph-core/src/lib.rs` — add `pub mod telemetry` |
| **Modify** | `liminis-graph-core/src/handlers.rs` — timing + sink parameter |
| **Modify** | `liminis-graph-core/src/extractor.rs` — token usage emission |
| **Modify** | `liminis-graph/src/main.rs` — construct + thread sink |
| **Create** | `liminis-graph/src/sink.rs` — `StderrSink` / `SocketSink` impl |
| **Create** | `docs/telemetry.md` — event catalog (acceptance scenario 1 requirement) |
| **Create** | `assets/llm_pricing.json` (or `.toml`) — default pricing table |

No changes to WAL or fallback paths — those remain stubs with `// TODO: issue #3` and `// TODO: FR-009` comments.
