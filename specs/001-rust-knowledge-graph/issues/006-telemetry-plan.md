# Implementation Plan: Issue #6 — Telemetry and Operator Visibility

**Branch**: `fabrik/issue-6` | **Date**: 2026-05-19 | **Spec**: `.fabrik-context/stage-Specify.md`  
**Input**: Research findings in `specs/001-rust-knowledge-graph/issues/006-telemetry-research.md`

---

## Summary

Add structured JSON Lines telemetry emission to the Rust service: per-call IPC timing, token usage with estimated cost, stub event types for WAL and fallback paths (deferred to issues #3 and FR-009), and a `docs/telemetry.md` event catalog. The `TelemetrySink` trait lives in the library crate (`liminis-graph-core`) satisfying Principle II; the binary wires a channel-backed `StderrSink`. UNIX socket transport is deferred to a follow-up.

---

## Technical Context

**Language/Version**: Rust (stable, edition 2021)  
**Primary Dependencies**: `tokio` (async runtime), `serde_json` (JSONL serialization), `reqwest` (Anthropic client)  
**Storage**: LadybugDB (unchanged)  
**Testing**: `cargo test`; `CaptureSink` test helper for event assertions  
**Target Platform**: macOS / Linux server  
**Project Type**: Library (`liminis-graph-core`) + binary (`liminis-graph`)  
**Performance Goals**: Telemetry emission MUST NOT add measurable latency to p95 search (≤ 500 ms budget); channel-send is non-blocking  
**Constraints**: No synchronous I/O on the hot path; `NoopSink` provides zero-cost opt-out for library consumers  
**Scale/Scope**: Two Rust crates; small surface area — no new dependencies required

---

## Architecture Decisions

### AD-T1: TelemetrySink trait in the library, StderrSink in the binary

The `TelemetrySink` trait + `NoopSink` + `TelemetryEvent` enum live in `liminis-graph-core/src/telemetry.rs` (Principle II: library API is the source of truth). The concrete `StderrSink` that does channel I/O lives in `liminis-graph/src/sink.rs`. This keeps the library free of tokio runtime dependencies that would conflict with embedding scenarios.

### AD-T2: Sink stored on Extractor struct, not passed to extract()

`Extractor` stores `sink: Arc<dyn TelemetrySink + Send + Sync>` on the struct. `extract()` retains its current two-argument signature. This avoids propagating an extra parameter through `episode::add_episode()` and all its callers. `Extractor::from_env()` gains a `sink` parameter:

```rust
pub fn from_env(sink: Arc<dyn TelemetrySink + Send + Sync>) -> Self
```

The binary constructs the sink before constructing the `Extractor`.

### AD-T3: dispatch() gains one sink parameter for IPC timing

`handlers::dispatch()` adds a `sink: Arc<dyn TelemetrySink + Send + Sync>` parameter. The timing wrap lives here (not inside `handle()`) so all 11 IPC methods are covered by one instrumentation point:

```rust
pub async fn dispatch(
    req: IpcRequest,
    db: Arc<Db>,
    embedder: Arc<Embedder>,
    extractor: Arc<Extractor>,
    sink: Arc<dyn TelemetrySink + Send + Sync>,
) -> IpcResponse
```

`handle()` does not need the sink — timing and error status are captured by observing the `Ok`/`Err` returned to `dispatch()`.

### AD-T4: Compiled-in pricing table via include_str!

`assets/llm_pricing.json` is compiled into the binary via `include_str!()`. Override at runtime with `LIMINIS_LLM_COST_TABLE_PATH`. Schema:

```json
{
  "claude-haiku-4-5-20251001": {
    "input_per_mtok": 0.80,
    "output_per_mtok": 4.00,
    "cache_read_per_mtok": 0.08,
    "cache_creation_per_mtok": 1.00
  }
}
```

Unknown models yield `estimated_cost_usd: null` (JSON null) rather than erroring.

### AD-T5: StderrSink is channel-backed; UNIX socket deferred

`StderrSink` uses a `tokio::sync::mpsc::unbounded_channel`. The `emit()` method calls `try_send()` — non-blocking, drops events on overflow (full-channel pressure is not expected). A background `tokio::task` drains the channel and writes JSONL to stderr.

UNIX socket support (`LIMINIS_TELEMETRY_SOCKET`) is out of scope for this issue — a `// TODO: LIMINIS_TELEMETRY_SOCKET` comment marks the wiring point in `main.rs`.

### AD-T6: WAL and fallback event types are defined but not emitted

All five event types (`IpcCall`, `TokenUsage`, `LlmFallback`, `WalAppend`, `WalReplayComplete`) are defined in the enum now. Emit call sites for `LlmFallback` are stubs (`// TODO: FR-009`); those for `WalAppend`/`WalReplayComplete` are stubs (`// TODO: issue #3`). This satisfies acceptance scenario 3 (event *types* available) while WAL and fallback remain unimplemented.

---

## Event Type Schemas (JSONL format)

Each event is one JSON object per line. All events share a `"type"` discriminant and `"ts_ms"` (Unix epoch milliseconds).

### ipc_call
```json
{"type":"ipc_call","ts_ms":1716100000000,"method":"knowledge_add_episode","request_id":1,"duration_ms":42,"success":true}
```

### token_usage
```json
{"type":"token_usage","ts_ms":1716100000001,"role":"extraction","model":"claude-haiku-4-5-20251001",
 "input_tokens":512,"output_tokens":128,"cache_read_tokens":384,"cache_creation_tokens":0,
 "estimated_cost_usd":0.000512}
```

### llm_fallback
```json
{"type":"llm_fallback","ts_ms":1716100000002,"role":"extraction","primary_model":"claude-sonnet-4-6",
 "fallback_model":"claude-haiku-4-5-20251001","error_reason":"rate_limit_exceeded"}
```

### wal_append
```json
{"type":"wal_append","ts_ms":1716100000003,"duration_us":180,"bytes":1024}
```

### wal_replay_complete
```json
{"type":"wal_replay_complete","ts_ms":1716100000004,"episodes_replayed":42,"duration_ms":380,"throughput_eps":110.5}
```

---

## Constitution Check

### Principle gates

- **I. IPC Parity** — `dispatch()` gains a Rust parameter; the Unix-socket JSON-RPC wire format is unchanged. No parity test needed for this change. **PASS**
- **II. Library and Binary are Peers** — `TelemetrySink` trait lives in `liminis-graph-core`. All telemetry types are reachable via the library API. **PASS**
- **III. LadybugDB Only** — no storage changes. **PASS (N/A)**
- **IV. WAL is Authoritative** — WAL hooks are stubs only; no WAL format change. **PASS (N/A)**
- **V. LLM/Embedding adapters out-of-process** — no ML runtime added to `Cargo.toml`. **PASS**

### Performance budget gates

- `ipc_call` emit: non-blocking `UnboundedSender::send()` — heap alloc + atomic push, < 1 µs. Does not affect the 500 ms p95 search budget.
- `token_usage` emit: same path, only after Anthropic API returns. Not on the latency-critical search path.
- A microbench (`benches/telemetry_overhead.rs`) will measure the overhead of a `NoopSink` and a `StderrSink` emit call to confirm < 10 µs per event. Required for `[HOT]` tag compliance.

### Workflow gates

- Spec: `.fabrik-context/stage-Specify.md` (authoritative; file-on-disk is missing but content is in stage summary). ✓
- No IPC wire format change → no IPC parity test required for this issue. ✓
- `[HOT]` tags on T003, T004 → bench in `benches/` required (T009). ✓
- WAL/fallback stubs → TDD not mandatory (no WAL serialization logic added). ✓

---

## Project Structure

### Files created or modified

```text
liminis-graph-core/
├── src/
│   ├── telemetry.rs          [CREATE] TelemetrySink trait, TelemetryEvent enum, NoopSink, cost calc
│   ├── extractor.rs          [MODIFY] store sink on struct, emit token_usage after API call
│   ├── handlers.rs           [MODIFY] add sink param to dispatch(), emit ipc_call
│   └── lib.rs                [MODIFY] add `pub mod telemetry`

liminis-graph/
├── src/
│   ├── sink.rs               [CREATE] StderrSink (channel-backed tokio task)
│   └── main.rs               [MODIFY] construct sink, thread to Extractor + dispatch()

assets/
└── llm_pricing.json          [CREATE] compiled-in default pricing table

docs/
└── telemetry.md              [CREATE] event catalog for acceptance scenario 1

benches/
└── telemetry_overhead.rs     [CREATE] HOT-path overhead microbench
```

---

## Complexity Tracking

> No constitution violations — no entries required.

---

## Open Questions Resolved

| # | Question | Decision |
|---|----------|----------|
| 1 | Sink on struct vs. parameter | Store on `Extractor` struct; `dispatch()` gets one new parameter |
| 2 | Pricing table source | `include_str!("../../assets/llm_pricing.json")` + `LIMINIS_LLM_COST_TABLE_PATH` override |
| 3 | Test strategy | `CaptureSink` in `liminis-graph-core/src/telemetry.rs` (pub for tests, gated behind `#[cfg(test)]` or test feature); handler tests extend to assert event emission |
| 4 | UNIX socket | Deferred — stderr JSONL only in this issue; `// TODO: LIMINIS_TELEMETRY_SOCKET` comment left in `main.rs` |
