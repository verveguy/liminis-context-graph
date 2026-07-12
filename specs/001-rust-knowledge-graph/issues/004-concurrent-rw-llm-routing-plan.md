# Implementation Plan: Issue #4 — Concurrent Reader/Writer with Per-Role LLM Routing

**Branch**: `fabrik/issue-4` | **Date**: 2026-05-19 | **Spec**: `specs/001-rust-knowledge-graph/issues/004-concurrent-rw-llm-routing-spec.md`

## Summary

Refactor `liminis-graph-core` to (1) enforce a reader/writer split using `tokio::sync::RwLock` so that long-running LLM extractions do not block search reads, (2) introduce per-role LLM routing via env vars with a primary→fallback chain and once-per-session failure logging, (3) add a `DedupAdapter` trait for out-of-process LLM dedup verification, (4) apply Anthropic prompt-caching headers on the Sonnet extraction path, and (5) add a `benches/concurrent_rw.rs` bench proving the p95 search-latency budget (≤ 500 ms) under ≥ 100 concurrent extraction tasks. The IPC surface is unchanged.

---

## Technical Context

**Language/Version**: Rust stable (≥ 1.75)  
**Primary Dependencies**: `lbug = "=0.16.1"`, `tokio` 1.x full, `reqwest` 0.12, `serde_json` 1, `futures` 0.3 (for `BoxFuture`), `criterion` 0.5  
**Storage**: LadybugDB (single-file, `lbug`); all adapters are out-of-process (HTTP or subprocess)  
**Testing**: `cargo test` — integration tests in `liminis-graph-core/tests/`; `cargo bench` for `benches/`  
**Target Platform**: Linux (ubuntu-latest) + macOS (macos-latest)  
**Project Type**: Library extension (`liminis-graph-core`) + binary wiring (`liminis-graph`)  
**Performance Goals**: p95 search latency ≤ 500 ms while ≥ 100 episodes extracting concurrently (SC-001)  
**Constraints**: No ML-runtime crates; no IPC surface changes; no new lbug API; write guard held only during DB commit, not during HTTP calls  
**Scale/Scope**: Single workspace per process; per-workspace write serialization

---

## Architecture Decisions

### AD-1: `AppState` struct replaces four separate `Arc` args in `dispatch`

Introduce `liminis-graph-core/src/app_state.rs`:

```rust
pub struct AppState {
    pub db:         Arc<Db>,
    pub embedder:   Arc<Embedder>,
    pub extractor:  Arc<dyn Extractor>,
    pub dedup:      Arc<dyn DedupAdapter>,
    pub write_lock: Arc<tokio::sync::RwLock<()>>,
    pub sink:       Arc<dyn TelemetrySink>,
}
```

`handlers::dispatch` takes `Arc<AppState>` instead of four separate `Arc` parameters. `main.rs` builds `AppState` from env vars. `ipc_parity.rs` is updated to construct `AppState` directly. This is a mechanical refactor — the IPC protocol wire format is unchanged.

**Why**: Consolidating app-wide shared state into one struct simplifies call sites, avoids signature churn as new adapters are added, and gives the write lock a natural home without modifying `Db`.

### AD-2: `Extractor` becomes an object-safe async trait

```rust
// liminis-graph-core/src/extractor.rs
pub trait Extractor: Send + Sync {
    fn extract<'a>(
        &'a self,
        body:     &'a str,
        group_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<ExtractionResult, Error>>;
}
```

`BoxFuture` (from the already-present `futures = "0.3"` workspace dep) gives object safety without a new proc-macro crate. The existing concrete struct is renamed `AnthropicExtractor` and implements this trait. A zero-latency `MockExtractor` also implements it for the bench.

Call sites updated:
- `handlers.rs`: `extractor: Arc<dyn Extractor>` (via `AppState`)
- `episode.rs`: same
- `main.rs`: `Arc::new(AnthropicExtractor::from_env(...)) as Arc<dyn Extractor>`
- `ipc_parity.rs`: constructs a `MockExtractor` (or `Arc<dyn Extractor>`)

No new Cargo.toml dependency needed.

