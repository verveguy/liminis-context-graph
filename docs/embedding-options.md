---
layout: default
title: Embedding Options
---

# Embedding Options

**What should I run, on my platform, and what will it cost me?** This page answers that
question in one read. It is a capability matrix and a set of pointers, not a restatement of
the full reference — see [Configuration: Embedder sidecar](configuration.md#embedder-sidecar)
for every flag, environment variable, and resolution-order detail.

`liminis-context-graph` needs an external embedding backend reachable at startup. It talks to
that backend over the OpenAI-compatible `POST /v1/embeddings` contract, selected with either
`--embedder-uds <path>` (Unix domain socket) or `--embedder-http <url>` (HTTP).

## Capability matrix

| Option | Platform(s) | Install | Setup / model cost | Dimension | Flag | Offline? | Dimension auto-probe? | Unreachable at startup | Verification |
|---|---|---|---|---|---|---|---|---|---|
| **Swift CoreML sidecar** | macOS 26+ | Xcode command-line tools; build from [`native/local-inference/`](https://github.com/verveguy/liminis-context-graph/tree/main/native/local-inference) | ~500 MB disk for the BGE-base-en-v1.5 CoreML model, downloaded on first setup; no per-call cost | 768 (fixed, BGE-base-en-v1.5) | `--embedder-uds` (or no flag — auto-detected at `/tmp/liminis-inference.sock`) | Yes, fully local | Yes | Fail-fast (socket-service mode) or bounded retry-and-degrade (`--mcp-stdio`) — see [Embedder sidecar](configuration.md#embedder-sidecar) | Verified — the [`swift test`](https://github.com/verveguy/liminis-context-graph/blob/main/native/local-inference/README.md#tests) suite, and [`verify-embedding-parity.py`](https://github.com/verveguy/liminis-context-graph/blob/main/native/local-inference/verify-embedding-parity.py) (cosine parity vs. the PyTorch reference, run manually — not yet wired into CI) |
| **Infinity** (Python, sentence-transformers-based) | Linux, macOS, Windows | Install and run [Infinity](https://github.com/michaelfeil/infinity) yourself; no download bundled with this repo | Depends on the model you load; Infinity itself is free/open-source, no per-call cost | Varies (model-chosen) | `--embedder-http` | Yes, fully local | Yes | Fail-fast or retry-and-degrade, same as above | Not independently verified in this repo — relies on the generic OpenAI-compatible contract. Listed as an unauthenticated-compatible server in [Configuration](configuration.md#http-transport-ci--linux--custom-embedders--hosted-providers) |
| **Ollama** | Linux, macOS, Windows | Install [Ollama](https://ollama.com), pull an embedding model (e.g. `nomic-embed-text`) | Model download only (varies by model, typically a few hundred MB); no per-call cost | Varies (model-chosen) | `--embedder-http` | Yes, fully local | Yes | Fail-fast or retry-and-degrade, same as above | Not independently verified in this repo — relies on the generic OpenAI-compatible contract. Listed with a concrete recipe in [Configuration: MCP client config recipes](configuration.md#mcp-client-config-recipes) |
| **vLLM / text-embeddings-inference (TEI)** | Linux (GPU recommended); macOS/Windows via Docker | Install vLLM or TEI, load a model | Model download (varies); no per-call cost; GPU strongly recommended for throughput | Varies (model-chosen) | `--embedder-http` | Yes, fully local | Yes | Fail-fast or retry-and-degrade, same as above | Not independently verified in this repo — relies on the generic OpenAI-compatible contract. Listed in [Configuration: MCP client config recipes](configuration.md#mcp-client-config-recipes) |
| **Hosted OpenAI-compatible provider** (e.g. OpenAI `/v1/embeddings`) | Any (network required) | An API key; no local install | No download; per-call pricing set by the provider | Model-chosen (e.g. 1536 for `text-embedding-3-small`) | `--embedder-http` + `LCG_EMBEDDING_API_KEY` (or `OPENAI_API_KEY`) | No — requires network | Yes | Fail-fast — an auth rejection (401/403) is always fatal and is **not** bypassable via `LCG_EMBEDDING_DIM` | OpenAI's own `/v1/embeddings` is **the concrete, verified case** — see [Configuration: HTTP transport](configuration.md#http-transport-ci--linux--custom-embedders--hosted-providers). Other self-described OpenAI-compatible hosted providers are not verified beyond this. |

Every non-macOS, non-OpenAI-hosted row above is caveated deliberately: none of them has been
run against a live instance and tested end-to-end in this repository. Their rows exist because
the wire contract they claim to speak matches what `OaiEmbedder` sends and expects — not
because this repo has executed an integration test against them. Treat that as "should work,
matches the documented shape," not "verified working here."

## Any OpenAI-compatible endpoint is a valid target

The rows above are examples, not an exhaustive list. **Any server that accepts
`POST /v1/embeddings` with an OpenAI-shaped `{input, model}` request and returns an
OpenAI-shaped `{data: [{embedding}]}` response is a valid `--embedder-http` target.** This is
the actual extension point — if your preferred embedding server speaks that contract, point
`--embedder-http` at it and it will work, whether or not it's named above. See
[ADR 0006](adr/0006-embedder-http-contract.md) and
[ADR 0016](adr/0016-oai-embedding-contract-uds-transport.md) for the wire contract itself.

## No bundled cross-platform option

If you are on Linux or Windows: **there is currently no bundled, zero-setup embedder for your
platform.** The Swift CoreML sidecar is macOS-only (it depends on CoreML and, for extraction,
Apple Foundation Models — neither exists on other platforms). You must run one of the external
options above (or any other OpenAI-compatible server) yourself and point
`liminis-context-graph` at it with `--embedder-http`.

This isn't an oversight: a native, in-process, cross-platform Rust embedder was evaluated and
rejected. See
[`docs/spikes/native-embedder-2026-05.md`](https://github.com/verveguy/liminis-context-graph/blob/main/docs/spikes/native-embedder-2026-05.md)
for the full spike — `candle` was a **NO-GO**, `ort` was **GO-with-caveats**, and neither was
ever built into production. Out-of-process, over the same wire contract as every other row in
the matrix above, remains the supported path on every platform.

## Switching embedders on an existing workspace is not a drop-in swap

The embedding dimension is fixed into the graph's LadybugDB schema (`FLOAT[dim]`) at
database-creation time. `liminis-context-graph` auto-probes the dimension of whatever embedder
you configure at startup; `LCG_EMBEDDING_DIM` can override a **probe failure**, but it cannot
resolve a genuine mismatch between a newly-configured embedder's dimension and vectors already
stored under the old one.

If you switch to a different-dimension embedder against a workspace that already has content
in its `.lcg/` database, that is not something you can just reconfigure and restart into — see
[Configuration: Switching an existing workspace's embedder](configuration.md#switching-an-existing-workspaces-embedder)
for the remedy (full re-ingest, or `knowledge_recover` with `rebuild_from_workspace_wal`).

**Even a same-dimension model swap invalidates stored vectors.** Changing the embedding
*model* — even to one that happens to produce the same dimension — means your existing vectors
were computed by a different model and are no longer comparable to freshly-embedded ones.
Issue [#440](https://github.com/verveguy/liminis-context-graph/issues/440) added model-identity
stamping and mismatch detection for exactly this case (`knowledge_status`'s
`embedding_model_status` field, and the `embeddings_recompute_failed` /
`embeddings_recompute_skipped_no_text` counters) — see
[Operations: `knowledge_status` health fields](operations.md#knowledge_status-health-fields)
for the full mechanics.

## Embedding is not extraction

This page is about **embedding** — the `--embedder-uds`/`--embedder-http` flags above. It has
nothing to do with **extraction**, the separate process of turning text into entities and
relationships, configured independently via
`--extractor-uds`/`--extractor-http`/`ANTHROPIC_API_KEY`. On macOS, the same sidecar process
can happen to serve both endpoints (`/v1/embeddings` for embedding, `/v1/chat/completions` for
extraction), which makes the two easy to conflate — but they are selected, resolved, and can
fail independently of each other. See
[Configuration: Extractor: local or hosted](configuration.md#extractor-local-or-hosted) for
full extraction configuration guidance.

## The macOS socket path is not shared with the Electron app

`liminis-context-graph`'s own default UDS auto-discovery path is
`/tmp/liminis-inference.sock` — a single, well-known, machine-wide path. The Electron
`liminis` app does **not** use that path: it launches its own sidecar process bound to a
separate, per-workspace socket at `<workspaceRoot>/.liminis/local-inference.sock`.

**These are two different sockets serving two different processes' sidecars — never one shared
socket.** A `liminis-context-graph` binary started by hand does not see, and cannot reach, the
sidecar the Electron app started for its own workspace, and vice versa. If you run a manual
CLI session alongside the app, expect to start (or point at) your own sidecar rather than
assuming the app's is reachable at the well-known path.
