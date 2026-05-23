# Feature Specification: Embedder Sidecar for liminis-graph

**Feature Branch**: `fabrik/issue-39`
**Created**: 2026-05-22
**Status**: Draft
**Input**: Issue #39 — "Need embedder sidecar: liminis-graph requires HTTP embedder, not bundled (Principle V) — blocks all semantic methods in liminis-app"

## Background

liminis-graph follows Principle V: no ML runtime in the Rust crate. Embeddings are delegated to an external HTTP service via `HttpEmbedder`, which POSTs to `http://127.0.0.1:8765` by default (env var `GRAPHITI_EMBEDDING_URL`). The Python graphiti service handled this transparently by loading `sentence-transformers` + `bge-base-en-v1.5` in-process.

When liminis-app switches its backend to liminis-graph (`backend = rust`), no embedder service starts. Every semantic method that calls the embedder fails immediately:

```
{"jsonrpc":"2.0","id":"f","error":{"code":-32000,"message":"HTTP error: error sending request for url (http://127.0.0.1:8765/)"}}
```

This blocks five methods that are among the highest-traffic in liminis-app: `knowledge_find_entities` (31 call sites), `knowledge_find_relationships` (12), `knowledge_search_passages` (11), `knowledge_process_chunk` (7), and `knowledge_reprocess_entity_types`. Read-only methods that do not touch the embedder (`health_check`, `knowledge_status`, `knowledge_list_entities`, `knowledge_get_episodes`) continue to work.

This spec covers the sidecar's HTTP contract, its location in the codebase, its configuration surface, and the lifecycle-wiring requirements it imposes on liminis-app. The actual implementation of the sidecar and the liminis-app lifecycle wiring are separate issues.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Semantic Search Works After Backend Switch (Priority: P1)

A liminis-app user switches `backend` to `rust`. The embedder sidecar starts automatically alongside liminis-graph and serves embedding requests. `knowledge_find_entities` returns results instead of an HTTP error.

**Why this priority**: Five high-traffic methods are completely blocked without the sidecar. This is the primary cutover blocker from Python to Rust backend.

**Independent Test**: Start the embedder sidecar, POST `{"text": "hello world", "model": "bge-base-en-v1.5"}` to `http://127.0.0.1:8765/`, and assert the response is `{"embedding": [<768 floats>]}` with all floats finite.

**Acceptance Scenarios**:

1. **Given** the sidecar is running and the model is loaded, **When** `POST /` is called with `{"text": "Adrian Tchaikovsky", "model": "bge-base-en-v1.5"}`, **Then** the response is `{"embedding": [<768 floats>]}` with HTTP 200 and Content-Type `application/json`.
2. **Given** the sidecar is running, **When** a liminis-graph `HttpEmbedder` calls it with any non-empty text, **Then** the returned vector has exactly 768 elements (matching `GRAPHITI_EMBEDDING_DIM` default) and all elements are finite `f32`-representable floats.
3. **Given** the sidecar is running, **When** `GET /health` is called, **Then** the response is HTTP 200 with `{"ok": true}` — this is used by liminis-app to health-probe before starting liminis-graph.
4. **Given** the sidecar has not yet finished loading the model, **When** `GET /health` is called, **Then** the response is HTTP 503 — the caller must retry rather than assuming the sidecar is broken.
5. **Given** the sidecar is running, **When** `POST /` is called with `{"text": "", "model": "bge-base-en-v1.5"}`, **Then** the response is HTTP 400 — an empty text string is not a valid embedding input.

---

### User Story 2 — Sidecar Lifecycle Does Not Race liminis-graph Startup (Priority: P1)

liminis-app starts the embedder sidecar, waits until it reports healthy, then starts liminis-graph. The first `knowledge_process_chunk` call does not fail because the embedder is still loading.

**Why this priority**: If liminis-graph starts before the sidecar is ready, the first embedding call after startup fails, corrupting or dropping the first ingested chunk. Model loading for `bge-base-en-v1.5` takes several seconds on a cold start.

**Independent Test**: Start the sidecar in a subprocess, poll `GET /health` every 100 ms, and assert that a subsequent `POST /` call succeeds within 500 ms of the first 200 response from `/health`.

**Acceptance Scenarios**:

