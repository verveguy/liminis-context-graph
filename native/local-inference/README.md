# local-inference — macOS Swift sidecar for liminis-context-graph

A small HTTP service that runs on-device CoreML embeddings (BGE-base-en-v1.5) and Apple Foundation Models chat completions. Exposes both behind an OpenAI-compatible API so `liminis-context-graph` (and any other OpenAI-compatible client) can consume them over UDS or HTTP.

**macOS 26+ only.** Foundation Models is a macOS 26 framework; the package's `Package.swift` requires it. There is no Linux or Windows equivalent in this package — non-Mac users should run a different OpenAI-compatible embedder / LLM (Python sentence-transformers, Ollama, vLLM, a cloud API, etc.) and point `liminis-context-graph` at it via `--embedder-http` and the OpenAI-compatible LLM env vars.

## What it provides

- `POST /v1/embeddings` — CoreML BGE-base-en-v1.5, 768-dim, batched
- `POST /v1/chat/completions` — Apple Foundation Models on-device LLM

Both endpoints follow OpenAI's wire shapes. The embeddings contract is documented in [`docs/adr/0006-embedder-http-contract.md`](../../docs/adr/0006-embedder-http-contract.md) and the UDS transport in [`docs/adr/0016-oai-embedding-contract-uds-transport.md`](../../docs/adr/0016-oai-embedding-contract-uds-transport.md).

## Requirements

- macOS 26+ (Foundation Models framework)
- Swift 6.2+ (ships with current Xcode)
- Xcode command-line tools (`xcode-select --install`)
- ~500 MB of disk for the BGE-base CoreML model (downloaded on first setup, not committed)

## First-time setup

The CoreML model file (`bge-base-en-v1.5.mlpackage`, ~400 MB) is not committed to the repo. It is downloaded and converted on first use:

```bash
cd native/local-inference
./prepare-embedding-assets.sh
```

This downloads the BGE-base-en-v1.5 model from HuggingFace and converts it to the CoreML format the sidecar expects. The result lives in `native/local-inference/bge-base-en-v1.5.mlpackage/` and is `.gitignore`-d.

Subsequent runs reuse the cached model.

## Build and run

```bash
cd native/local-inference
swift build -c release

# Default: listens on UDS at /tmp/liminis-inference.sock
.build/release/LocalInference
```

`liminis-context-graph` discovers this socket automatically:

```bash
# In a separate terminal
liminis-context-graph
# Or explicitly:
liminis-context-graph --embedder-uds /tmp/liminis-inference.sock
```

## Tests

```bash
cd native/local-inference
swift test
```

Tests use stub `.mlpackage` fixtures (under `Tests/LocalInferenceTests/Fixtures/`) so the real model is not required to run them. Tokenizer fixtures live under `Tests/LocalInferenceTests/Fixtures/tokenizer-cache/` and ship in-tree.

If you need to regenerate the stub fixtures (after a real schema change), run:

```bash
./refresh-test-fixtures.sh
```

## Dependencies and licenses

All Swift package dependencies are Apache 2.0 (mostly `github.com/apple/swift-*`, plus [hummingbird](https://github.com/hummingbird-project/hummingbird) and [swift-transformers](https://github.com/huggingface/swift-transformers)) or MIT (jinja). The BGE-base-en-v1.5 model is MIT by [BAAI](https://huggingface.co/BAAI/bge-base-en-v1.5).

Apple Foundation Models is provided by the macOS SDK; usage from third-party software is permitted under standard SDK terms.

See `Package.resolved` for the pinned dep tree.

## Relationship to the main project

This sidecar is an **optional** component. The Rust binary (`liminis-context-graph`) does not depend on it being built or running — it only needs *some* OpenAI-compatible embedder reachable at startup. The sidecar happens to be the easiest such embedder on macOS.

A near-identical copy lives at `liminis-app/native/local-inference/` in the (separate) Liminis app repo, which is its original home. The two copies are intentionally maintained side-by-side for now; future consolidation is tracked separately.