### AD-3: `DedupAdapter` trait + `LocalDedupAdapter` + `PassthroughDedupAdapter`

```rust
// liminis-graph-core/src/dedup_adapter.rs (NEW)
pub trait DedupAdapter: Send + Sync {
    fn is_duplicate<'a>(
        &'a self,
        candidate: &'a EntityRow,
        incoming:  &'a ExtractedEntity,
    ) -> futures::future::BoxFuture<'a, Result<bool, Error>>;
}

/// Always returns `true` — preserves cosine-only behavior when LCG_DEDUP_LLM is unset.
pub struct PassthroughDedupAdapter;

/// Calls an out-of-process local model via local HTTP.
/// Configured via LCG_DEDUP_ADAPTER_URL (default: http://127.0.0.1:8767).
pub struct LocalDedupAdapter { ... }
```

`AppState::dedup` is `Arc<dyn DedupAdapter>`. If `LCG_DEDUP_LLM` is unset, `AppState` uses `PassthroughDedupAdapter` (existing cosine-only behavior preserved). If set, it uses `LocalDedupAdapter`.

`LocalDedupAdapter` sends a JSON POST to the configured URL:

```json
{
  "candidate": { "uuid": "...", "name": "...", "summary": "..." },
  "incoming":  { "name": "...", "entity_type": "...", "summary": "..." }
}
```

Expected response: `{"is_duplicate": true|false}`. On HTTP error, returns `Err(Error::Http(...))`, which the caller treats as "not a duplicate" and logs a warning (does not use fallback; the adapter is either up or down, not subject to the LLM fallback chain).

**Candidate cap**: at most 1 cosine-similar candidate per entity (the existing `brute_force_similar_entity` already returns the single best match above threshold). No batching needed; the cap is structurally enforced.

### AD-4: `episode::add_episode` split — async dedup before commit `spawn_blocking`

Current structure: single `spawn_blocking` containing all of steps 3–7.

New structure (three async/blocking phases):

```
Phase A — async HTTP (no lock):
  tokio::try_join!(embedder.embed(body), extractor.extract(body, group_id))
  for each entity name: embedder.embed(name)
  for each edge fact:   embedder.embed(fact)

Phase B — async dedup (no lock):
  spawn_blocking: for each entity, fetch cosine candidates from DB
  for each candidate: dedup_adapter.is_duplicate(candidate, incoming).await
  → produce Vec<DedupDecision> (Merge(uuid, merged_summary) | Insert(entity_data))

Phase C — commit spawn_blocking (write lock held):
  acquire write_lock.write().await
  spawn_blocking (guard moved in):
    apply DedupDecision vec (merge or insert entities)
    insert relationship edges
    insert episodic node
    insert MENTIONS edges
  → guard dropped at end of spawn_blocking
```

**Why write guard only in Phase C**: The LLM extraction (Phase A, ~30s) is the bottleneck. Holding the write guard during Phase A would block all search reads for 30s per episode — directly violating FR-001. Releasing the guard after Phase C (the few-millisecond DB commit) means reads are blocked only for milliseconds.

**Conn<'db> lifetime**: `Arc<Db>` is cloned into both spawn_blocking closures; `Conn` borrows from the cloned Arc. The `RwLock<()>` guard (for Phase C) is also cloned into the closure and dropped when the closure returns. This satisfies the constraint that the guard outlives the full write transaction.

### AD-5: `LlmRouter` for extraction primary→fallback

```rust
// liminis-graph-core/src/llm_router.rs (NEW)
pub struct LlmRouter {
    primary:         AnthropicExtractor,
    fallback:        Option<Box<dyn Extractor>>,
    primary_failed:  AtomicBool,   // once-per-session flag
    sink:            Arc<dyn TelemetrySink>,
}
```

`LlmRouter` implements `Extractor`. On `extract()`:

1. If `primary_failed` is `false`, call `primary.extract()`.
2. On primary success: return result.
3. On primary failure (any `Error`):
   - `compare_exchange(false, true, ...)`: if CAS succeeds (first failure this session), emit `TelemetryEvent::LlmFallback` and log to stderr.
   - If `fallback.is_some()`, call `fallback.as_ref().unwrap().extract()`.
   - If `fallback.is_none()`, return the original error.
