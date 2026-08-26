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

## Selecting a mode

The sidecar serves both `/v1/embeddings` and `/v1/chat/completions` by default. Set
`LOCAL_INFERENCE_MODE` to run only one:

| `LOCAL_INFERENCE_MODE` | Serves | Setup required |
|---|---|---|
| `embeddings` | `/v1/embeddings` only | CoreML model (see "First-time setup" below) |
| `completions` | `/v1/chat/completions` only | **none** |
| `both` (default, or unset) | Both endpoints | CoreML model (see "First-time setup" below) |

`completions`-only mode needs no setup step at all: it skips the embedding-model
presence check, never loads a tokenizer, never compiles a `.mlmodelc`, and never
requires `LOCAL_INFERENCE_HF_CACHE`. A request to a route disabled by the selected
mode returns `404` with `error.type: "invalid_request_error"` — distinguishable from
the `503`/`server_error` a route returns when it's enabled but currently unavailable
(e.g. a missing embedding model in `embeddings`/`both` mode, or Apple Intelligence
being off in `completions`/`both` mode).

An invalid `LOCAL_INFERENCE_MODE` value fails fast at startup with a clear error,
rather than silently falling back to `both`.

**Foundation-Models-backed completions are not a substitute for the extraction
backend.** `LOCAL_INFERENCE_MODE=completions` only affects this sidecar's own
routes; it has no bearing on `liminis-context-graph`'s `--extractor-uds` flag, which
has no default-socket auto-detection by design (see
[ADR-0041](../../docs/adr/0041-local-openai-compatible-extraction-adapter.md)) —
this sidecar's Foundation Models backend is not recommended for extraction quality
(see [Extractor: local or hosted](../../docs/configuration.md#extractor-local-or-hosted)).

## First-time setup

Required for `embeddings` and `both` modes only — `completions`-only mode needs no
setup step at all (see "Selecting a mode" above).

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

`.github/workflows/swift.yml` runs `swift build` and `swift test` on every PR and push that touches `native/local-inference/**`, on a `macos-26` runner. Still run `swift test` locally before pushing — CI is a backstop, not a substitute for catching failures early.

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

`liminis-context-graph` is the **sole source of truth** for this sidecar's source. The Liminis app (`verveguy/liminis`) does not carry its own copy of `native/local-inference/` — it obtains a built binary via the distribution mechanism below. See [ADR-0503](../../docs/adr/0503-swift-sidecar-source-of-truth.md) for the consolidation history and rationale.

## Distribution

`verveguy/liminis` (or anyone else who wants a `LocalInference` binary without building it themselves) has three options, in order of convenience:

1. **Download the latest release asset.** Every `local-inference-v*` [GitHub Release](../../releases) on this repo carries `local-inference-aarch64-apple-macos.tar.gz` (a single self-contained `local-inference` binary) and its `.sha256` checksum:

   ```bash
   curl -L https://github.com/verveguy/liminis-context-graph/releases/download/<TAG>/local-inference-aarch64-apple-macos.tar.gz -o local-inference.tar.gz
   tar -xzf local-inference.tar.gz
   ```

   To discover the latest tag programmatically, filter client-side by tag prefix — this repo also publishes dotted-version Rust releases via `release.yml`/cargo-dist, so an unfiltered "most recent release" call (e.g. `GET /repos/.../releases?per_page=1`, which has no server-side tag-pattern filter) can just as easily return one of those instead:

   ```bash
   gh release list --repo verveguy/liminis-context-graph --limit 50 | awk -F'\t' '$3 ~ /^local-inference-v/' | head -1
   # or: gh api /repos/verveguy/liminis-context-graph/releases --jq '[.[] | select(.tag_name | startswith("local-inference-v"))][0].tag_name'
   ```

2. **Manually trigger the release workflow.** [`.github/workflows/swift-release.yml`](../../.github/workflows/swift-release.yml) is a `workflow_dispatch`-only GitHub Actions workflow: builds the sidecar in release mode on `macos-latest`, runs `swift test`, and publishes the binary to a new `local-inference-v<N>`-tagged Release. Trigger it from the Actions tab (or `gh workflow run swift-release.yml`) when you need a fresh build without waiting for someone else to cut one.

3. **Build locally.** Follow "Build and run" below on a macOS 26 / Swift 6.2+ machine.

This is deliberately **not** a tag-push-triggered release: this repo's `release.yml` (the Rust binary's cargo-dist pipeline) already triggers on any tag containing a `digit.digit.digit` substring, so a dotted version tag for the sidecar could collide with it. Release tags for this sidecar always use the undotted `local-inference-v<integer>` scheme instead — see ADR-0503 for the full reasoning.
