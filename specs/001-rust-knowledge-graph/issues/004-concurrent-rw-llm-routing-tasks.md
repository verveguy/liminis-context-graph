---
description: "Task list for Issue #4 — Concurrent reader/writer with per-role LLM routing"
---

# Tasks: Issue #4 — Concurrent Reader/Writer with Per-Role LLM Routing

**Input**: `specs/001-rust-knowledge-graph/issues/004-concurrent-rw-llm-routing-plan.md`  
**Spec**: `specs/001-rust-knowledge-graph/issues/004-concurrent-rw-llm-routing-spec.md`

## Format: `[ID] [P?] [Story] [Constitution-tag?] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[US1]**: Reader/writer split (p95 search latency under concurrent extractions)
- **[US2]**: Per-role LLM routing with automatic fallback
- **[US3]**: Anthropic prompt-cache efficiency on Sonnet path
- **[ADAPTER]**: Touches LLM or embedding adapter; out-of-process boundary must be preserved
- **[HOT]**: Touches search/dedup hot path; bench in `benches/` required
- **[IPC]**: Touches IPC dispatch; parity corpus assertions must pass

---

## Phase 1: Foundation (Blocking Prerequisites)

**Purpose**: Core refactors and new types that all user stories depend on. No story work can begin until this phase is complete.

- [x] T001 Author `docs/adr/0002-reader-writer-split.md` documenting the `tokio::sync::RwLock` design (write guard only around DB commit spawn_blocking, not HTTP calls), SC-003 deferral rationale, and follow-up issue reference for the quantitative cache-hit baseline

- [x] T002 [P] File a follow-up GitHub issue titled "Establish prompt-caching baseline measurement in-repo" referencing SC-003 deferral in ADR-042; note the issue number in the ADR

- [x] T003 Complete `TelemetryEvent::LlmFallback` variant in `liminis-graph-core/src/telemetry.rs`: add `ts_ms: u64, role: String, primary_model: String, fallback_model: String, error_reason: String` fields; ensure `serde_json` serialization matches existing event shapes

- [x] T004 Refactor `liminis-graph-core/src/extractor.rs` [ADAPTER]:
  - Extract the `Extractor` trait with `fn extract<'a>(&'a self, body: &'a str, group_id: &'a str) -> futures::future::BoxFuture<'a, Result<ExtractionResult, Error>>`
  - Rename existing `Extractor` struct to `AnthropicExtractor`
  - Implement `Extractor` for `AnthropicExtractor`
  - Add `MockExtractor` (zero-latency, returns fixed `ExtractionResult` with 2 entities + 1 edge) implementing `Extractor`
  - Re-export `Extractor` trait from `lib.rs`
  - Update `lib.rs` exports: `pub use extractor::{Extractor, AnthropicExtractor, MockExtractor}`

- [x] T005 Create `liminis-graph-core/src/dedup_adapter.rs` [ADAPTER]:
  - Define `DedupAdapter` trait with `fn is_duplicate<'a>(&'a self, candidate: &'a EntityRow, incoming: &'a ExtractedEntity) -> futures::future::BoxFuture<'a, Result<bool, Error>>`
  - Implement `PassthroughDedupAdapter` (always returns `Ok(true)`)
  - Implement `LocalDedupAdapter { url: String, client: reqwest::Client }` with `from_env()` reading `LCG_DEDUP_ADAPTER_URL` (default `http://127.0.0.1:8767`)
  - `LocalDedupAdapter::is_duplicate` POSTs `{"candidate": {...}, "incoming": {...}}` and parses `{"is_duplicate": bool}`
  - Add `pub mod dedup_adapter` and re-exports to `lib.rs`

- [x] T006 Create `liminis-graph-core/src/app_state.rs`:
  - `AppState { db: Arc<Db>, embedder: Arc<Embedder>, extractor: Arc<dyn Extractor>, dedup: Arc<dyn DedupAdapter>, write_lock: Arc<tokio::sync::RwLock<()>>, sink: Arc<dyn TelemetrySink> }`
  - `AppState::from_env(sink, db)`: reads `LCG_DEDUP_LLM` to choose `PassthroughDedupAdapter` vs `LocalDedupAdapter`; always builds `LlmRouter` for extraction
  - Add `pub mod app_state` to `lib.rs`

- [x] T007 Create `liminis-graph-core/src/llm_router.rs` [ADAPTER]:
  - `LlmRouter { primary: AnthropicExtractor, fallback: Option<AnthropicExtractor>, primary_failed: AtomicBool, sink: Arc<dyn TelemetrySink> }`
  - `LlmRouter::from_env(sink)`: parses `LCG_EXTRACTION_LLM` on `:` to get primary and optional fallback model names; builds two `AnthropicExtractor` instances (same key, different models)
  - Implement `Extractor for LlmRouter`: try primary; on any error and `primary_failed` CAS from false→true, emit `TelemetryEvent::LlmFallback` + `eprintln!` exactly once; on subsequent calls with `primary_failed=true`, skip primary; if no fallback, propagate error
  - Add `pub mod llm_router` and re-export to `lib.rs`