4. On subsequent calls after `primary_failed` is `true`: skip primary entirely, call fallback directly.

Env var parsing: `LCG_EXTRACTION_LLM` is split on `:` — first token is primary model, second (optional) is fallback model. Both resolve to `AnthropicExtractor` instances (same API key, different model names). A future issue can introduce a non-Anthropic fallback path; for now both primary and fallback are Anthropic.

`main.rs` builds `LlmRouter::from_env(sink)` and wraps it in `Arc<dyn Extractor>`.

### AD-6: Prompt caching on Sonnet path

In `AnthropicExtractor::extract()`, detect the Sonnet path by `self.model.to_lowercase().contains("sonnet")`.

**When Sonnet**:
1. Add HTTP header: `anthropic-beta: prompt-caching-2024-07-31`
2. Change the `system` field from a plain string to an array with cache_control:
   ```json
   "system": [{"type": "text", "text": "<static prompt>", "cache_control": {"type": "ephemeral"}}]
   ```

**When not Sonnet** (Haiku, local model): send `system` as a plain string, no caching headers.

The static system prompt content must remain unchanged across calls (no variable data injected). The current prompt already satisfies this — no modification to the prompt text is needed.

Token usage parsing (`cache_read_input_tokens`, `cache_creation_input_tokens`) is already implemented in `emit_token_usage`; no change needed.

**SC-003 deferral**: The quantitative cache hit-rate assertion (against `project_context_graph_caching_2026_04_30.md`) is deferred because the baseline file does not exist in this repo. The structural changes (headers, cache_control) are the deliverable. Deferral is documented in ADR-042 and tracked in a follow-up issue filed as part of this plan.

### AD-7: HTTP 529 backoff in `AnthropicExtractor`

Implement exponential backoff for Anthropic 429 (rate limit) and 529 (overloaded) status codes before returning an error to the caller:

- Max retries: 3
- Initial delay: 1s, doubling each retry (1s → 2s → 4s)
- On the fourth failure, return the original `Error::Http`

Backoff runs entirely inside `AnthropicExtractor::extract()`. No partial WAL writes are possible because WAL append (Step 7 in `add_episode`) runs in Phase C, after successful extraction. If extraction fails after all retries, `add_episode` returns early with an error before Phase C.

`tokio::time::sleep` is used for the delay (already available via `tokio = { full }`).

### AD-8: `RwLock` guard lifecycle in read handlers

Read IPC methods (`find_entities`, `find_relationships`, `get_episodes`, `get_nodes_by_group`, `get_edges_by_group`, `get_edges_by_uuids`, `query_cypher`) acquire a **shared read guard** before entering `spawn_blocking`:

```rust
let _guard = app_state.write_lock.read().await;
tokio::task::spawn_blocking(move || {
    // _guard moved in; released when closure completes
    ...
}).await??
```

This ensures that a write never races with a read — the write's exclusive guard waits for all in-flight read guards to drop before acquiring. Since the search `spawn_blocking` completes in under 500 ms (the performance budget), write latency is bounded by that window.

`build_indices` acquires a write guard (it issues DDL-level writes to lbug). `delete_episode` acquires a write guard.

### AD-9: Concurrent read/write benchmark

New file `liminis-graph-core/benches/concurrent_rw.rs` using `criterion::async_executor::TokioExecutor`.

Benchmark setup (run once, outside the measured loop):
1. Create a temp LadybugDB (via `tempfile`).
2. Initialize schema.
3. Insert ~100 `Entity` rows with random name embeddings (dim=768 but cosine-filled with zeros is fine for bench purposes; no real LLM needed).
4. Construct `AppState` with `MockExtractor` (zero latency), `PassthroughDedupAdapter`, and a `MockEmbedder` (returns fixed dim-768 vector).

