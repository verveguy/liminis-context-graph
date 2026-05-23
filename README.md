# liminis-graph

A thin Rust knowledge-graph service over [LadybugDB](https://github.com/lbugdb/lbug) — the logical successor to the Python [graphiti](https://github.com/getzep/graphiti) fork that currently backs Liminis.

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
cargo run -p liminis-graph -- /path/to/graph.db
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
| `GRAPHITI_SOCKET_PATH` | Yes | Unix socket path the IPC daemon listens on |
| `GRAPHITI_DB_PATH` | Yes | Path to the LadybugDB database file |
| `GRAPHITI_EMBEDDING_URL` | No | Base URL for the HTTP embedder sidecar (default `http://127.0.0.1:8765`) |
| `GRAPHITI_EMBEDDING_MODEL` | No | Embedding model name (default `bge-base-en-v1.5`) |
| `GRAPHITI_EMBEDDING_DIM` | No | Embedding vector dimension (default 768) |
| `GRAPHITI_EXTRACTION_LLM` | No | LLM model for entity extraction, optional `primary:fallback` format |
| `GRAPHITI_DEDUP_LLM` | No | If set, enables local dedup adapter |
| `GRAPHITI_WAL_DIR` | No | Directory for write-ahead log JSONL files |
| `ANTHROPIC_API_KEY` | No | API key for Anthropic extraction/classification LLM calls |
| `LIMINIS_WORKSPACE_ROOT` | No* | Absolute path to the workspace root. **Required** for all three corrections IPC methods (`knowledge_validate_corrections`, `knowledge_apply_corrections`, `knowledge_reprocess_entity_types`). If unset, those methods return a `-32000` error. The corrections file is read from `{LIMINIS_WORKSPACE_ROOT}/.liminis/knowledge-corrections.yaml`. |

## Dependencies

| Crate | Version | Role |
|-------|---------|------|
| `lbug` | `=0.16.1` | LadybugDB Rust bindings (pinned) |
| `thiserror` | `2` | Error type generation |

No ML-runtime dependencies (`tch`, `candle`, `onnxruntime`) are permitted — embeddings are produced out-of-process.

## Embedder Sidecar

`HttpEmbedder` delegates embedding to an external HTTP service. You must start the embedder sidecar **before** starting the liminis-graph binary. Without it, the following five IPC methods fail immediately with an HTTP connection error:

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

The sidecar binds to `GRAPHITI_EMBEDDING_URL` (default `http://127.0.0.1:8765`). It logs model loading progress to stderr. Poll `GET /health` to confirm readiness before starting liminis-graph:

```bash
until curl -sf http://127.0.0.1:8765/health | grep -q '"ok": *true'; do
  echo "waiting for embedder…"; sleep 1
done
```

See [ADR 0044](docs/adr/0044-embedder-http-contract.md) for the full HTTP contract specification.

## Architecture decisions

See [`docs/adr/`](docs/adr/) for recorded architecture decisions. The project constitution lives at [`.specify/memory/constitution.md`](.specify/memory/constitution.md).
