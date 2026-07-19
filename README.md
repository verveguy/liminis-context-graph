# Liminis Context Graph

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**A local-first context graph engine.** One Rust binary that turns a stream of text into a queryable graph of entities, relationships, and episodes — combining property-graph storage, HNSW vector search, and full-text search in a single embedded service. No database server, no separate vector store, no search cluster: everything runs in one process, on your machine, against files in your workspace.

Originally inspired by the knowledge-graph ideas in [graphiti](https://github.com/getzep/graphiti), then deliberately narrowed: instead of a general framework over pluggable backends, `liminis-context-graph` is a purpose-built engine with one storage layer, one wire protocol, and a local-first design from top to bottom.

## Why

AI assistants and agents need durable, structured context: *who* was mentioned, *how* things relate, *what* happened when. Most solutions assemble that from a stack of services — a graph database here, a vector store there, an embedding API in the cloud. That stack is heavy to run, awkward to back up, and quietly moves your data off your machine.

`liminis-context-graph` takes the opposite bet:

1. **One embedded engine.** [LadybugDB](https://github.com/lbugdb/lbug) (the community continuation of KuzuDB) provides the property graph, HNSW vector indices, and full-text search **in a single embedded database** — chosen deliberately for local-first performance: no server process, no network hop, data in ordinary files under your workspace.

2. **The write-ahead log is the source of truth — and it's just JSON.** Every mutation is appended to plain JSONL files in `.lcg/wal/` before it touches the database. The WAL is human-readable, append-only, and **git-friendly**: check it into the same repository as your notes or documents, diff it, and carry it across machines. The database is a derived index — delete it and `knowledge_rebuild_from_wal` reconstructs the entire graph from the log.

3. **Models stay out of process.** Embedding and LLM inference are reached through narrow adapters — a Unix-socket or HTTP endpoint speaking the OpenAI `/v1/embeddings` shape, and a configurable extraction LLM. Run fully local models (the repo ships a macOS CoreML sidecar) or point at a hosted API; the engine itself contains no ML runtime.

The result is a context graph you can treat like the rest of your local tooling: a single process, a directory of files, versionable with git, rebuildable from its own log.

## How it works

```
                      ┌─────────────────────────────────────────────────┐
   text chunks        │  liminis-context-graph (one process)            │
  ──────────────────► │                                                 │
   JSON-RPC 2.0       │  extraction LLM ──► entities + relations        │
   over Unix socket   │  (out-of-process)   dedup + resolution          │
                      │                          │                      │
   search queries     │           1. append ┌────▼───────┐              │
  ──────────────────► │           ────────► │ WAL (JSONL)│ .lcg/wal/    │
   hybrid results     │                     └────┬───────┘ source of    │
  ◄────────────────── │           2. apply       │         truth        │
                      │           ────────► ┌────▼───────┐              │
                      │                     │ LadybugDB  │ .lcg/db/     │
                      │  embedder sidecar   │ graph+HNSW │ derived      │
                      │  (out-of-process)   │ +FTS       │ index        │
                      └─────────────────────┴────────────┴──────────────┘
```

**Ingestion**: `knowledge_process_chunk` sends a chunk of text through the extraction LLM, which returns typed entities and relationships (optionally constrained by your [ontology](#ontology)). New facts are deduplicated against the existing graph, appended to the WAL, then written to the database with embeddings from the sidecar. Every chunk becomes a time-stamped **episode** linked to the facts it produced, so provenance is queryable.

**Search** is hybrid by default: `knowledge_find_entities` and `knowledge_find_relationships` combine full-text and vector similarity over the same embedded store; `knowledge_search_passages` does semantic passage retrieval over episode content; `knowledge_get_entity_neighbors` and `knowledge_query_cypher` traverse the graph directly.

**Everything on disk lives under `.lcg/` in your workspace:**

```
.lcg/
├── wal/               # append-only JSONL mutation log — the durable record (git-friendly)
├── db/liminis.db      # LadybugDB files — a derived index, rebuildable from the WAL
├── ontology.yaml      # optional extraction vocabulary (yours to edit)
└── service.sock       # JSON-RPC 2.0 endpoint while the service runs
```

## Features

- **34 JSON-RPC methods** over a Unix domain socket, covering ingestion, hybrid search, graph reads, curation (`knowledge_merge_entities`, a corrections workflow, relation canonicalization), and administration.
- **Hybrid retrieval** — full-text + HNSW vector similarity in one query path, plus raw Cypher (`knowledge_query_cypher`) for arbitrary graph queries.
- **Optional ontology** — declare entity and relation types (with single-parent hierarchies) in YAML; `open` mode prefers your vocabulary, `strict` mode enforces it. Drift detection flags when the graph predates an ontology change.
- **Episodes with provenance** — every ingested chunk is a time-stamped episode linked to the entities and relationships it produced.
- **WAL administration** — rebuild the database from the log (`knowledge_rebuild_from_wal`), dump the database back to a compacted log (`knowledge_dump_wal`), checkpoint before backups (`knowledge_prepare_checkpoint`). A successful non-dry-run rebuild automatically rebuilds the entity/relationship search indices, so `knowledge_find_entities`/`knowledge_find_relationships` are immediately queryable afterward — `knowledge_build_indices` is not normally required. Check the rebuild result's (or `knowledge_status`'s) `indices_built` field to confirm search-readiness rather than assuming it (see [`knowledge_status` summary](#knowledge_status-summary) below).
- **Self-healing** — the service binds its socket *before* opening the database, so a corrupted store leaves it reachable in degraded mode rather than dead; autonomous startup recovery reopens at the last good checkpoint, replays the WAL tail, and rebuilds indices without intervention.
- **Streaming progress** — long operations accept a `_progress_token` and stream progress frames before the terminal result.
- **Operational telemetry** — structured JSONL events on stderr with per-call timings and LLM token/cost accounting (see [`docs/telemetry.md`](docs/telemetry.md)).

## Quickstart

### Install prebuilt binary

No Rust toolchain required:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/verveguy/liminis-context-graph/releases/latest/download/lcg-service-installer.sh | sh
```

Prebuilt binaries are published for **macOS (Apple Silicon)**, **Linux x86_64**, and **Linux ARM64** on every tagged release.

> **macOS Gatekeeper note**: If macOS blocks the downloaded binary, clear the quarantine attribute before running:
> ```sh
> xattr -d com.apple.quarantine ~/.cargo/bin/liminis-context-graph
> ```
> Code signing will be added in a future release.

> **Embedder required at runtime**: the binary connects to an out-of-process embedding service on startup. See [Embedder sidecar](#embedder-sidecar).

### Run it

```sh
# start your embedding service first — see "Embedder sidecar" below
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

### Build from source

Requires [Rust/Cargo](https://rustup.rs/). The first build downloads a prebuilt, self-contained lbug bundle (LadybugDB bindings) — no C++ toolchain or `cmake` build step:

```bash
cargo build --release                         # build both crates
cargo test -p lcg-core                        # integration tests (LadybugDB round-trip)
cargo run --example basic_ingest -p lcg-core  # example: ingest 3 docs, search, print
cargo run -p lcg-service                      # run the service binary
```

### Bundling in downstream apps

For consumers (e.g. Electron apps or CI pipelines) that need a pinned binary version without running cargo, use the direct tarball URL from GitHub Releases:

```sh
curl -L https://github.com/verveguy/liminis-context-graph/releases/download/<TAG>/lcg-service-aarch64-apple-darwin.tar.xz \
  -o lcg-service-aarch64-apple-darwin.tar.xz
tar -xJf lcg-service-aarch64-apple-darwin.tar.xz
# binary is at: lcg-service-aarch64-apple-darwin/liminis-context-graph
```

Release artifacts are named after the `lcg-service` package (`lcg-service-<target>.tar.xz`); the binary *inside* is `liminis-context-graph`. Targets: `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`. The archive layout is set by cargo-dist 0.32.0; if cargo-dist is upgraded, verify the layout before updating consumer scripts. Each release includes a `.sha256` companion file for verification (`shasum -a 256 -c <file>.sha256`). The macOS Gatekeeper note above applies to script-downloaded binaries too.

Discover the latest release tag programmatically:

```sh
curl -s https://api.github.com/repos/verveguy/liminis-context-graph/releases/latest | jq -r '.tag_name'
```

### Release runbook (maintainers)

1. Update `CHANGELOG.md`: rename `## [Unreleased]` to `## [x.y.z]`.
2. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. The release workflow builds all three platforms and publishes the GitHub Release automatically (~30–45 min).

If a release build fails: delete the tag (`git push --delete origin vX.Y.Z`), fix the issue, then re-tag and re-push.

## Scope

**In scope**: a single-workspace, single-user context graph engine, shipped as a library crate (`lcg-core`) and an IPC binary (`lcg-service`) that are peers — embed it in a Rust application, or drive it from any language over the socket.

**Out of scope, by design:**

- Storage engines other than LadybugDB — the single-engine bet is what keeps the service embedded, fast, and simple to operate.
- In-process ML runtimes (`tch`, `candle`, `onnxruntime`) — embeddings and extraction stay behind out-of-process adapters.
- Hosted or multi-tenant deployment — this is local-first infrastructure: one workspace, one process.

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
| `LCG_WAL_DIR` | No | Directory for write-ahead log JSONL files (default `.lcg/wal`) |
| `LCG_WAL_MAX_BYTES_PER_FILE` | No | Per-file byte-size rotation threshold for the WAL (default `5242880` = 5 MB); set to `0` to disable byte-size rotation and rely on event count only |
| `LCG_WAL_MAX_EVENTS_PER_FILE` | No | Per-file event-count rotation threshold for the WAL (default `10000`); rotation fires when either this threshold or `LCG_WAL_MAX_BYTES_PER_FILE` is reached |
| `LCG_REPLAY_LOG_INTERVAL_SECS` | No | Throttle interval in seconds between `[WAL PROGRESS]` log lines written to stderr during WAL replay (default `30`). Set to `0` to emit a line on every progress event. |
| `ANTHROPIC_API_KEY` | No | API key for Anthropic extraction/classification LLM calls (only needed if routing extraction to a hosted Anthropic model) |
| `LIMINIS_WORKSPACE_ROOT` | No* | Absolute path to the workspace root. **Required** for the three corrections IPC methods (`knowledge_validate_corrections`, `knowledge_apply_corrections`, `knowledge_reprocess_entity_types`). If unset, those methods return a `-32000` error. The corrections file is read from `{LIMINIS_WORKSPACE_ROOT}/.liminis/knowledge-corrections.yaml`. |

## Ontology

`liminis-context-graph` supports an **optional workspace-scoped ontology** that declares the entity types and relation types the LLM should use during extraction. Without an ontology, the LLM derives types ad-hoc (free-form behavior). With one, vocabulary is consistent and queryable across all chunks.

### File location

Place the ontology at `{workspace}/.lcg/ontology.yaml`.

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
  - name: Document
  - name: Rfc
    parent: Document       # optional: Rfc is a subtype of Document
  - name: Adr
    parent: Document       # optional: Adr is also a subtype of Document
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

#### Entity type hierarchy

The optional `parent: <TypeName>` field on an entity type declares a single-parent (tree) subtype relationship. A node typed `Rfc` will carry labels `["Entity", "Document", "Rfc"]` — enabling both specific queries (`WHERE 'Rfc' IN e.labels`) and rollup queries (`WHERE 'Document' IN e.labels`).

- **Additive**: the specific type is never replaced by its parent; ancestor labels are added alongside it.
- **Transitive**: a 3-level chain `SubDoc → Rfc → Document` stamps all four labels.
- **Safe degrades**: an undeclared parent is cleared with a warning; cycles are detected and broken at startup (no crash).
- **Flat ontologies unaffected**: types without `parent` fields behave exactly as before — `["Entity", <SpecificType>]`.
- **Drift detection**: adding, removing, or changing a `parent` changes the ontology content hash, which triggers a `drifted: true` status in `knowledge_status`. Run `knowledge_reprocess_entity_types` to propagate new hierarchy to existing nodes.

See [`docs/examples/ontology.example.yaml`](docs/examples/ontology.example.yaml) for a fully annotated scientific-paper-domain example.

### Modes

| Mode | Entity types | Relation types |
|------|-------------|----------------|
| `open` (default) | Preferred by the LLM; free-form fallback allowed | Same |
| `strict` | Out-of-vocabulary entities dropped post-extraction | Out-of-vocabulary edges dropped |

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

The response also includes an `indices_built` boolean, reporting whether the entity/relationship
FTS + HNSW search indices are currently built and reflect the graph's current contents. This is
normally `true` — a successful `knowledge_rebuild_from_wal` or `knowledge_build_indices` call sets
it. It is `false` when the post-rebuild index build genuinely failed (as opposed to the common,
harmless "already built" case) or before the first index build of a session. `false` does not
mean search is broken: `knowledge_find_entities`/`knowledge_find_relationships` auto-heal by
transparently rebuilding indices and retrying on their first call after a `false` state — the
field exists so a caller can *observe* readiness proactively (e.g. before reporting a rebuild as
fully complete) instead of discovering it only via a search attempt. The same field appears on
`knowledge_rebuild_from_wal`'s result (and on `knowledge_rebuild_status`'s `result` for the
background-job path) for the specific rebuild that produced it; it is omitted from dry-run
rebuild results, since a dry run never touches indices.

## Embedder sidecar

`OaiEmbedder` delegates embedding to an external service over the OpenAI-compatible
`POST /v1/embeddings` contract. The binary supports two transports, selected via CLI flags:

```
liminis-context-graph --embedder-uds /tmp/liminis-inference.sock            # Unix domain socket (default on macOS)
liminis-context-graph --embedder-http http://127.0.0.1:8765/v1/embeddings   # HTTP
```

**Default behaviour** (no flags): the binary looks for the Swift CoreML sidecar socket at
`/tmp/liminis-inference.sock`. If absent, it falls back to `LCG_EMBEDDING_URL` (HTTP). If
neither exists, it exits with a clear error.

The binary probes the embedder at startup to confirm it is reachable and auto-detect the
embedding dimension. If the probe fails and `LCG_EMBEDDING_DIM` is not set, the process
exits with an error rather than failing silently on the first embed request.

Start the embedder sidecar **before** starting the `liminis-context-graph` binary.
Without it, the five embedding-dependent IPC methods fail immediately with an embedding error:
`knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_search_passages`,
`knowledge_process_chunk`, and `knowledge_reprocess_entity_types`. Read-only methods that do
not call the embedder (`health_check`, `knowledge_status`, `knowledge_list_entities`,
`knowledge_get_episodes`) work without the sidecar.

### macOS: Swift CoreML sidecar (default)

This repository ships a Swift CoreML sidecar at [`native/local-inference/`](native/local-inference/)
that serves OpenAI-compatible `/v1/embeddings` (BGE-base-en-v1.5) and `/v1/chat/completions`
(Apple Foundation Models) over UDS at `/tmp/liminis-inference.sock` — fully local inference: no
API key, no network. macOS 26+ and Xcode command-line tools are required. See
[`native/local-inference/README.md`](native/local-inference/README.md) for build and run instructions.

`liminis-context-graph` discovers the sidecar's default UDS socket automatically — start the
sidecar first, then start the binary.

### HTTP transport (CI / Linux / custom embedders)

For environments without the Swift sidecar, pass `--embedder-http` pointing at any
OpenAI-compatible embedding endpoint (local or remote):

```bash
liminis-context-graph --embedder-http http://127.0.0.1:8765/v1/embeddings
```

See [ADR 0006](docs/adr/0006-embedder-http-contract.md) and
[ADR 0016](docs/adr/0016-oai-embedding-contract-uds-transport.md) for the wire contract
specification and transport decision record.

## Repository layout

```
crates/core/             # lcg-core: library crate — all DB interaction
crates/core/benches/     # performance benchmarks (criterion)
crates/core/examples/    # standalone consumers demonstrating the library API
crates/service/          # lcg-service: binary crate — IPC service (builds `liminis-context-graph`)
native/local-inference/  # Swift CoreML embedding/LLM sidecar for macOS
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