Bench loop (measured):
1. Spawn ≥ 100 tokio tasks each calling `episode::add_episode` with `MockExtractor`.
2. Concurrently fire ≥ 100 `search::hybrid_entity_search` queries on the same DB.
3. Collect p95 search latency from the search tasks.
4. Assert p95 ≤ 500 ms (criterion custom measurement).

**MockExtractor** returns a fixed `ExtractionResult` with 2 entities and 1 edge (matches test corpus shape). It implements `Extractor` using `BoxFuture` wrapping an `async { Ok(fixed_result) }`.

**MockEmbedder**: mirroring `MockExtractor`, a zero-latency embedder returning `vec![0.0f32; dim]`. Used only in the bench; not part of the main library API.

Note: the bench proves the RwLock contention model — it does not simulate real Anthropic latency. Real timing belongs in a manual end-to-end bench that is not run in CI.

### AD-10: ADR-042 content

`docs/adr/0002-reader-writer-split.md` documents:
- **Context**: lbug returns `Error::FailedQuery` on concurrent write connections; extraction (30s) must not block search (p95 ≤ 500 ms).
- **Decision**: `tokio::sync::RwLock<()>` in `AppState`. Write guard acquired immediately before the DB commit `spawn_blocking`, after all async HTTP work completes. Read guard acquired before any read `spawn_blocking`. Both guards move into the closure and are dropped on completion.
- **Consequences**: p95 search latency bounded by DB-commit duration (milliseconds), not LLM call duration (30s). Write throughput is single-writer; concurrent extractions queue on the write guard. Phase B (dedup HTTP) runs without a lock, so N concurrent extractions can overlap on dedup while serializing only on DB commit.
- **Deferral note**: SC-003 (quantitative prompt-cache hit rate ≥ baseline) is deferred pending `project_context_graph_caching_2026_04_30.md` being available in-repo. A follow-up issue must establish the baseline.

---

## Constitution Check

- **Principle I (IPC Parity)** — PASS. No IPC method signatures, request shapes, or response shapes change. The internal `dispatch` signature changes (takes `Arc<AppState>` instead of four `Arc` args) but the wire protocol is unchanged. Existing `ipc_parity.rs` corpus tests will be updated to use `AppState` construction but the test assertions are unchanged.

- **Principle II (Library and Binary Are Peers)** — PASS. `AppState`, `Extractor`, `DedupAdapter`, `LlmRouter`, `AnthropicExtractor`, `LocalDedupAdapter` are all in `liminis-graph-core` (library). No binary-only behavior introduced.

- **Principle III (LadybugDB Only)** — PASS. No driver abstraction introduced. `RwLock` serializes writes to the existing `lbug::Connection` layer; no new DB backend.

- **Principle IV (WAL Is Authoritative)** — PASS. WAL append (Step 7) remains in Phase C (the commit `spawn_blocking`), after all DB inserts succeed. No WAL writes occur on extraction failure (FR-012). The 529-backoff is entirely inside `AnthropicExtractor::extract()`, before the episode pipeline commits anything.

- **Principle V (No ML Runtimes)** — PASS. `DedupAdapter` and `EmbeddingAdapter` are HTTP or subprocess. `MockExtractor` and `MockEmbedder` are zero-latency in-process stubs implementing the trait — no model weights loaded. `cargo tree | grep -E "tch|candle|onnxruntime|mlx"` must remain empty.

### Performance budget gates

- **p95 search latency ≤ 500 ms** — `benches/concurrent_rw.rs` [HOT] bench covers this. Write guard is held only during DB commit (< 10 ms typical for lbug mutations). Bench asserts ≤ 500 ms.
- **Other budgets** (dedup wall time, memory, WAL replay) — not touched by this issue; no regression expected.

### Workflow gates

- Spec: `specs/001-rust-knowledge-graph/issues/004-concurrent-rw-llm-routing-spec.md` ✓
- IPC-touching change: `dispatch` internal signature only — no wire format change; parity corpus assertions unchanged ✓
- Hot-path change: `search::hybrid_entity_search` unchanged; new bench [HOT] in `benches/concurrent_rw.rs` ✓
- WAL-touching change: none (WAL TODO stub remains; no new WAL format changes in this issue) ✓
- Constitution deviation: AD-10 documents the RwLock design in ADR-042 ✓

