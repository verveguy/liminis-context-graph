# ADR 0044: HTTP Embedding Sidecar Contract

**Date**: 2026-05-22
**Status**: Accepted

## Context

liminis-graph follows Principle V: no ML runtime in the Rust crate. `HttpEmbedder` in `liminis-graph-core` delegates all embedding work to an external HTTP service, POSTing to `LCG_EMBEDDING_URL` (default `http://127.0.0.1:8765`). This out-of-process boundary introduces a stable API contract between the Rust crate and whatever process serves embeddings.

The upstream Python graphiti-core service previously served this role implicitly — it loaded `sentence-transformers` in-process and answered the same wire format. When liminis-app switches to `backend = rust`, no embedder service starts automatically, blocking all five semantic methods:

- `knowledge_find_entities` (31 call sites)
- `knowledge_find_relationships` (12)
- `knowledge_search_passages` (11)
- `knowledge_process_chunk` (7)
- `knowledge_reprocess_entity_types`

A standalone sidecar script (`embedder_server.py` in `liminis-framework`) replaces the implicit Python service. Recording its API contract here ensures future implementors cannot silently break compatibility with existing HNSW indexes.

## Decision

### Wire Format

**Embed endpoint** — `POST <LCG_EMBEDDING_PATH>` (default `/`):

Request body:
```json
{"text": "<non-empty string>", "model": "<model name>"}
```

Response body (HTTP 200):
```json
{"embedding": [<768 floats>]}
```

**Health endpoint** — `GET /health`:

- HTTP 200 + `{"ok": true}` — model is loaded and ready
- HTTP 503 + `{"ok": false}` — model is still loading; client should retry

### Env Var Linkage

| Variable | Consumer | Default | Description |
|----------|----------|---------|-------------|
| `LCG_EMBEDDING_URL` | `HttpEmbedder` (reads) + sidecar (binds) | `http://127.0.0.1:8765` | Full URL `HttpEmbedder` posts to; sidecar extracts host:port from this |
| `LCG_EMBEDDING_MODEL` | `HttpEmbedder` (sends in body) + sidecar (loads at startup) | `bge-base-en-v1.5` / `BAAI/bge-base-en-v1.5` | See model-name normalization below |
| `LCG_EMBEDDING_DIM` | `HttpEmbedder` (reads) + sidecar (warns on mismatch) | `768` | Expected vector length; sidecar logs a warning at startup if the loaded model's actual dim differs |
| `LCG_EMBEDDING_PATH` | sidecar (mounts route) | `/` | Path the sidecar mounts the embed endpoint on; must match the path in `LCG_EMBEDDING_URL` |

### L2 Normalization is Mandatory

The sidecar **must** call `model.encode(text, normalize_embeddings=True)`. The HNSW index uses dot-product similarity and assumes unit vectors. The upstream Python graphiti-core service wrote all vectors with `normalize_embeddings=True`; a sidecar that omits normalization silently degrades search quality for any database with pre-existing embeddings. This is not a performance trade-off — it is a correctness requirement.

### Model-Name Basename Normalization

`HttpEmbedder`'s default `LCG_EMBEDDING_MODEL` is `bge-base-en-v1.5` (no org prefix). The sidecar's default is `BAAI/bge-base-en-v1.5` (HuggingFace-format with org prefix required to resolve from the Hub). When neither side sets the env var, the Rust request sends `"model": "bge-base-en-v1.5"` while the sidecar loads `BAAI/bge-base-en-v1.5`.

Exact string comparison would produce a false HTTP 400 in the unset default case. The rule: compare `model_name.split("/")[-1]` (basename) from both the request body and the loaded model name. `bge-base-en-v1.5 == bge-base-en-v1.5` passes; a genuinely different model (`all-MiniLM-L6-v2`) still returns HTTP 400.

### Server-Before-Model Startup Ordering

The HTTP server **must** bind and begin accepting connections before the `SentenceTransformer` model load begins. Model loading takes typically 5–15 s on CPU (warm HuggingFace cache). If the socket is not open during load, lifecycle pollers cannot poll `GET /health` and will time out waiting for TCP connectivity. The sidecar uses `aiohttp`'s `AppRunner`/`TCPSite` runner pattern to achieve this: `runner.setup()` + `site.start()` binds the socket synchronously, then `asyncio.to_thread(SentenceTransformer, ...)` loads the model without blocking the event loop.

### Error Responses

| Condition | Status |
|-----------|--------|
| `text` missing or empty | 400 |
| `model` basename does not match loaded model | 400 |
| Model still loading (embed request) | 503 |
| Internal error during encode | 500 |

### Concurrency

The sidecar handles concurrent `POST /` requests by offloading each `encode()` call to a thread via `asyncio.to_thread`. The Python GIL serializes the CPU-bound encoding work itself; the `aiohttp` event loop remains responsive to incoming connections. No application-level serialization lock is introduced.

## Implementation Notes

### Framework: `aiohttp`, not FastAPI

The sidecar uses `aiohttp`'s `AppRunner`/`TCPSite` runner pattern rather than FastAPI + uvicorn. FastAPI's uvicorn runner opens the TCP socket inside or after the lifespan hook — during the cold-start window (5–15 s of model loading) the socket is not bound and lifecycle pollers receive `ECONNREFUSED` instead of a 503. The `aiohttp` pattern binds the socket synchronously in `runner.setup()` + `site.start()`, then loads the model in a background thread, so `GET /health` returns 503 from the very first millisecond. Feature spec #43 originally specified FastAPI (FR-007); that requirement is superseded by this ADR.

### Env Vars: `LCG_EMBEDDING_URL`, not `EMBEDDER_HOST`/`EMBEDDER_PORT`

Both `HttpEmbedder` (Rust caller) and the sidecar read a single `LCG_EMBEDDING_URL` to determine the bind/target address. Feature spec #43 proposed splitting this into `EMBEDDER_HOST` and `EMBEDDER_PORT` (FR-002); that approach is superseded by this ADR. Using a single URL env var keeps the two processes in lockstep — setting `LCG_EMBEDDING_URL` once overrides both where the Rust crate sends requests and where the sidecar binds.

## Consequences

- **`HttpEmbedder` requires a running sidecar** — any deployment of the liminis-context-graph binary must also start `embedder_server.py` before accepting connections. Failing to start the sidecar is now an explicit deployment error, not a silent misconfiguration.
- **L2 normalization is load-bearing** — future sidecar implementations (GPU, ANE/MLX) must also normalize. This constraint must be documented in any replacement implementation.
- **Basename normalization is the contract** — callers and sidecar operators who set `LCG_EMBEDDING_MODEL` should use consistent values (either both with or both without the `BAAI/` prefix). The basename normalization is a compatibility shim for the default case, not a license to use arbitrary prefixes.
- **`LCG_EMBEDDING_PATH` is a new env var** — not present in any prior Rust or Python code. Its default `/` is consistent with `HttpEmbedder`'s default URL (bare host:port with no path component).
- **ADR-0001 coverage**: This ADR satisfies ADR-0001's requirement for recording decisions that introduce or stabilize the embedding contract. Future changes to the wire format, normalization behavior, or health-check semantics require a new or amended ADR.
