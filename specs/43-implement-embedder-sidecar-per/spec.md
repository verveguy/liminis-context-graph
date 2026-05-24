# Feature Specification: Embedder Sidecar HTTP Server (BGE bge-base-en-v1.5)

**Feature Branch**: `fabrik/issue-43`
**Created**: 2026-05-22
**Status**: Draft
**Input**: Issue #43 — "Implement embedder sidecar per spec from #39 (BGE bge-base-en-v1.5 over HTTP/Unix socket)"

## Background

liminis-graph's Rust backend embeds text via `HttpEmbedder`, which calls `POST http://127.0.0.1:8765/` expecting a `{"embedding": [...]}` response. Without a running sidecar on that port, every semantic method in liminis-graph fails at runtime: `knowledge_find_entities`, `knowledge_search_passages`, `knowledge_find_relationships`, `knowledge_process_chunk`, and `knowledge_reprocess_entity_types` all return `HTTP error: error sending request for url (http://127.0.0.1:8765/)`.

Issue #39 produced the spec and HTTP contract (ADR-044) for this sidecar but did not implement it. This issue implements the sidecar per that spec. The larger lifecycle wiring (spawn/probe/shutdown from liminis-app) is tracked separately in #42; this issue only delivers the sidecar process itself, runnable standalone for testing.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Developer Runs the Sidecar and Gets Embeddings (Priority: P1)

A developer starts the embedder sidecar with `uv run embedder_server.py` and immediately can send text to `POST http://127.0.0.1:8765/` to receive a 768-float embedding vector. No separate venv setup or pip install is required — the PEP 723 inline dependencies are resolved automatically by `uv`.

**Why this priority**: This is the blocking failure: no embeddings → no semantic methods → the Rust backend is effectively non-functional. Everything else is secondary.

**Independent Test**: Run `uv run embedder_server.py`, wait for the model-ready log line, then `curl -X POST http://127.0.0.1:8765/ -H 'content-type: application/json' -d '{"text":"hello","model":"bge-base-en-v1.5"}'` and assert the response contains `"embedding"` with exactly 768 floats.

**Acceptance Scenarios**:

1. **Given** the sidecar is started with `uv run embedder_server.py` and the model has loaded, **When** a client sends `POST /` with `{"text": "hello", "model": "bge-base-en-v1.5"}`, **Then** the response is `{"embedding": [<768 floats>]}` with HTTP 200.
2. **Given** the sidecar is running, **When** a client sends `POST /` with an empty `"text"` field, **Then** the response is an HTTP 400 error with a descriptive message.
3. **Given** the sidecar is running, **When** a client sends `POST /` with a missing `"text"` field, **Then** the response is an HTTP 400 error.
4. **Given** the sidecar is running, **When** a client sends `POST /` with `"model"` set to an unrecognised model name, **Then** the sidecar returns HTTP 400; it does NOT silently embed with a different model.
5. **Given** `liminis-graph`'s `HttpEmbedder` is configured to call `http://127.0.0.1:8765/`, **When** `knowledge_find_entities` is called against a populated demo-notebook workspace, **Then** it completes without HTTP errors.

---

### User Story 2 — Caller Can Probe Readiness Before Using the Embedder (Priority: P1)

