---
description: "Task list for Issue #6 — Telemetry and operator visibility"
---

# Tasks: Issue #6 — Telemetry and Operator Visibility

**Input**: `specs/001-rust-knowledge-graph/issues/006-telemetry-plan.md`  
**Branch**: `fabrik/issue-6`

---

## Phase 1: Core Library Types (Foundational — blocks all phases)

**Purpose**: Define the `TelemetrySink` trait, `TelemetryEvent` enum, `NoopSink`, and cost-calculation helper in the library crate. All other phases depend on these types existing.

- [ ] T001 [US6] Create `liminis-graph-core/src/telemetry.rs` with `TelemetryEvent` enum (five variants: `IpcCall`, `TokenUsage`, `LlmFallback`, `WalAppend`, `WalReplayComplete`), `TelemetrySink` trait (`fn emit(&self, event: TelemetryEvent)`), `NoopSink` struct implementing `TelemetrySink`, and `CaptureSink` struct (stores events in `Mutex<Vec<TelemetryEvent>>` — pub for use in tests across both crates)
- [ ] T002 [P] [US6] Add `pub mod telemetry` and re-export `TelemetrySink`, `TelemetryEvent`, `NoopSink` in `liminis-graph-core/src/lib.rs`
- [ ] T003 [P] [US6] Create `assets/llm_pricing.json` with compiled-in default pricing table for `claude-haiku-4-5-20251001` (fields: `input_per_mtok`, `output_per_mtok`, `cache_read_per_mtok`, `cache_creation_per_mtok`); add `cost_for_usage()` helper function in `telemetry.rs` that loads from `LIMINIS_LLM_COST_TABLE_PATH` env var if set, falling back to `include_str!()` embedded bytes; unknown models return `None`

**Checkpoint**: `cargo test -p liminis-graph-core` passes; `CaptureSink::events()` returns stored events.

---

## Phase 2: IPC Timing — dispatch() instrumentation [HOT]

**Purpose**: Wrap every IPC call in a timing measurement and emit an `IpcCall` event. This is a hot-path change (fires on every IPC request).

- [ ] T004 [HOT] [US6] Modify `liminis-graph-core/src/handlers.rs`: add `sink: Arc<dyn TelemetrySink + Send + Sync>` as the 5th parameter to `dispatch()`; capture `Instant::now()` before calling `handle()`; emit `TelemetryEvent::IpcCall { method, request_id, duration_ms, success }` after `handle()` returns (both `Ok` and `Err` branches)

**Checkpoint**: `cargo build -p liminis-graph-core` succeeds; `dispatch()` compile-time signature is updated.

---

## Phase 3: Token Usage — Extractor instrumentation [HOT]

**Purpose**: Capture and emit token usage from the Anthropic API response, including cache tokens needed by the 2026-04-30 caching audit.

- [ ] T005 [HOT] [US6] Modify `liminis-graph-core/src/extractor.rs`: add `sink: Arc<dyn TelemetrySink + Send + Sync>` field to the `Extractor` struct; update `Extractor::from_env()` to accept `sink: Arc<dyn TelemetrySink + Send + Sync>` as a parameter; after a successful Anthropic API response, read `resp["usage"]` fields (`input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`), compute `estimated_cost_usd` using `cost_for_usage()`, and emit `TelemetryEvent::TokenUsage { role: "extraction", model, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, estimated_cost_usd }`

**Checkpoint**: `cargo build -p liminis-graph-core` succeeds; all callers of `Extractor::from_env()` are updated.

---

## Phase 4: Stub Event Types for Future Hooks

**Purpose**: Plant `// TODO` comment hooks for WAL and fallback events so future issues have clear hook points without requiring implementation now.

- [ ] T006 [P] [US6] In `liminis-graph-core/src/episode.rs` at the `// Step 7: TODO issue #3: append WAL line` comment (line ~152), add a second comment line: `// TODO: issue #3 — emit TelemetryEvent::WalAppend { duration_us, bytes } via sink`
- [ ] T007 [P] [US6] In `liminis-graph-core/src/telemetry.rs`, add a doc comment on the `LlmFallback` variant: `// TODO: FR-009 — emit from primary→fallback transition in extractor chain`

**Checkpoint**: Comments are present; no functional change; `cargo test` still passes.

---

## Phase 5: Binary Sink Wiring

**Purpose**: Construct the concrete `StderrSink` in the binary and thread it through to both `Extractor` and `dispatch()`.