1. **Given** the sidecar process just started and the model is still loading, **When** `GET /health` returns HTTP 503, **Then** retrying every 100–500 ms eventually yields HTTP 200 once the model is loaded.
2. **Given** `GET /health` returns HTTP 200, **When** `POST /` is immediately called, **Then** the call succeeds and returns a valid embedding vector — no race between "health ok" and "model actually ready."
3. **Given** liminis-app has confirmed the sidecar is healthy, **When** liminis-graph starts and immediately calls `HttpEmbedder::embed`, **Then** the embedding request succeeds.

---

### Edge Cases

- `POST /` with missing `text` field → HTTP 400, descriptive error message.
- `POST /` with `model` field naming a model that is not loaded → HTTP 400 with a message indicating the model mismatch (the sidecar only loads one model at startup; it does not hot-swap models).
- Sidecar receives concurrent `POST /` requests → each is handled independently; no global lock that would serialize embedding calls and stall the liminis-graph worker pool.
- Sidecar is killed while liminis-graph is running → liminis-graph embedding calls fail with an HTTP error, which surfaces as a JSON-RPC error to the caller; liminis-graph does not crash.
- `GRAPHITI_EMBEDDING_URL` points to a different host:port → sidecar binds to the configured address, not hardcoded `127.0.0.1:8765`.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The sidecar MUST expose `POST /` (path configurable via `GRAPHITI_EMBEDDING_PATH`, default `/`) accepting `{"text": "<string>", "model": "<string>"}` and returning `{"embedding": [<floats>]}` with HTTP 200.
- **FR-002**: The sidecar MUST expose `GET /health` returning HTTP 200 with `{"ok": true}` when the model is fully loaded and ready to serve, and HTTP 503 when not yet ready. No other status codes are expected on this path.
- **FR-003**: The sidecar MUST bind to the address and port derived from `GRAPHITI_EMBEDDING_URL` (default `http://127.0.0.1:8765`). If the env var is unset, the defaults apply.
- **FR-004**: The sidecar MUST load the model named by `GRAPHITI_EMBEDDING_MODEL` (default `bge-base-en-v1.5`) via `sentence-transformers` at startup. The `/health` endpoint MUST return 503 until the model is fully loaded.
- **FR-005**: The sidecar MUST return HTTP 400 for `POST /` with an empty `text` field or a missing `text` field.
- **FR-006**: The sidecar MUST return HTTP 400 for `POST /` when the `model` field names a model other than the one loaded at startup (the sidecar loads exactly one model; runtime hot-swap is out of scope).
- **FR-007**: The sidecar MUST handle concurrent `POST /` requests without a global serialization lock. Multiple simultaneous embedding calls from a liminis-graph worker pool MUST all be served.
- **FR-008**: The sidecar MUST be implemented as a Python script runnable via `uv run` with no pre-installed environment beyond a `uv`-managed inline dependency spec. It MUST declare its own dependencies (e.g., `sentence-transformers`, `fastapi`, `uvicorn` or `aiohttp`) in the script header in PEP 723 format.
- **FR-009**: The sidecar MUST live in the liminis-framework knowledge-graph skill scripts directory (alongside any existing Python server scripts) so that it ships with framework workspaces and uses the same `uv` runtime as the rest of the skill.
- **FR-010**: The liminis-graph README MUST be updated to document that `HttpEmbedder` requires a running embedder service at `GRAPHITI_EMBEDDING_URL`, name the sidecar script, and explain how to start it manually.
- **FR-011**: liminis-app's lifecycle for `backend = rust` MUST start the sidecar before starting liminis-graph, poll `GET /health` until HTTP 200, and only then start liminis-graph. The exact polling interval and timeout are implementation decisions for the lifecycle-wiring issue.
- **FR-012**: The sidecar MUST log startup progress to stderr: at minimum, which model is loading and when it is ready.

### Key Entities

