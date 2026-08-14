# Liminis Context Graph

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**A local-first context graph engine.** One Rust binary that turns a stream of text into a queryable graph of entities, relationships, and episodes — combining property-graph storage, HNSW vector search, and full-text search in a single embedded service. No database server, no separate vector store, no search cluster: everything runs in one process, on your machine, against files in your workspace.

Originally inspired by the knowledge-graph ideas in [graphiti](https://github.com/getzep/graphiti), then deliberately narrowed: instead of a general framework over pluggable backends, `liminis-context-graph` is a purpose-built engine with one storage layer, one wire protocol, and a local-first design from top to bottom.

## Why

- **One embedded engine.** [LadybugDB](https://github.com/lbugdb/lbug) (the community continuation of KuzuDB) provides the property graph, HNSW vector indices, and full-text search in a single embedded database — no server process, no network hop, data in ordinary files under your workspace.
- **The write-ahead log is the source of truth — and it's just JSON.** Every mutation is appended to plain JSONL files under `.lcg/wal/` before it touches the database. The database is a derived index — delete it and `knowledge_rebuild_from_wal` reconstructs the entire graph from the log.
- **One database, many graphs.** `.lcg/wal/` is a WAL *root*: each `group_id` owns its own stream in `.lcg/wal/<group_id>/`, independently replayable and independently discardable. One process can hold several graphs at once without them bleeding into each other, and because a stream is just a directory of JSONL files it can be versioned, shipped, and replayed somewhere else.
- **Models stay out of process.** Embedding and LLM inference are reached through narrow adapters over the OpenAI-compatible `/v1/embeddings` and `/v1/chat/completions` shapes. Embedding runs fully local out of the box on macOS; extraction can run fully local too, or against the hosted Anthropic API.

The result is a context graph you can treat like the rest of your local tooling: a single process, a directory of files, versionable with git, rebuildable from its own log.

## How it works

```
                      ┌─────────────────────────────────────────────────┐
   text chunks        │  liminis-context-graph (one process)            │
  ──────────────────► │                                                 │
   JSON-RPC 2.0       │  extraction LLM ──► entities + relations        │
   over Unix socket   │  (out-of-process)   dedup + resolution          │
                      │                          │                      │
   search queries     │           1. append ┌────▼───────┐ .lcg/wal/    │
  ──────────────────► │           ────────► │ WAL (JSONL)│  <group_id>/ │
   hybrid results     │                     └────┬───────┘ one stream   │
  ◄────────────────── │           2. apply       │         per graph,   │
                      │                          │         source of    │
                      │                          │         truth        │
                      │           ────────► ┌────▼───────┐              │
                      │                     │ LadybugDB  │ .lcg/db/     │
                      │  embedder sidecar   │ graph+HNSW │ derived      │
                      │  (out-of-process)   │ +FTS       │ index        │
                      └─────────────────────┴────────────┴──────────────┘
```

**Ingestion**: `knowledge_process_chunk` sends a chunk of text through the extraction LLM, which returns typed entities and relationships (optionally constrained by your [ontology](https://v3rv.com/liminis-context-graph/ontology)). New facts are deduplicated against the existing graph, appended to the WAL, then written to the database with embeddings from the sidecar. Every chunk becomes a time-stamped **episode** linked to the facts it produced.

**Search** is hybrid by default: `knowledge_find_entities` and `knowledge_find_relationships` combine full-text and vector similarity; `knowledge_search_passages` does semantic passage retrieval; `knowledge_get_entity_neighbors` and `knowledge_query_cypher` traverse the graph directly.

**Graphs are separated by `group_id`, and each one owns its WAL stream.** A `group_id` is a real isolation boundary, not a filter applied after the fact: entity resolution, dedup, merge, and purge are all scoped to it, so one graph's ingest cannot rewrite or delete another's data. Each group's mutations land in `.lcg/wal/<group_id>/`, which makes a single graph independently replayable (`knowledge_rebuild_from_wal`), independently disposable (`knowledge_delete_by_group`), and independently shippable — a stream is a directory of JSONL files, so it can be committed to git, distributed, and hydrated into someone else's database.

That is what makes **replication and layering** practical. A consumer can hydrate several upstream streams into one local database, each keeping its own `group_id`, and still query any one of them in isolation or all of them together. A stream carries a **generation identity** (`wal.generation`) so a consumer can tell a forward advance from a reset and never mistakes a rebuilt upstream for an extension of the one it already replayed. Because entities in different groups stay distinct at the graph layer, references *between* graphs are expressed as resolvable pointers (`knowledge_add_cross_group_edge`, `knowledge_rebind_pointers`) rather than raw edges — so a **layer graph** can carry its own `group_id` and connect entities across two source graphs without either source having to know about it, and without the link dangling when a source is re-ingested.

**Two transport surfaces.** By default the engine serves the Unix-socket JSON-RPC protocol shown above. It can equally run as a native **[Model Context Protocol](https://modelcontextprotocol.io) server over stdin/stdout** (`--mcp-stdio`), pointing any MCP client straight at the graph with no app or custom client in between. See the [IPC & MCP Reference](https://v3rv.com/liminis-context-graph/ipc-mcp-reference).

## Quickstart

### Install prebuilt binary

No Rust toolchain required:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/verveguy/liminis-context-graph/releases/latest/download/lcg-service-installer.sh | sh
```

Prebuilt binaries are published for **macOS (Apple Silicon)**, **Linux x86_64**, and **Linux ARM64** on every tagged release. If macOS blocks the binary, clear the quarantine attribute: `xattr -d com.apple.quarantine ~/.cargo/bin/liminis-context-graph`.

> **An embedder is required at runtime** — see [Configuration: Embedder sidecar](https://v3rv.com/liminis-context-graph/configuration#embedder-sidecar).

### Run it

```sh
# start your embedding service first — see Configuration: Embedder sidecar
cd your-workspace/            # the directory whose content you're indexing
liminis-context-graph         # creates .lcg/, binds .lcg/service.sock
```

### Talk to it

The service speaks newline-delimited JSON-RPC 2.0 over the socket — from any language:

```python
import socket, json

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(".lcg/service.sock")
f = s.makefile("r", encoding="utf-8")

def call(method, params, id=1):
    s.sendall((json.dumps({"jsonrpc": "2.0", "id": id, "method": method, "params": params}) + "\n").encode())
    return json.loads(f.readline())["result"]

# ingest a chunk of text
call("knowledge_process_chunk", {
    "chunk_text": "Ada Lovelace wrote the first program for Babbage's Analytical Engine.",
    "chunk_id": "notes-0001",
    "source_file": "notes.md",
})

# hybrid (full-text + vector) entity search
print(call("knowledge_find_entities", {"query": "early computing pioneers", "num_results": 5}, id=2))

# graph + WAL health at a glance
print(call("knowledge_status", {}, id=3))
```

Or run it as a native MCP server instead — add to your client's MCP config:

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--scope=read,write"],
      "cwd": "/path/to/your-workspace"
    }
  }
}
```

### Build from source

Requires [Rust/Cargo](https://rustup.rs/). The first build downloads a prebuilt, self-contained lbug bundle (LadybugDB bindings) — no C++ toolchain or `cmake` build step:

```bash
cargo build --release                         # build both crates
cargo test -p lcg-core                        # integration tests (LadybugDB round-trip)
cargo run --example basic_ingest -p lcg-core  # example: ingest 3 docs, search, print
cargo run -p lcg-service                      # run the service binary
```

See [Getting Started](https://v3rv.com/liminis-context-graph/getting-started) for downstream-app bundling and pinned-release tarball URLs.

## Scope

**In scope**: a single-user, local-first context graph engine, shipped as a library crate (`lcg-core`) and an IPC binary (`lcg-service`) that are peers — embed it in a Rust application, or drive it from any language over the socket.

**Multi-graph, not multi-tenant.** One process can hold many graphs, each with its own `group_id` and its own WAL stream, and a graph can be replicated into another database by shipping that stream. That is a data-organisation capability for one user's own workspaces and subscriptions — it is not tenancy. There is no authentication, no authorisation, and no per-tenant resource isolation: anything that can reach the socket can reach every group in the database. Treat the process boundary as the trust boundary.

**Out of scope, by design:**

- Storage engines other than LadybugDB — the single-engine bet is what keeps the service embedded, fast, and simple to operate.
- In-process ML runtimes (`tch`, `candle`, `onnxruntime`) — embeddings and extraction stay behind out-of-process adapters.
- Hosted or multi-tenant deployment — this is local-first infrastructure: one process, on your machine, serving one user. Groups separate that user's graphs from each other; they do not separate users from each other, and are not a security boundary.

## Documentation

Full reference documentation is published at **[v3rv.com/liminis-context-graph](https://v3rv.com/liminis-context-graph)**:

- [Getting Started](https://v3rv.com/liminis-context-graph/getting-started) — install, run, build from source, bundle in downstream apps.
- [Configuration](https://v3rv.com/liminis-context-graph/configuration) — every environment variable and CLI flag.
- [IPC & MCP Reference](https://v3rv.com/liminis-context-graph/ipc-mcp-reference) — the JSON-RPC and Model Context Protocol method surface.
- [Telemetry](https://v3rv.com/liminis-context-graph/telemetry) — structured JSONL events emitted on stderr.
- [Ontology](https://v3rv.com/liminis-context-graph/ontology) — the optional entity/relation type vocabulary.
- [Operations](https://v3rv.com/liminis-context-graph/operations) — WAL administration, degraded mode, and self-healing recovery.
- [Testing & Evaluation](https://v3rv.com/liminis-context-graph/testing-and-evaluation) — LLM cassettes and the extraction-quality eval harness.
- [ADR Index](https://v3rv.com/liminis-context-graph/adr/index) — architecture decision records (historical, not current-state, documentation).

The site documents the version stated on its home page and may lag `main` between releases; this README's quickstart always works against `main`.

## Repository layout

```
crates/core/             # lcg-core: library crate — all DB interaction
crates/core/benches/     # performance benchmarks (criterion)
crates/core/examples/    # standalone consumers demonstrating the library API
crates/service/          # lcg-service: binary crate — IPC service (builds `liminis-context-graph`)
crates/eval/             # lcg-eval: binary crate — extraction-quality eval harness
native/local-inference/  # Swift CoreML embedding/LLM sidecar for macOS
docs/                    # documentation site source (published at v3rv.com/liminis-context-graph)
docs/adr/                # architecture decision records (index at docs/adr/index.md)
specs/                   # feature specifications
```

## Dependencies

| Crate | Version | Role |
|-------|---------|------|
| `lbug` | `=0.17.0` | LadybugDB Rust bindings (pinned) |
| `thiserror` | `2` | Error type generation |

No ML-runtime dependencies (`tch`, `candle`, `onnxruntime`) are permitted — embeddings are produced out-of-process.

## Architecture decisions

See [`docs/adr/`](docs/adr/) for recorded architecture decisions ([index](docs/adr/index.md)). The project constitution lives at [`.specify/memory/constitution.md`](.specify/memory/constitution.md).

## Contributing

Contributions are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for how to file issues, submit pull requests, and the required pre-commit checks. No CLA or DCO sign-off is required — contributions are accepted under the project's MIT license by inbound=outbound convention.

## Security

To report a security vulnerability, please use [GitHub's private vulnerability reporting](https://github.com/verveguy/liminis-context-graph/security/advisories/new) rather than filing a public issue. See [`SECURITY.md`](SECURITY.md) for supported versions, response time, and disclosure policy.