- [ ] T008 [US6] Create `liminis-graph/src/sink.rs` implementing `StderrSink`: uses `tokio::sync::mpsc::unbounded_channel`; `emit()` calls `try_send()` (non-blocking, drops on overflow); a `tokio::spawn` background task drains the receiver and writes each event as JSONL to stderr using `serde_json::to_string`; add `StderrSink::new() -> Self` and `StderrSink::start(self) -> Arc<StderrSinkHandle>` (or a simpler single constructor that spawns the task and returns `Arc<Self>`)
- [ ] T009 [US6] Modify `liminis-graph/src/main.rs`: declare `mod sink`; construct `Arc<StderrSink>` before the accept loop; pass it to `Extractor::from_env(Arc::clone(&sink))`; clone it into each `tokio::spawn` task and pass to `handlers::dispatch(..., Arc::clone(&sink))`; add `// TODO: LIMINIS_TELEMETRY_SOCKET — wire SocketSink here if env var is set` comment

**Checkpoint**: `cargo build` (binary crate) succeeds; running `./liminis-graph` and sending an IPC request writes JSONL telemetry events to stderr.

---

## Phase 6: Benchmarks (HOT-path compliance)

**Purpose**: Demonstrate that telemetry emission overhead on the hot path is negligible (< 10 µs per event) — required for `[HOT]` tag compliance per the constitution.

- [ ] T010 [P] [US6] Create `benches/telemetry_overhead.rs` using `criterion`; benchmark two scenarios: (a) `NoopSink::emit()` round-trip, (b) `StderrSink` (channel-send only, not draining) round-trip; assert documented in bench output that overhead is < 10 µs; add `[[bench]]` entry to root `Cargo.toml` if not already present

**Checkpoint**: `cargo bench --bench telemetry_overhead` compiles and runs; overhead shown in output.

---

## Phase 7: Tests

**Purpose**: Verify event shapes, counts, and cost calculations against the acceptance scenarios.

- [ ] T011 [P] [US6] Add unit tests in `liminis-graph-core/src/telemetry.rs` (behind `#[cfg(test)]`): test `cost_for_usage()` with known token counts and the compiled-in pricing table; test `cost_for_usage()` with an unknown model returns `None`; test `CaptureSink::emit()` stores events and `events()` returns them
- [ ] T012 [P] [US6] Add integration test in `liminis-graph-core/tests/` (new file `telemetry_ipc.rs` or extend existing test): inject a `CaptureSink` into `dispatch()` directly; call `dispatch()` with a `knowledge_find_entities` request; assert one `IpcCall` event is captured with `success: true` and `duration_ms >= 0`; this covers acceptance scenario 1 (timing events emitted per IPC call)

**Checkpoint**: `cargo test -p liminis-graph-core` passes including new tests.

---

## Phase 8: Documentation

**Purpose**: Create `docs/telemetry.md` — required by acceptance scenario 1 (event stream field shapes must match the doc).

- [ ] T013 [P] [US6] Create `docs/telemetry.md` documenting: overview of the telemetry system, transport (stderr JSONL default, future UNIX socket), all five event types with every field, type, and description, the `LIMINIS_LLM_COST_TABLE_PATH` env var format, the `LIMINIS_TELEMETRY_SOCKET` env var (future, not yet implemented), and a sample JSONL output block covering all five event types

**Checkpoint**: `docs/telemetry.md` exists and covers every field in `TelemetryEvent`.

---

## Dependencies & Execution Order

```
Phase 1 (T001, T002, T003) — foundational, no deps
  ↓
Phase 2 (T004) — depends on T001 (TelemetryEvent type)
Phase 3 (T005) — depends on T001 (TelemetrySink trait + cost calc from T003)
Phase 4 (T006, T007) — depends on T001 (TelemetryEvent variants defined)
  [Phases 2, 3, 4 can proceed in parallel after Phase 1]
  ↓
Phase 5 (T008, T009) — depends on T004, T005 (updated signatures)
  ↓
Phase 6 (T010) — depends on T008 (StderrSink available)
Phase 7 (T011, T012) — depends on T004, T005 (dispatch/extractor updated)
Phase 8 (T013) — depends on T001 (event schema finalized)
  [Phases 6, 7, 8 can proceed in parallel after Phase 5]
```

### Within-phase parallel opportunities

- T001 and T002 and T003 can be done in one pass (same file/adjacent files)
- T006 and T007 are independent edits
- T011, T012, T013 are independent

---

## Constitution Compliance

| Task | Tag | Gate requirement |
|------|-----|-----------------|
| T004 | `[HOT]` | Bench in `benches/` (T010) REQUIRED before merge |
| T005 | `[HOT]` | Bench in `benches/` (T010) REQUIRED before merge |
| T010 | bench | Must run without error and show overhead < 10 µs |

No `[IPC]`, `[WAL]`, `[LDB]`, or `[ADAPTER]` tags apply — IPC wire format is unchanged, WAL is untouched, no DB driver changes, no ML runtime added.