- **Sidecar process**: A Python script (`embedder_server.py` or similar) managed by the same `uv` runtime as other knowledge-graph skill scripts. One instance per liminis-app workspace. Binds to `127.0.0.1:8765` by default.
- **HTTP embed endpoint** (`POST /`): Request body `{"text": str, "model": str}`. Response body `{"embedding": list[float]}`. Used exclusively by `HttpEmbedder` in liminis-graph-core.
- **Health endpoint** (`GET /health`): Response `{"ok": true}` at HTTP 200 when ready; HTTP 503 when loading. Used by liminis-app lifecycle to gate liminis-graph startup.
- **`GRAPHITI_EMBEDDING_URL`**: Env var controlling where `HttpEmbedder` sends requests and where the sidecar binds. Both sides read the same variable.
- **`GRAPHITI_EMBEDDING_MODEL`**: Env var controlling which `sentence-transformers` model the sidecar loads. `HttpEmbedder` sends this value in the `model` field of every request.
- **`GRAPHITI_EMBEDDING_DIM`**: Env var controlling the expected embedding dimension. The sidecar's output vector length MUST match this value (default 768).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `POST /` with a non-empty text string returns a 768-element (or `GRAPHITI_EMBEDDING_DIM`-element) float array within 5 seconds on a warm sidecar (model already loaded).
- **SC-002**: `GET /health` returns HTTP 200 within 500 ms of the model finishing its load, and returns HTTP 503 during loading — verified by timing the health poll against a fresh sidecar startup.
- **SC-003**: `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_search_passages`, `knowledge_process_chunk`, and `knowledge_reprocess_entity_types` all return successful JSON-RPC results (not HTTP-error errors) when the sidecar is running alongside liminis-graph — verified end-to-end against a real LadybugDB.
- **SC-004**: Unmodified `HttpEmbedder::from_env()` in liminis-graph-core can complete an embedding round-trip against the sidecar without code changes to the Rust crate.
- **SC-005**: The liminis-graph README contains a section explaining the sidecar dependency, the `GRAPHITI_EMBEDDING_URL` env var, and how to start the sidecar manually.

## Assumptions

- `HttpEmbedder` sends `POST` to the root path (`/`) with body `{"text": "...", "model": "..."}` and expects `{"embedding": [...]}`. This is confirmed by `liminis-graph-core/src/embedder.rs`. No changes to `HttpEmbedder` are needed.
- The `sentence-transformers` library can load `bge-base-en-v1.5` and produce 768-dimensional vectors matching what the Python graphiti service previously produced. The embeddings do not need to be byte-identical to prior runs; semantic search quality within existing-graph tolerance is sufficient.
- `uv` is available in the same environment where liminis-app runs the sidecar. This is already a dependency of the knowledge-graph skill.
- TCP binding on `127.0.0.1:8765` is acceptable (no Unix socket). `HttpEmbedder` currently expects an HTTP URL, and changing the transport would require adding a Rust dep (`reqwest-unix-socket`). That trade-off is deferred.
- The sidecar is a single-model server: it loads one model at startup and never hot-swaps. Multi-model support is out of scope.
- The `model` field in `POST /` requests from `HttpEmbedder` will always match `GRAPHITI_EMBEDDING_MODEL`. A mismatch is treated as a client error (HTTP 400) rather than triggering a model swap.

## Out of Scope

- Unix socket transport for the embedder endpoint — TCP is retained; Rust-side change not warranted at this time.
- ANE/MLX-based embedding backend — follow-up opportunity noted in `[[project_ane_opportunities]]`; CPU `sentence-transformers` is sufficient for Python parity.
- Multi-model serving or runtime model hot-swap.
- Sidecar implementation code — covered by a follow-up issue: "Implement embedder sidecar (`embedder_server.py`)".
- liminis-app lifecycle wiring (`backend = rust` process management) — covered by a separate follow-up issue: "Wire embedder sidecar into liminis-app graphiti-service lifecycle for `backend = rust`".
- Changes to `HttpEmbedder`, `MockEmbedder`, or any other Rust code in liminis-graph-core.
- GPU/CUDA inference — the sidecar runs on CPU by default; GPU is a follow-up.

## Source References

- `liminis-graph-core/src/embedder.rs` — `HttpEmbedder` implementation; defines wire format and env vars
- `liminis-graph-core/src/types.rs` — `EmbeddingResult { embedding: Vec<f32> }` (response shape)
- Python reference: `liminis-framework/framework/src/skills/knowledge-graph/scripts/` — target location for the sidecar script