---

## Project Structure

```text
docs/adr/
└── 0042-reader-writer-split.md          # NEW — documents RwLock design + SC-003 deferral

liminis-graph-core/
└── src/
    ├── app_state.rs                     # NEW — AppState struct (Db, Embedder, Extractor, DedupAdapter, RwLock, sink)
    ├── dedup_adapter.rs                 # NEW — DedupAdapter trait, PassthroughDedupAdapter, LocalDedupAdapter
    ├── llm_router.rs                    # NEW — LlmRouter implementing Extractor with primary→fallback + AtomicBool
    ├── extractor.rs                     # MODIFIED — Extractor becomes trait; existing struct → AnthropicExtractor; prompt-caching; 529 backoff
    ├── handlers.rs                      # MODIFIED — dispatch takes Arc<AppState>; read/write guards wired
    ├── episode.rs                       # MODIFIED — split into Phase A + Phase B + Phase C; DedupAdapter integration
    ├── lib.rs                           # MODIFIED — pub mod app_state; pub mod dedup_adapter; pub mod llm_router; re-exports
    └── error.rs                         # MODIFIED — add LlmFallback(String) variant (or reuse Ipc; see below)

liminis-graph/
└── src/
    └── main.rs                          # MODIFIED — build AppState; AnthropicExtractor + LlmRouter wiring

liminis-graph-core/benches/
└── concurrent_rw.rs                     # NEW [HOT] — async criterion bench; ≥100 writers + readers; p95 assert

liminis-graph-core/tests/
└── concurrent_rw_integration.rs         # NEW — integration test: fallback activation, once-per-session log dedup
```

**New env vars** (no new Cargo.toml dependencies required):

| Var | Default | Notes |
|-----|---------|-------|
| `LCG_EXTRACTION_LLM` | `claude-haiku-4-5-20251001` | `primary:fallback` colon-separated |
| `LCG_DEDUP_LLM` | unset | If unset, `PassthroughDedupAdapter` used |
| `LCG_DEDUP_ADAPTER_URL` | `http://127.0.0.1:8767` | URL for `LocalDedupAdapter` |
| `LCG_EMBEDDING_MODEL` | `bge-base-en-v1.5` | Existing env var, unchanged |

---

## Public Library API

### `AppState` (new)

```rust
pub struct AppState {
    pub db:         Arc<Db>,
    pub embedder:   Arc<Embedder>,
    pub extractor:  Arc<dyn Extractor>,
    pub dedup:      Arc<dyn DedupAdapter>,
    pub write_lock: Arc<tokio::sync::RwLock<()>>,
    pub sink:       Arc<dyn TelemetrySink>,
}

impl AppState {
    pub fn from_env(sink: Arc<dyn TelemetrySink>, db: Arc<Db>) -> Self;
}
```

### `Extractor` trait (modified)

```rust
pub trait Extractor: Send + Sync {
    fn extract<'a>(
        &'a self,
        body:     &'a str,
        group_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<ExtractionResult, Error>>;
}

pub struct AnthropicExtractor { /* api_key, model, client, sink */ }
impl AnthropicExtractor {
    pub fn from_env(sink: Arc<dyn TelemetrySink>) -> Self;
}
impl Extractor for AnthropicExtractor { ... }

pub struct MockExtractor;
impl Extractor for MockExtractor { /* returns fixed ExtractionResult, zero latency */ }
```

### `DedupAdapter` trait (new)

```rust
pub trait DedupAdapter: Send + Sync {
    fn is_duplicate<'a>(
        &'a self,
        candidate: &'a EntityRow,
        incoming:  &'a ExtractedEntity,
    ) -> futures::future::BoxFuture<'a, Result<bool, Error>>;
}

pub struct PassthroughDedupAdapter;   // always returns Ok(true)
pub struct LocalDedupAdapter { url: String, client: reqwest::Client }
impl LocalDedupAdapter {
    pub fn from_env() -> Self;
}
```

### `LlmRouter` (new)

