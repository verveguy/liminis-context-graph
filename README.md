# Liminis Context Graph

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A purpose-built Rust knowledge-graph service over [LadybugDB](https://github.com/lbugdb/lbug). Distinct product from the upstream Python graphiti-core library — different language, different DB engine, different concurrency model, different API surface, different LLM routing.

## Goals

- Preserve the IPC surface liminis-framework consumes from the current Python service (drop-in replacement behind a feature flag).
- Reuse existing on-disk artifacts (WAL JSONL, LadybugDB files) without migration.
- Ship as both a library crate and a standalone IPC binary so non-Liminis projects can adopt it.

## Non-goals (v1)

- Replacing the Node-side MCP servers or chat-agent retrieval glue.
- Drivers other than LadybugDB.
- In-process LLM / embedding models (kept as out-of-process adapters).
- Hosted / multi-tenant deployment.

## Quickstart

```bash
# Build both crates
cargo build --release

# Run the integration test (validates LadybugDB round-trip)
cargo test -p liminis-graph-core

# Run the example consumer (ingests 3 docs, searches, prints results)
cargo run --example basic_ingest -p liminis-graph-core

# Run the binary stub
cargo run -p liminis-context-graph
```

## Workspace layout

```
liminis-graph-core/          # library crate — all DB interaction
liminis-graph-core/benches/  # performance benchmarks (criterion)
liminis-graph/               # binary crate — IPC service (depends on core)
examples/                    # standalone consumers demonstrating the library API
docs/adr/                    # architecture decision records
specs/                       # feature specifications
```

## Configuration (environment variables)

| Variable | Required | Description |
|----------|----------|-------------|
| `LCG_SOCKET_PATH` | No | Unix socket path the IPC daemon listens on (default `.lcg/service.sock`) |
| `LCG_DB_PATH` | No | Path to the LadybugDB database file (default `.lcg/db/liminis.db`) |
| `LCG_EMBEDDING_URL` | No | Fallback HTTP URL used when neither `--embedder-uds` nor `--embedder-http` is passed and the default UDS socket (`/tmp/liminis-inference.sock`) is absent. On Unix, if this var is also unset, the binary exits with an error. On non-Unix, defaults to `http://127.0.0.1:8765/v1/embeddings`. |
| `LCG_EMBEDDING_MODEL` | No | Embedding model name sent in requests (default `bge-base-en-v1.5`) |
| `LCG_EMBEDDING_DIM` | No | Embedding dimension override if probe fails at startup (default: auto-detected via probe) |
| `LCG_EXTRACTION_LLM` | No | LLM model for entity extraction, optional `primary:fallback` format |
| `LCG_DEDUP_LLM` | No | If set, enables local dedup adapter |
| `LCG_DEDUP_ADAPTER_URL` | No | URL for the local dedup HTTP adapter (default `http://127.0.0.1:8767`) |
| `LCG_WAL_DIR` | No | Directory for write-ahead log JSONL files |
| `LCG_WAL_MAX_BYTES_PER_FILE` | No | Per-file byte-size rotation threshold for the WAL (default `5242880` = 5 MB); set to `0` to disable byte-size rotation and rely on event count only |
| `LCG_WAL_MAX_EVENTS_PER_FILE` | No | Per-file event-count rotation threshold for the WAL (default `10000`); rotation fires when either this threshold or `LCG_WAL_MAX_BYTES_PER_FILE` is reached |
| `ANTHROPIC_API_KEY` | No | API key for Anthropic extraction/classification LLM calls |
| `LIMINIS_WORKSPACE_ROOT` | No* | Absolute path to the workspace root. **Required** for all three corrections IPC methods (`knowledge_validate_corrections`, `knowledge_apply_corrections`, `knowledge_reprocess_entity_types`). If unset, those methods return a `-32000` error. The corrections file is read from `{LIMINIS_WORKSPACE_ROOT}/.liminis/knowledge-corrections.yaml`. |

> **Phase A migration**: `GRAPHITI_*` env var names are still accepted as fallbacks with a deprecation warning. They will be removed in Phase B. Rename your `.env` entries to `LCG_*` at your convenience.
>
> **Workspace directory**: Fresh installs create `.lcg/` in the working directory. Existing workspaces with `.graphiti/` are automatically renamed to `.lcg/` on first startup.

## Ontology

liminis-graph supports an **optional workspace-scoped ontology** that declares the entity types and relation types the LLM should use during extraction. Without an ontology, the LLM derives types ad-hoc (free-form behavior). With one, vocabulary is consistent and queryable across all chunks.

### File location

Place the ontology at `{LIMINIS_WORKSPACE_ROOT}/.lcg/ontology.yaml`. The older path `.graphiti/ontology.yaml` is also accepted as a fallback.

**Requires a service restart to take effect.** The ontology is loaded once at startup and held in memory. Editing the file while the service runs has no effect until the next restart.

### Format

```yaml
# mode: open | strict
# open (default): declared types are preferred; free-form fallback allowed
# strict: entities and edges outside the vocabulary are dropped post-extraction
mode: strict

entity_types:
  - name: Person           # normalized to PascalCase
    description: A human individual, not a role or title.
  - name: Organization
  - name: Paper

relation_types:
  - name: AUTHORED         # normalized to SCREAMING_SNAKE_CASE
    description: A person wrote a paper.
    source_type: Person    # optional signature constraint (informational in v1)
    target_type: Paper
  - name: AFFILIATED_WITH
    source_type: Person
    target_type: Organization
```

See [`docs/examples/ontology.example.yaml`](docs/examples/ontology.example.yaml) for a fully annotated scientific-paper-domain example.

### Modes

| Mode | Entity types | Relation types |
|------|-------------|----------------|
| `open` (default) | Preferred by the LLM; free-form fallback allowed | Same |
| `strict` | Out-of-vocabulary entities dropped post-extraction | Out-of-vocabulary edges dropped (requires #82) |

### `knowledge_status` summary

The `knowledge_status` IPC response always includes an `ontology` field:

```json
{
  "ontology": {
    "present": true,
    "mode": "strict",
    "entity_type_count": 4,
    "relation_type_count": 4
  }
}
```

When no ontology is loaded, `present` is `false` and counts are `0`.

## Dependencies

| Crate | Version | Role |
|-------|---------|------|
| `lbug` | `=0.16.1` | LadybugDB Rust bindings (pinned) |
| `thiserror` | `2` | Error type generation |

No ML-runtime dependencies (`tch`, `candle`, `onnxruntime`) are permitted — embeddings are produced out-of-process.

## Embedder Sidecar

`OaiEmbedder` delegates embedding to an external service over the OpenAI-compatible
`POST /v1/embeddings` contract. The binary supports two transports, selected via CLI flags:

```
liminis-context-graph --embedder-uds /tmp/liminis-inference.sock   # Unix domain socket (default on macOS)
liminis-context-graph --embedder-http http://127.0.0.1:8765/v1/embeddings  # HTTP
```

**Default behaviour** (no flags): the binary looks for the Swift CoreML sidecar socket at
`/tmp/liminis-inference.sock`. If absent, it falls back to `LCG_EMBEDDING_URL` (HTTP). If
neither exists, it exits with a clear error.

The binary probes the embedder at startup to confirm it is reachable and auto-detect the
embedding dimension. If the probe fails and `LCG_EMBEDDING_DIM` is not set, the process
exits with an error rather than failing silently on the first embed request.

You must start the embedder sidecar **before** starting the liminis-context-graph binary.
Without it, the following five IPC methods fail immediately with an embedding error:

- `knowledge_find_entities`
- `knowledge_find_relationships`
- `knowledge_search_passages`
- `knowledge_process_chunk`
- `knowledge_reprocess_entity_types`

Read-only methods that do not call the embedder (`health_check`, `knowledge_status`,
`knowledge_list_entities`, `knowledge_get_episodes`) continue to work without the sidecar.

### macOS: Swift CoreML sidecar (default)

This repository ships a Swift CoreML sidecar at [`native/local-inference/`](native/local-inference/)
that serves OpenAI-compatible `/v1/embeddings` (BGE-base-en-v1.5) and `/v1/chat/completions`
(Apple Foundation Models) over UDS at `/tmp/liminis-inference.sock`. macOS 26+ and Xcode
command-line tools are required. See [`native/local-inference/README.md`](native/local-inference/README.md)
for build and run instructions.

`liminis-context-graph` discovers the sidecar's default UDS socket automatically — start the
sidecar first, then start the binary.

### HTTP transport (CI / Linux / custom embedders)

For environments without the Swift sidecar, pass `--embedder-http` pointing at any
OpenAI-compatible embedding endpoint (local or remote):

```bash
liminis-context-graph --embedder-http http://127.0.0.1:8765/v1/embeddings
```

See [ADR 0044](docs/adr/0044-embedder-http-contract.md) and
[ADR 0048](docs/adr/0048-oai-embedding-contract-uds-transport.md) for the wire contract
specification and transport decision record.

## Architecture decisions

See [`docs/adr/`](docs/adr/) for recorded architecture decisions. The project constitution lives at [`.specify/memory/constitution.md`](.specify/memory/constitution.md).

## Contributing

Contributions are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for how to file issues, submit pull requests, and the required pre-commit checks. No CLA or DCO sign-off is required — contributions are accepted under the project's MIT license by inbound=outbound convention.

## Security

To report a security vulnerability, please use [GitHub's private vulnerability reporting](https://github.com/verveguy/liminis-graph/security/advisories/new) rather than filing a public issue. See [`SECURITY.md`](SECURITY.md) for supported versions, response time, and disclosure policy.
