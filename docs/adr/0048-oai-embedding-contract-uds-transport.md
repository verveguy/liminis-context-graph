# ADR-0048: OpenAI-compatible embedding contract over UDS; hyper for UDS transport

**Date**: 2026-05-25
**Status**: Accepted
**Supersedes**: ADR-0044 (wire-format section only)

## Context

The liminis-graph Rust binary (`liminis-context-graph`) previously spoke a bespoke
`{"text": ..., "model": ...}` / `{"embedding": [...]}` contract to a Python
`embedder_server.py` sidecar over HTTP at `127.0.0.1:8765`. As part of the embedder
cutover to the Swift CoreML BGE-base sidecar (liminis#794), we need the Rust binary to
reach the new sidecar over a Unix domain socket.

The Swift sidecar already exposes the OpenAI-compatible `POST /v1/embeddings` contract:
request `{"input": "...", "model": "..."}`, response
`{"data": [{"embedding": [...f64...], "index": 0}], "model": "...", "usage": {...}}`.
Rather than invent a third contract, we commit to the OpenAI shape as the stable
interface this binary uses for all embedding calls, regardless of transport.

`reqwest` 0.12 does not support Unix domain sockets natively. `hyper` 1.x does, and it
is already a transitive dependency via `reqwest`. We added it explicitly to the workspace
to make the dependency intentional and to get compile-time conflict detection if reqwest
is ever bumped to a version that pulls a different hyper major.

## Decision

### Wire format: OpenAI-compatible on both transports

`OaiEmbedder` uses the OpenAI wire contract regardless of transport:

- **Request body**: `{"input": "<text>", "model": "<model>"}`
- **Response body**: `{"data": [{"embedding": [...], "index": 0}], "model": "...", "usage": {...}}`

This supersedes ADR-0044's wire-format section. The old `{"text": ..., "model": ...}` /
`{"embedding": [...]}` contract is retired. Any service called by `liminis-context-graph`
must implement the OpenAI shape.

### Transport selection: CLI flags at startup

`--embedder-uds <path>` and `--embedder-http <url>` are mutually exclusive CLI flags.
If neither is passed, the binary defaults to UDS at `/tmp/liminis-inference.sock` (the
bundled Swift sidecar path on macOS). If the default path doesn't exist, it falls back
to `LCG_EMBEDDING_URL` (HTTP). If that's also absent, it exits with a clear error.

### UDS transport: hyper 1.x `client::conn::http1`

Each embed call opens a fresh `UnixStream`, performs an HTTP/1.1 exchange via
`hyper::client::conn::http1::handshake`, then closes. Fresh-per-call semantics make
cancellation safe — dropping the future closes the stream with no leaked state.

`hyper` and `hyper-util` are added as explicit workspace dependencies matching the major
version range that `reqwest` 0.12 already pulls transitively. If `reqwest` is ever
upgraded and its transitive hyper version changes majors, the explicit dep will produce
a compile-time conflict that forces a deliberate version resolution.

### f64 → f32 conversion

The Swift sidecar returns `embedding: [Double]` (f64). The Rust response struct
deserializes as `Vec<f64>` and then converts to `Vec<f32>` explicitly:

```rust
embedding.into_iter().map(|v| v as f32).collect()
```

This is intentional, documented, and auditable. The precision loss is acceptable for
unit-normalized BGE embeddings where all values are in `[-1, 1]`. Relying on serde's
undocumented implicit f64→f32 narrowing was rejected.

### Startup probe

`OaiEmbedder::probe()` sends a single embed request at startup and returns
`(dim, model_name)` from the response. This:

1. Confirms the embedder is reachable before the first real request.
2. Auto-detects the embedding dimension (avoids misconfigured `LCG_EMBEDDING_DIM`).
3. Records the model name as reported by the sidecar (not the env-var value).

`LCG_EMBEDDING_DIM` is retained as a fallback override for environments where the
probe cannot succeed at startup.

## Consequences

- **Backward-incompatible**: `embedder_server.py` used the old `{"text":...}` contract
  and is no longer called by this binary. It can be removed from `liminis-framework`
  in a separate follow-up.
- **hyper coupling**: the explicit `hyper = "1"` workspace dep must be kept in sync with
  whatever reqwest pulls. If reqwest upgrades to hyper 2.x, the explicit dep must be
  bumped deliberately — the compile error is the signal.
- **Startup latency**: the probe adds one round-trip latency at startup. If the sidecar
  is slow to start, `liminis-context-graph` will block. This is acceptable (fail-fast
  over silent degradation), but operators should ensure the sidecar is fully ready
  before launching the graph service.
- **Linux/CI compatibility**: the UDS transport is `#[cfg(unix)]`. On non-Unix platforms,
  only HTTP transport is available. The default resolution falls back to `LCG_EMBEDDING_URL`
  or `http://127.0.0.1:8765/v1/embeddings` on non-Unix.