Before forwarding embedding requests, liminis-app (or a developer's test script) can call `GET /health` and receive a machine-readable signal indicating whether the model has finished loading. During the cold-start window the sidecar responds with a not-ready status, preventing false negatives from being reported as embedding failures.

**Why this priority**: The model loading window (5–15 seconds) is long enough that callers will attempt requests before the sidecar is ready. Without a health endpoint, the caller cannot distinguish "sidecar not started" from "sidecar starting" from "sidecar ready" — all three look like a connection refused or a 500.

**Independent Test**: Start the sidecar, immediately poll `GET /health` in a loop (1 Hz), and verify: (a) during loading the endpoint returns 503, (b) once loading completes it returns 200 `{"ok": true}`, (c) no request between start and ready returns 200.

**Acceptance Scenarios**:

1. **Given** the sidecar process has started but the model has not finished loading, **When** a client calls `GET /health`, **Then** the response is HTTP 503 with body `{"ok": false}`.
2. **Given** the model has finished loading, **When** a client calls `GET /health`, **Then** the response is HTTP 200 with body `{"ok": true}`.
3. **Given** the model is ready, **When** `GET /health` is polled 100 times in rapid succession, **Then** every response is HTTP 200 `{"ok": true}` — the endpoint never flaps after the model is ready.

---

### User Story 3 — Cold-Start Is Visible in Logs (Priority: P2)

When a developer starts the sidecar they see clear log lines indicating (a) that the model is loading, and (b) the moment it has finished loading and is accepting requests. This lets a human or automated supervisor know exactly when to begin forwarding traffic without polling.

**Why this priority**: Without structured logging the cold-start window is a silent black box. Operators over-wait or under-wait, causing either wasted startup time or premature request failures.

**Independent Test**: Capture stdout/stderr from `uv run embedder_server.py`, assert one log line contains "loading" (or equivalent) before the model is ready, and a distinct log line contains "ready" (or equivalent) after.

**Acceptance Scenarios**:

1. **Given** the sidecar starts, **When** it begins loading the model, **Then** it emits a log line containing a "loading" signal (e.g., `INFO: Loading model bge-base-en-v1.5...`) before the first successful `/health` 200.
2. **Given** the model load completes, **When** the sidecar becomes ready, **Then** it emits a log line containing a "ready" signal (e.g., `INFO: Model ready. Listening on 127.0.0.1:8765`) and subsequently `/health` returns 200.

---

### Edge Cases

- `LCG_EMBEDDING_URL` env var overrides the default bind address and port (both host and port are extracted from the URL).
- `POST /` received while the model is still loading → HTTP 503, not a crash or hang.
- Malformed JSON body → HTTP 400 with a message; the sidecar keeps running.
- `"text"` field present but empty string → HTTP 400.
- `"model"` field absent → HTTP 400 (do not silently use a default model; the caller must be explicit).
- Very long input text (e.g., 10 000 tokens) → `sentence-transformers` truncates to the model's max sequence length (512 tokens for `bge-base-en-v1.5`); the sidecar returns the embedding without erroring. This truncation behaviour is documented.
- Port already in use → sidecar exits with a clear error message naming the port; it does NOT retry on a different port.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The sidecar MUST be a single Python file (`embedder_server.py`) with PEP 723 inline dependency metadata so it can be invoked directly with `uv run embedder_server.py` without any prior environment setup.
- **FR-002**: The sidecar MUST bind to `127.0.0.1:8765` by default. The bind address and port MUST be overrideable via the `LCG_EMBEDDING_URL` environment variable (the sidecar extracts host and port from the URL). This supersedes the originally-proposed `EMBEDDER_HOST`/`EMBEDDER_PORT` split — using a single URL keeps the sidecar and `HttpEmbedder` (Rust) in lockstep, since both read `LCG_EMBEDDING_URL`. See ADR-044 Implementation Notes.
- **FR-003**: The sidecar MUST serve `POST /` accepting `application/json` body `{"text": "<string>", "model": "<string>"}` and returning `{"embedding": [<N floats>]}` with HTTP 200. The `text` and `model` fields are both required.
- **FR-004**: The sidecar MUST load `BAAI/bge-base-en-v1.5` via `sentence-transformers`. The returned embedding MUST have exactly 768 dimensions, matching the dimension expected by liminis-graph's `HttpEmbedder`.
- **FR-005**: The sidecar MUST serve `GET /health`. While the model is loading, this endpoint MUST return HTTP 503 with body `{"ok": false}`. Once the model is ready, it MUST return HTTP 200 with body `{"ok": true}`. The ready state MUST NOT revert to 503 after the model has loaded.
- **FR-006**: The sidecar MUST emit a structured log line at the moment it begins loading the model and a second structured log line at the moment the model is ready and the server is accepting requests.
- **FR-007**: The sidecar MUST use `aiohttp` as its HTTP framework (`AppRunner`/`TCPSite` runner pattern). This supersedes the originally-proposed FastAPI + uvicorn — FastAPI's uvicorn runner opens the TCP socket inside or after the lifespan hook, so lifecycle pollers receive `ECONNREFUSED` during the cold-start window instead of HTTP 503. The `aiohttp` pattern binds the socket synchronously before model loading begins. See ADR-044 Implementation Notes.
- **FR-008**: The sidecar MUST reject requests to `POST /` while the model is still loading with HTTP 503; it MUST NOT hang, crash, or queue the request indefinitely.
- **FR-009**: `POST /` MUST return HTTP 400 for: (a) missing or empty `"text"` field, (b) missing `"model"` field, (c) `"model"` value that does not match the loaded model name, (d) malformed JSON body.
- **FR-010**: The sidecar MUST operate over TCP only (no Unix socket transport). The `HttpEmbedder` in Rust already targets `http://127.0.0.1:8765/`; no Rust-side changes are required for this issue.
- **FR-011**: The sidecar MUST be a single-text endpoint (no batch `"texts"` array). One `POST /` call embeds one string. Batch support is a follow-up.
- **FR-012**: The cold-start time (process spawn → first HTTP 200 from `GET /health`) MUST be documented in the project README, with a measured reference time for `bge-base-en-v1.5` on CPU (expected range: 5–15 seconds).
- **FR-013**: The script MUST be placed alongside the existing knowledge-graph skill scripts. The exact path MUST be documented in the PR description.

### Key Entities

- **`embedder_server.py`**: Single-file PEP 723 Python script. Serves `POST /` (embed) and `GET /health`. Depends on `aiohttp`, `sentence-transformers`.
- **Embed request**: `{"text": "<non-empty string>", "model": "<model name>"}` — both fields required.
- **Embed response**: `{"embedding": [<768 floats>]}` — exactly 768 dimensions for `bge-base-en-v1.5`.
- **Health response (loading)**: HTTP 503, `{"ok": false}`.
- **Health response (ready)**: HTTP 200, `{"ok": true}`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `uv run embedder_server.py` starts without error on a machine with no pre-existing Python environment for this script; all dependencies are resolved by `uv` from the inline metadata.
- **SC-002**: `curl -X POST http://127.0.0.1:8765/ -H 'content-type: application/json' -d '{"text":"hello","model":"bge-base-en-v1.5"}'` returns `{"embedding": [...]}` with exactly 768 floats.
- **SC-003**: `GET /health` returns HTTP 503 while the model is loading and HTTP 200 `{"ok": true}` once the model is ready; it never returns 200 before the model is loaded.
- **SC-004**: With the sidecar running on the default port, `knowledge_find_entities` completes end-to-end against demo-notebook without HTTP errors (verified using the repro from #39).
- **SC-005**: Cold-start time from `uv run` invocation to first HTTP 200 from `/health` is documented in the README; the documented value reflects a CPU run with a warm HuggingFace cache (5–15 s). First-run time (which includes a ~500 MB model download) is noted separately.
- **SC-006**: Port-collision: if `127.0.0.1:8765` is already in use, the sidecar exits non-zero with a clear error message naming the port; it does not start silently on a different port.

## Assumptions

- `uv` is available on the target developer machine; no other Python toolchain is required.
- `bge-base-en-v1.5` weights are downloaded from HuggingFace Hub on first run; subsequent runs use the local `sentence-transformers` cache. Cold-start time includes this download only on first run.
- The loaded model's embedding dimension is always 768 for `BAAI/bge-base-en-v1.5`. No dimension check beyond asserting the output shape at startup is required.
- `HttpEmbedder` in liminis-graph currently calls `POST /` with `{"text": "...", "model": "..."}` and expects `{"embedding": [...]}` — the contract is defined in ADR-044. No changes to the Rust side are needed for this issue.
- The sidecar is a single-process, single-worker server. Concurrency under load is not a requirement for this issue; embedding is CPU-bound and the in-process GIL serialises calls naturally.
- `aiohttp` + `sentence-transformers` is an acceptable dependency footprint for a developer tool sidecar. FastAPI + uvicorn was originally assumed but superseded by ADR-044 (server-before-model ordering requirement).
- Logging goes to stdout; structured format (human-readable level + message) is sufficient. No JSON log format or log aggregation is required.

## Out of Scope

- liminis-app lifecycle wiring (spawn, health-probe loop, graceful shutdown) — tracked in #42.
- Bundling the sidecar into the liminis-app distribution — tracked in #42.
- Batch embedding endpoint (`{"texts": [...]}`) — file as a follow-up after this ships.
- Unix socket transport — follow-up after this ships; would also require `HttpEmbedder` changes.
- ANE/MLX-accelerated embedding — separate spec.
- Reranker model serving — separate concern.
- `bge-large-en-v1.5` or other model variants — out of scope; the loaded model name is fixed at `bge-base-en-v1.5` for this issue.

## Source References

- `specs/39-need-embedder-sidecar-liminis/spec.md` — prior spec (produced by #39, defines requirements this issue implements)
- `docs/adr/0044-embedder-http-contract.md` — HTTP wire contract between `HttpEmbedder` and the sidecar
- `liminis-graph-core/src/embedder.rs` — `HttpEmbedder` Rust implementation (caller of this sidecar)
- `liminis-framework/framework/src/skills/knowledge-graph/scripts/` — existing skill scripts; `embedder_server.py` ships alongside these
