# Liminis Context Graph

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
| `LCG_EMBEDDING_URL` | No | Base URL for the HTTP embedder sidecar (default `http://127.0.0.1:8765`) |
| `LCG_EMBEDDING_MODEL` | No | Embedding model name (default `bge-base-en-v1.5`) |
| `LCG_EMBEDDING_DIM` | No | Embedding vector dimension (default 768) |
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

## Dependencies

| Crate | Version | Role |
|-------|---------|------|
| `lbug` | `=0.16.1` | LadybugDB Rust bindings (pinned) |
| `thiserror` | `2` | Error type generation |

No ML-runtime dependencies (`tch`, `candle`, `onnxruntime`) are permitted — embeddings are produced out-of-process.

## Embedder Sidecar

`HttpEmbedder` delegates embedding to an external HTTP service. You must start the embedder sidecar **before** starting the liminis-context-graph binary. Without it, the following five IPC methods fail immediately with an HTTP connection error:

- `knowledge_find_entities`
- `knowledge_find_relationships`
- `knowledge_search_passages`
- `knowledge_process_chunk`
- `knowledge_reprocess_entity_types`

Read-only methods that do not call the embedder (`health_check`, `knowledge_status`, `knowledge_list_entities`, `knowledge_get_episodes`) continue to work without the sidecar.

### Sidecar location

The sidecar script lives in the `liminis-framework` repository at:

```
framework/src/skills/knowledge-graph/scripts/embedder_server.py
```

### Starting manually

```bash
# From the liminis-framework checkout:
uv run framework/src/skills/knowledge-graph/scripts/embedder_server.py
```

The sidecar binds to `LCG_EMBEDDING_URL` (default `http://127.0.0.1:8765`). It logs model loading progress to stderr.

**Cold-start time**: `bge-base-en-v1.5` takes typically **5–15 s** to load on CPU (warm HuggingFace cache). The first run includes a ~500 MB model download from HuggingFace Hub, which adds variable time depending on network speed.

Poll `GET /health` to confirm readiness before starting liminis-context-graph:

```bash
until curl -sf http://127.0.0.1:8765/health | grep -q '"ok": *true'; do
  echo "waiting for embedder…"; sleep 1
done
```

See [ADR 0044](docs/adr/0044-embedder-http-contract.md) for the full HTTP contract specification.

## Architecture decisions

See [`docs/adr/`](docs/adr/) for recorded architecture decisions. The project constitution lives at [`.specify/memory/constitution.md`](.specify/memory/constitution.md).