**Checkpoint**: All new traits, adapters, and `AppState` compile. `cargo check` passes.

---

## Phase 2: User Story 1 — Reader/Writer Split (Priority: P1)

**Goal**: Search reads complete in ≤ 500 ms p95 even while ≥ 100 episodes are extracting.

**Independent test**: `cargo bench --bench concurrent_rw` passes with p95 ≤ 500 ms assertion.

### Implementation

- [x] T008 [US1] [IPC] Refactor `liminis-graph-core/src/handlers.rs` to take `Arc<AppState>` instead of four `Arc` args:
  - Update `dispatch(req: IpcRequest, state: Arc<AppState>) -> IpcResponse`
  - Internal `handle()` receives `&Arc<AppState>`
  - Write methods (`handle_add_episode`, `handle_delete_episode`, `handle_build_indices`): acquire `state.write_lock.write().await` before spawning `spawn_blocking`; move guard into closure
  - Read methods (all others): acquire `state.write_lock.read().await` before spawning `spawn_blocking`; move guard into closure
  - `knowledge_close` handler: no lock needed (returns immediately without DB access)
  - Pass `state.sink` where `sink` was previously a separate arg

- [x] T009 [US1] Refactor `liminis-graph-core/src/episode.rs` to split into three phases (AD-4):
  - Signature: `add_episode(state: Arc<AppState>, name, body, source, source_description, reference_time, group_id) -> Result<String, Error>`
  - Phase A: concurrent embed + extract, sequential name/fact embeddings (no lock)
  - Phase B: spawn_blocking for candidate fetching; async `DedupAdapter::is_duplicate` loop; build `Vec<DedupDecision>` (no lock)
  - Phase C: acquire `state.write_lock.write().await`; spawn_blocking for all DB commits (merge or insert entities, insert edges, episodic, MENTIONS); guard moved into closure

- [x] T010 [US1] [IPC] Update `liminis-graph/src/main.rs`:
  - Build `AppState::from_env(sink, db)` 
  - Thread `Arc<AppState>` into each spawned connection handler
  - Call `handlers::dispatch(req, Arc::clone(&state))` instead of the four-arg form

- [x] T011 [US1] [IPC] Update `liminis-graph-core/tests/ipc_parity.rs`:
  - Construct `AppState` directly (with `MockExtractor`, `PassthroughDedupAdapter`, test embedder/db)
  - All 11 IPC method assertions unchanged; only the `dispatch` call site changes
  - Verify: `cargo test --test ipc_parity` passes

- [x] T012 [US1] [HOT] Create `liminis-graph-core/benches/concurrent_rw.rs`:
  - Use `criterion::async_executor::TokioExecutor`
  - Setup: temp LadybugDB, init schema, insert ~100 `Entity` rows, build `AppState` with `MockExtractor` + `PassthroughDedupAdapter` + `MockEmbedder`
  - `MockEmbedder`: returns `vec![0.0f32; 768]` (zero-latency; lives in `benches/` not `src/`, or in `src/` as a `#[cfg(test)]` / `#[cfg(feature = "bench")]` type — prefer `benches/` local helper)
  - Bench group "concurrent_rw": spawn 100 `tokio::spawn` tasks each calling `episode::add_episode` with `MockExtractor`; simultaneously run 100 `search::hybrid_entity_search` calls; collect latencies; assert p95 ≤ 500 ms using criterion custom measurement
  - Verify: `cargo bench --bench concurrent_rw` compiles and asserts pass

**Checkpoint**: `cargo test --test ipc_parity` passes; `cargo bench --bench concurrent_rw` passes with p95 assertion.

---

## Phase 3: User Story 2 — Per-Role LLM Routing with Fallback (Priority: P1)

**Goal**: Extraction, dedup, and embedding independently configurable via env vars; primary failure logs once per session and falls back cleanly.

**Independent test**: `cargo test --test concurrent_rw_integration` passes (fallback scenario).

### Implementation

- [x] T013 [US2] [ADAPTER] Wire `AnthropicExtractor` 529/429 backoff (AD-7):
  - In `AnthropicExtractor::extract()`, after receiving a response with status 429 or 529, sleep `1s * 2^attempt` and retry up to 3 times total
  - Use `tokio::time::sleep` for delay
  - On 4th failure, return `Error::Http(...)` — no partial WAL write occurs since Phase C has not started

