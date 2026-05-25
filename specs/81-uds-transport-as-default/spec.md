# Feature Specification: liminis-graph speaks OpenAI-compatible embeddings over UDS by default, with HTTP as opt-in

**Feature Branch**: `fabrik/issue-81`
**Created**: 2026-05-25
**Status**: Draft
**Input**: User description: "Make the liminis-graph Rust service speak UDS by default to the Swift CoreML sidecar — but keep HTTP support for future OpenAI / external embedder use. The Rust process accepts a CLI param at startup telling it which transport endpoint to use."

## Background

The Swift CoreML BGE-base sidecar shipped as part of the embedder cutover (liminis#794 + liminis-framework#179). End-to-end validation confirmed it serves `POST /v1/embeddings` (OpenAI-compatible contract) over a Unix domain socket at `/tmp/liminis-inference.sock` with full quality parity and 21–22 ms latency.

However, the cutover only wired the Python `graphiti_service.py` to use it (via the dual-path UDS/python factory in framework PR #181). The Rust `liminis-context-graph` binary — which has since become the active graph backend in the Liminis app — still talks to a **separate** Python embedder over HTTP at `127.0.0.1:8765` via `HttpEmbedder` (`liminis-graph-core/src/embedder.rs`). On a current Liminis install:

- Swift sidecar (PID 4694) runs, serving `/v1/embeddings` on UDS — nothing calls it.
- liminis-context-graph (PID 4721) runs, calling `embedder_server.py` on TCP 8765 (Python sentence-transformers).
- The two services don't connect. The Swift sidecar is orphaned.

This issue closes the gap: teach `HttpEmbedder` to speak the Swift sidecar's OpenAI-compatible contract over UDS, while preserving HTTP as an opt-in transport for future use (e.g., calling a real OpenAI endpoint, or a remote embedder over the network). Transport is selected at startup via a new CLI flag; the request/response shape is OpenAI-style on both transports so the same client code handles both.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Mac end-user embeddings flow through the Swift CoreML sidecar (Priority: P1)

A Liminis end user installs the packaged `.app` on macOS, opens a workspace, adds a document. The knowledge graph indexes the document and every embedding call is served by the local Swift CoreML sidecar. The Python embedder process is no longer launched on Mac.

**Why this priority**: This is the user-visible payoff for the entire embedder cutover. Today, despite the Swift sidecar working in isolation, every embedding call still goes through Python sentence-transformers because the Rust backend doesn't know how to reach the sidecar. Closing this gap completes the cutover that started with liminis#794.

**Independent Test**: With the new `liminis-context-graph` binary in place, launch Liminis on a Mac, add a markdown file to a workspace, watch `~/Library/Logs/Liminis Notes/local-inference.log` for `/v1/embeddings` requests appearing during indexing. Confirm no Python `embedder_server.py` process is spawned by the app's lifecycle code on macOS.

**Acceptance Scenarios**:

1. **Given** a fresh Liminis install on macOS, **When** the app spawns `liminis-context-graph` with the default startup configuration, **Then** the Rust process connects to the Swift sidecar over UDS and routes all embedding calls there.
2. **Given** `liminis-context-graph` is running with UDS transport, **When** a chunk is processed (`knowledge_process_chunk`), **Then** the corresponding `Episodic.content_embedding` row in LadybugDB has a non-null 768-dim L2-normalized vector matching the spike's parity threshold (cosine ≥0.9999 vs reference PyTorch BGE-base).
3. **Given** the UDS socket file is missing at startup, **When** the Rust process starts, **Then** it exits with a clear error naming the path and pointing at the sidecar lifecycle — not at the first embed request.

---

### User Story 2 - Developer or CI can opt back to HTTP transport via CLI flag (Priority: P2)

A contributor working on a Linux dev environment, or a CI runner that doesn't have the Swift sidecar available, can launch `liminis-context-graph` with an explicit CLI flag pointing at an HTTP embedder endpoint. The Rust code path is identical except for the transport layer — same request shape, same response handling, same dimension probe.

**Why this priority**: Keeps the door open for (a) Linux/CI environments where the Swift sidecar isn't available, (b) future use of a real OpenAI embedding endpoint over HTTPS, (c) remote-embedder topologies. Per the user's framing, "OpenAI to be used in future" is the explicit motivation for keeping HTTP.

**Independent Test**: Launch `liminis-context-graph --embedder-http <url>` against any OpenAI-compatible embedding endpoint (could be a local mock, could be the Swift sidecar's UDS path wrapped by `socat`, could be a real `api.openai.com/v1/embeddings`). Confirm embeddings flow through the same code path with no UDS-specific behavior.

**Acceptance Scenarios**:

1. **Given** `liminis-context-graph` started with `--embedder-http http://127.0.0.1:8765/v1/embeddings`, **When** a chunk is processed, **Then** the embed request goes over HTTP using the OpenAI-compatible request body.
2. **Given** the HTTP endpoint returns a malformed response (wrong shape, wrong dim), **When** the Rust process tries to use it, **Then** the error names "embedding response shape mismatch" — distinct from a transport error.

---

### User Story 3 - Maintainer can verify the active transport from the log at startup (Priority: P3)

A maintainer debugging an embedding issue can read `liminis-context-graph`'s startup log and immediately see which transport (UDS or HTTP) is active, the endpoint string, and the negotiated embedding dimension. No process inspection or strace needed.

**Why this priority**: Diagnostic friction multiplier — when something goes wrong with embeddings, the first question is always "which embedder are we hitting?" One log line at startup answers it.

**Acceptance Scenarios**:

1. **Given** `liminis-context-graph` started with either transport, **When** it begins serving IPC, **Then** the service log contains a single line of the form `embedder: transport=<uds|http>, endpoint=<path-or-url>, dim=<N>` before the first request.

---

### Edge Cases

- What happens when **both** `--embedder-uds` and `--embedder-http` flags are passed? The process must exit with a clear error (mutually exclusive options) — never silently prefer one over the other.
- What happens when **neither** flag is passed? The process uses a sensible default for Mac (UDS at `/tmp/liminis-inference.sock`). If that default path doesn't exist at startup and no `LCG_EMBEDDING_URL` fallback is set, the process exits with the missing-socket error from US1 acceptance scenario 3.
- What happens when the negotiated embedding dimension changes mid-run (e.g., the sidecar is restarted with a different model)? Out of scope here — the dim is captured at startup. Mid-run model changes are caught later, when the resulting vector doesn't match the indexed dim and the write fails.
- What happens when the legacy `LCG_EMBEDDING_URL` env var is set but `--embedder-http` is also passed? CLI wins. Env var only used when neither CLI flag is present and the default UDS path doesn't exist — UDS has no env-var equivalent (the path is filesystem-local and not a credential-bearing URL).
- What happens to the old `{"text": ..., "model": ...}` request shape that `embedder_server.py` used? Dropped. After this lands, `embedder_server.py` no longer matches the contract this binary uses — it becomes dead code and should be removed in the companion liminis-framework follow-up.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `liminis-context-graph` (the Rust binary) MUST support UDS as an embedder transport, calling the OpenAI-compatible `POST /v1/embeddings` endpoint over a Unix domain socket.
- **FR-002**: `liminis-context-graph` MUST also support HTTP as an embedder transport, calling the same OpenAI-compatible endpoint at a configured URL. HTTP support is not regressed by this work.
- **FR-003**: Transport selection MUST be controlled by mutually exclusive CLI flags: `--embedder-uds <socket-path>` and `--embedder-http <url>`. Exactly one of these is active at any time.
- **FR-004**: If neither flag is passed, the binary MUST default to UDS at `/tmp/liminis-inference.sock` (the Liminis app's bundled Swift sidecar location).
- **FR-005**: If both flags are passed, the binary MUST exit at startup with a clear, specific error naming the conflict — not silently prefer one.
- **FR-006**: The request and response shape MUST be OpenAI-compatible across both transports (request body `{"input": "...", "model": "..."}`; response body `{"data": [{"embedding": [...], "index": 0}], "model": "...", "usage": {...}}`). The old `{"text": ..., "model": ...}` contract is retired.
- **FR-007**: The legacy `LCG_EMBEDDING_URL` env var MUST continue to work as a fallback for HTTP transport when no CLI flag is passed and the default UDS path does not exist at startup. CLI flags take precedence over env vars; UDS default takes precedence over the env var when the socket exists.
- **FR-008**: The embedding dimension MUST be auto-detected at startup via a probe request to the embedder. The legacy `LCG_EMBEDDING_DIM` env var continues to work as an override when the embedder cannot be probed at startup.
- **FR-009**: At startup, after transport selection and dimension probe, the binary MUST log a single structured line naming the active transport, endpoint, and dim.
- **FR-010**: If the resolved transport is UDS (either explicitly via `--embedder-uds` or by default with no env-var fallback) and the socket file does not exist at startup, the binary MUST exit at startup with a clear error naming the missing path — never at the first embedding request.
- **FR-011**: If the HTTP URL is malformed, or the host is unreachable at the probe stage, the binary MUST exit at startup with a clear error — never at the first embedding request.
- **FR-012**: The integration test suite MUST exercise both transports against a stub embedder serving the OpenAI-compatible contract. Tests skip cleanly on platforms where UDS isn't supported.

### Key Entities

- **Embedder transport**: The mechanism for reaching the embedding service. Either UDS (filesystem socket path) or HTTP (URL). Selected at startup, immutable for the process lifetime.
- **OpenAI-compatible embedding contract**: The wire format the binary uses, regardless of transport. Matches what the Swift sidecar already serves and what `api.openai.com/v1/embeddings` accepts. Documented as a stable contract in this binary, decoupled from any specific server implementation.
- **Embedding dim probe**: A one-shot request at startup that derives the embedder's output dimension (e.g., 768 for BGE-base, 1536 for ada-002). Cached for the process lifetime.
- **`HttpEmbedder` (existing struct)**: Refactored or renamed to reflect that it's transport-pluggable. The implementation acquires its transport from configuration; the call site doesn't care which transport is active.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On a Mac with a default Liminis install and the Swift sidecar bundled, 100% of `knowledge_process_chunk` calls produce non-null 768-dim L2-normalized embeddings, and 100% of those embed requests are served by the Swift sidecar (verifiable by request count in `~/Library/Logs/Liminis Notes/local-inference.log` over a test indexing run).
- **SC-002**: Switching the CLI flag from `--embedder-uds` to `--embedder-http` (and vice versa) requires zero code changes in `liminis-context-graph` — only the launch command differs.
- **SC-003**: Cosine similarity between embeddings produced via UDS and via a reference HTTP endpoint serving the same model is ≥0.9999 on the 50-sentence reference set from the spike — i.e., transport choice does not introduce numerical drift.
- **SC-004**: An invalid CLI flag combination (both flags set, missing socket, malformed URL) produces a startup-time failure with a specific, actionable error message in 100% of attempts. The previously-existing failure mode of "succeeds at startup, fails on first embed request" no longer occurs for these conditions.
- **SC-005**: After this work lands and the companion liminis-app spawn change is also in place, the Python `embedder_server.py` process is not launched by Liminis on macOS. (Confirming this requires the companion app-side change — tracked separately.)

## Assumptions

- The Swift sidecar's `POST /v1/embeddings` contract is stable and is the contract this binary commits to. If the Swift sidecar's contract changes upstream, that becomes a separate breakage tracked elsewhere.
- `tokio` / `hyper` (or whatever HTTP/UDS stack already in use) supports UDS — verify the chosen client library supports Unix-socket connectors. If not, this spec covers the cost of switching to one that does.
- The Liminis app, when spawning `liminis-context-graph`, will pass the new CLI flag with the correct socket path. This is the companion change tracked in a separate issue in `verveguy/liminis` — not in scope here.
- The Python `embedder_server.py` removal (deletion from `liminis-framework`) is tracked separately. This spec only requires that `liminis-context-graph` no longer calls it; whether the file is deleted from disk is a follow-up.
- The integration test stub can be a small Rust HTTP/UDS server in the test crate that serves a deterministic embedding (e.g., echoes a hash of the input as a fake vector). Quality parity is a separate test that runs against a real embedder, not this stub.
- Backwards compatibility for the legacy `{"text": ..., "model": ...}` request shape is **not** preserved. The old shape was only used by `embedder_server.py`, which is being retired.

## Out of Scope *(optional)*

- **Companion liminis-app change**: passing the new CLI flag when spawning `liminis-context-graph`. Tracked in a separate issue in `verveguy/liminis` to be filed once this lands.
- **Removing `embedder_server.py`** from `liminis-framework`. Once Mac no longer needs it, it can be deleted in a separate follow-up in `verveguy/liminis-framework`. Until both lands, it stays in place.
- **Changes to `graphiti_service.py`**: it's already in phase-out per `liminis-graph/ideas/cutover-plan.md`. Phase 4 of that plan deletes it.
- **External OpenAI endpoint validation**: this spec keeps HTTP transport available for that future use, but actually wiring up `api.openai.com` (auth, rate-limiting, billing) is a separate downstream issue.
- **Dynamic transport switching at runtime**: out of scope; transport is fixed at process startup.
- **Multi-embedder routing** (e.g., different embedders for different roles): out of scope; one embedder per process.

## Source References *(optional)*

- `liminis-graph-core/src/embedder.rs:18-78` — current `HttpEmbedder` implementation, the file most of this work touches.
- `liminis-graph-core/src/app_state.rs:13,72` — where `HttpEmbedder::from_env()` is called.
- `liminis-graph-core/tests/ipc_parity.rs:26,57,81,320` — existing tests using `HttpEmbedder`, to extend with UDS coverage.
- `~/dev/liminis-project/liminis/liminis-app/native/local-inference/Sources/LocalInference/AppRouter.swift` — Swift sidecar `/v1/embeddings` handler, the contract this binary connects to.
- `~/dev/liminis-project/liminis-graph/ideas/cutover-plan.md` (Phase 2 prereq #1: embedder lifecycle).
- `verveguy/liminis-framework` PR #181 (F1 cutover for `graphiti_service.py` path — context for why this only covered the Python service, not the Rust one).
- `verveguy/liminis` PR #805 (A1 — tokenizer + mlmodelc cache), PR #818 (#810 — handler dtype fix), PR #820 (spike size addendum).
- Live process snapshot (2026-05-25): Swift sidecar PID 4694 unused; `liminis-context-graph` PID 4721 calling `embedder_server.py` PID 82706 on TCP 8765.
