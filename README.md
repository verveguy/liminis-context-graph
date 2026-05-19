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

## Dependencies

| Crate | Version | Role |
|-------|---------|------|
| `lbug` | `=0.16.1` | LadybugDB Rust bindings (pinned) |
| `thiserror` | `2` | Error type generation |

No ML-runtime dependencies (`tch`, `candle`, `onnxruntime`) are permitted — embeddings are produced out-of-process.

## Architecture decisions

See [`docs/adr/`](docs/adr/) for recorded architecture decisions. The project constitution lives at [`.specify/memory/constitution.md`](.specify/memory/constitution.md).