- [x] T014 [US2] [ADAPTER] Write integration tests in `liminis-graph-core/tests/concurrent_rw_integration.rs`:
  - Test 1 — **fallback once-per-session**: construct `LlmRouter` with a primary `AnthropicExtractor` pointing at an invalid URL; call `extract()` twice; assert `TelemetryEvent::LlmFallback` emitted exactly once (use `CaptureSink`); assert fallback `AnthropicExtractor` is called both times after first failure (can use a second invalid URL to force a known error shape and verify error propagation)
  - Test 2 — **PassthroughDedupAdapter default**: verify `AppState::from_env` with `LCG_DEDUP_LLM` unset produces `PassthroughDedupAdapter` behavior (is_duplicate always returns true for any candidate)
  - Test 3 — **write serialization**: two concurrent `add_episode` calls on the same `AppState` with `MockExtractor` complete without error (no lbug `-32000`); verify both episodes inserted

**Checkpoint**: `cargo test --test concurrent_rw_integration` passes.

---

## Phase 4: User Story 3 — Prompt-Cache Structural Changes (Priority: P2)

**Goal**: Sonnet extraction path sends correct caching headers; non-Sonnet paths do not.

**Independent test**: `cargo test --test ipc_parity` still passes; unit test for system message format passes.

### Implementation

- [x] T015 [US3] [ADAPTER] Add prompt-caching to `AnthropicExtractor::extract()` (AD-6):
  - Detect Sonnet path: `self.model.to_lowercase().contains("sonnet")`
  - When Sonnet: add `anthropic-beta: prompt-caching-2024-07-31` request header; serialize `system` as `[{"type": "text", "text": "<prompt>", "cache_control": {"type": "ephemeral"}}]` instead of a bare string
  - When non-Sonnet: keep existing bare-string `system` field; no caching header
  - Token usage parsing (`cache_read_input_tokens`, `cache_creation_input_tokens`) already in `emit_token_usage` — no change needed
  - Add unit test in `liminis-graph-core/tests/concurrent_rw_integration.rs` (or a new `extractor_unit.rs`): assert that a `AnthropicExtractor` with a Sonnet model name produces a request body with `system` as array + `cache_control` (mock the HTTP call with a local server or test the JSON construction without sending)

**Checkpoint**: Unit test for system message format passes; `cargo test` clean.

---

## Phase 5: Polish and Cross-Cutting

**Purpose**: Ensure all constitution tags satisfied, CI gates pass, follow-up artifacts filed.

- [x] T016 [P] Verify `cargo tree | grep -E "tch|candle|onnxruntime|mlx"` output is empty; document in PR description (Principle V SC-005)

- [x] T017 [P] Run full test suite: `cargo test --workspace` — all tests pass

- [x] T018 [P] Run `cargo bench --bench concurrent_rw` — p95 assertion passes; note bench output in PR description

- [x] T019 [P] Run `cargo bench --bench telemetry_overhead` — no regression vs. prior run; HOT-path compliance maintained

- [x] T020 [P] Confirm ADR-042 follow-up issue number is recorded in `docs/adr/0002-reader-writer-split.md` (from T002)

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (Foundation)**: No prior dependencies — start immediately
- **Phase 2 (US1)**: Depends on Phase 1 complete (needs `AppState`, `Extractor` trait, `DedupAdapter`)
- **Phase 3 (US2)**: Depends on Phase 1 complete; can run in parallel with Phase 2
- **Phase 4 (US3)**: Depends on T004 (`AnthropicExtractor` exists); can start once T004 is done
- **Phase 5 (Polish)**: Depends on Phases 2, 3, 4 complete

### Within Phase 1

- T001, T002, T003 can run in parallel (different files)
- T004 and T005 can run in parallel (different files)
- T006 depends on T004 + T005 (needs `Extractor` and `DedupAdapter` types)
- T007 depends on T004 (needs `AnthropicExtractor`)

### Within Phase 2

- T008 depends on T006 (`AppState`)
- T009 depends on T005 + T006 (`DedupAdapter`, `AppState`)
- T010 depends on T008 + T009
- T011 depends on T008
- T012 depends on T009 + T004 (`MockExtractor`)

---

## Constitution Compliance Summary

| Task | Tag | Gate |
|------|-----|------|
| T004, T005, T007, T013, T015 | [ADAPTER] | No ML-runtime crate in `Cargo.toml`; `cargo tree` check in T016 |
| T008, T010, T011 | [IPC] | `ipc_parity.rs` corpus assertions pass (T011) |
| T012 | [HOT] | `concurrent_rw.rs` bench passes with p95 ≤ 500 ms assertion (T012, T018) |

All `[ADAPTER]` tasks: out-of-process boundary preserved. `LocalDedupAdapter` and `AnthropicExtractor` call external HTTP endpoints only; `MockExtractor` is test-only with no model weights.