```rust
pub struct LlmRouter {
    primary:        AnthropicExtractor,
    fallback:       Option<AnthropicExtractor>,
    primary_failed: std::sync::atomic::AtomicBool,
    sink:           Arc<dyn TelemetrySink>,
}
impl LlmRouter {
    pub fn from_env(sink: Arc<dyn TelemetrySink>) -> Self;
}
impl Extractor for LlmRouter { ... }
```

### Updated `handlers::dispatch`

```rust
pub async fn dispatch(req: IpcRequest, state: Arc<AppState>) -> IpcResponse;
```

### Updated `episode::add_episode`

```rust
pub async fn add_episode(
    state:              Arc<AppState>,
    name:               &str,
    body:               &str,
    source:             &str,
    source_description: &str,
    reference_time:     &str,
    group_id:           &str,
) -> Result<String, Error>;
```

---

## `TelemetryEvent::LlmFallback` wire format

Already present as a `TODO` variant in `telemetry.rs`. This issue completes the definition:

```rust
TelemetryEvent::LlmFallback {
    ts_ms:          u64,
    role:           String,   // "extraction"
    primary_model:  String,
    fallback_model: String,
    error_reason:   String,   // Display of the original Error
}
```

Emitted at most once per role per process lifetime. JSON serialization via `serde_json` to stderr (existing `StderrSink` channel).

---

## `episode::add_episode` Phase Split Detail

```rust
pub async fn add_episode(state, name, body, source, source_description, reference_time, group_id) {
    // ── Phase A: concurrent HTTP (no lock) ─────────────────────────────────
    let (content_embedding, extraction) = tokio::try_join!(
        state.embedder.embed(body),
        state.extractor.extract(body, group_id),
    )?;
    let mut name_embeddings = vec![];
    for n in &extraction.entities { name_embeddings.push(state.embedder.embed(n).await?); }
    let mut fact_embeddings = vec![];
    for f in &extraction.edges    { fact_embeddings.push(state.embedder.embed(f).await?); }

    // ── Phase B: dedup (DB reads + async LLM verification, no lock) ────────
    // Candidate fetch runs in spawn_blocking; is_duplicate runs async after.
    let candidates: Vec<Option<EntityRow>> = {
        let db = Arc::clone(&state.db);
        let gid = group_id.to_string();
        let name_embs = name_embeddings.clone();
        let entity_count = tokio::task::spawn_blocking(move || {
            let conn = db.connect()?;
            conn.entity_count_in_group(&gid)
        }).await??;
        // fetch candidates in one blocking pass
        ...
    };
    let mut dedup_decisions: Vec<DedupDecision> = vec![];
    for (i, (extracted, candidate)) in extraction.entities.iter().zip(candidates.iter()).enumerate() {
        let decision = if let Some(existing) = candidate {
            if state.dedup.is_duplicate(existing, extracted).await? {
                DedupDecision::Merge { existing_uuid: existing.uuid.clone(), merged_summary: ... }
            } else {
                DedupDecision::Insert { entity_data: ..., name_embedding: name_embeddings[i].clone() }
            }
        } else {
            DedupDecision::Insert { ... }
        };
        dedup_decisions.push(decision);
    }

    // ── Phase C: commit (exclusive write lock) ──────────────────────────────
    let _write_guard = state.write_lock.write().await;
    tokio::task::spawn_blocking(move || {
        // _write_guard moved in; dropped when closure returns
        let conn = db.connect()?;
        // apply DedupDecision vec (merges + inserts)
        // insert edges, episodic, MENTIONS
        // WAL append (Step 7 — still TODO pending issue #3 full wire-up)
        Ok::<_, Error>(episode_uuid)
    }).await??
}
```

---

## Complexity Tracking

No constitution violations.

| Note | Detail |
|------|--------|
| SC-003 deferred | `project_context_graph_caching_2026_04_30.md` absent from repo; structural caching changes only; ADR-042 records the deferral; follow-up issue to be filed |
| Write guard held post-Phase-B | Dedup adds one async HTTP round-trip per extracted entity (typically 2–5 per episode) before the write guard is acquired; this is acceptable latency on the write path and does not affect read paths |
