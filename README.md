# Liminis Context Graph

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**A local-first context graph engine.** One Rust binary that turns a stream of text into a queryable graph of entities, relationships, and episodes — combining property-graph storage, HNSW vector search, and full-text search in a single embedded service. No database server, no separate vector store, no search cluster: everything runs in one process, on your machine, against files in your workspace.

Originally inspired by the knowledge-graph ideas in [graphiti](https://github.com/getzep/graphiti), then deliberately narrowed: instead of a general framework over pluggable backends, `liminis-context-graph` is a purpose-built engine with one storage layer, one wire protocol, and a local-first design from top to bottom.

## Why

AI assistants and agents need durable, structured context: *who* was mentioned, *how* things relate, *what* happened when. Most solutions assemble that from a stack of services — a graph database here, a vector store there, an embedding API in the cloud. That stack is heavy to run, awkward to back up, and quietly moves your data off your machine.

`liminis-context-graph` takes the opposite bet:

1. **One embedded engine.** [LadybugDB](https://github.com/lbugdb/lbug) (the community continuation of KuzuDB) provides the property graph, HNSW vector indices, and full-text search **in a single embedded database** — chosen deliberately for local-first performance: no server process, no network hop, data in ordinary files under your workspace.

2. **The write-ahead log is the source of truth — and it's just JSON.** Every mutation is appended to plain JSONL files in `.lcg/wal/` before it touches the database. The WAL is human-readable, append-only, and **git-friendly**: check it into the same repository as your notes or documents, diff it, and carry it across machines. The database is a derived index — delete it and `knowledge_rebuild_from_wal` reconstructs the entire graph from the log. A `from_seq: 0` (default) rebuild against a database that already has data in it fails fast with an explicit error instead of silently producing a duplicate-key failure per node — pass `force_clear: true` to clear it automatically first, or delete it yourself before calling rebuild.

3. **Models stay out of process.** Embedding and LLM inference are reached through narrow adapters, each speaking the OpenAI-compatible `/v1/embeddings` and `/v1/chat/completions` shapes over a Unix socket or HTTP. **Embedding runs fully local out of the box** via the bundled macOS CoreML sidecar, auto-detected when present. **Extraction can also run fully local** — against any OpenAI-compatible endpoint, e.g. a quality-verified model like `qwen3.6-27b` served via `mlx_lm.server` — or against the hosted Anthropic API via `ANTHROPIC_API_KEY`; unlike embedding, extraction requires an explicit `--extractor-uds`/`--extractor-http` flag (or `LCG_EXTRACTION_URL`) to pick a local endpoint, since the bundled sidecar's own model is not recommended for extraction quality (see [Extractor: local or hosted](#extractor-local-or-hosted)). The engine itself contains no ML runtime.

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

**Ingestion**: `knowledge_process_chunk` sends a chunk of text through the extraction LLM, which returns typed entities and relationships (optionally constrained by your [ontology](#ontology)). New facts are deduplicated against the existing graph, appended to the WAL, then written to the database with embeddings from the sidecar. Every chunk becomes a time-stamped **episode** linked to the facts it produced, so provenance is queryable. Any relationship whose endpoint can't be resolved — even after a name-embedding similarity salvage attempt against the chunk's own entities — is dropped rather than written, and the count of such drops is returned as `edges_dropped_unresolvable` in the result.

**Search** is hybrid by default: `knowledge_find_entities` and `knowledge_find_relationships` combine full-text and vector similarity over the same embedded store; `knowledge_search_passages` does semantic passage retrieval over episode content; `knowledge_get_entity_neighbors` and `knowledge_query_cypher` traverse the graph directly.

**Everything on disk lives under `.lcg/` in your workspace:**

```
.lcg/
├── wal/               # append-only JSONL mutation log — the durable record (git-friendly)
├── db/liminis.db      # LadybugDB files — a derived index, rebuildable from the WAL
├── ontology.yaml      # optional extraction vocabulary (yours to edit)
└── service.sock       # JSON-RPC 2.0 endpoint while the service runs
```

**Two transport surfaces.** By default the engine serves the Unix-socket JSON-RPC protocol shown above. It can equally run as a native **[Model Context Protocol](https://modelcontextprotocol.io) server over stdin/stdout** (`--mcp-stdio`), pointing any MCP client — Claude Code, Claude Desktop, other agents — straight at the graph with no app or custom client in between. Either surface routes through the same core dispatch; see [MCP-over-stdio transport](#mcp-over-stdio-transport) below.

## Features

- **35 JSON-RPC methods** over a Unix domain socket, covering ingestion, hybrid search, graph reads, curation (`knowledge_merge_entities`, a corrections workflow, relation canonicalization), and administration.
- **Native MCP server** — the same graph is exposed to any [Model Context Protocol](https://modelcontextprotocol.io) client over stdin/stdout via `--mcp-stdio`, with per-scope tool gating (`read` / `write` / `cypher` / `admin`). Point Claude Code, Claude Desktop, or any agent straight at your workspace — no app, no custom client. See [MCP-over-stdio transport](#mcp-over-stdio-transport).
- **Hybrid retrieval** — full-text + HNSW vector similarity in one query path, plus raw Cypher (`knowledge_query_cypher`) for arbitrary graph queries.
- **Optional ontology** — declare entity and relation types (with single-parent hierarchies) in YAML; `open` mode prefers your vocabulary, `strict` mode enforces it. Drift detection flags when the graph predates an ontology change.
- **Episodes with provenance** — every ingested chunk is a time-stamped episode linked to the entities and relationships it produced.
- **WAL administration** — rebuild the database from the log (`knowledge_rebuild_from_wal`), dump the database back to a compacted log (`knowledge_dump_wal`), checkpoint before backups (`knowledge_prepare_checkpoint`). A successful non-dry-run rebuild automatically rebuilds the entity/relationship search indices, so `knowledge_find_entities`/`knowledge_find_relationships` are immediately queryable afterward — `knowledge_build_indices` is not normally required. Check the rebuild result's (or `knowledge_status`'s) `indices_built` field to confirm search-readiness rather than assuming it (see [`knowledge_status` summary](#knowledge_status-summary) below). A `from_seq: 0` full rebuild refuses to run against a non-empty database unless `force_clear: true` is passed — see [Scopes](#scopes) below. Failure reports also dedupe by `(template, error)`, so a schema gap on one mutation type can no longer hide an unrelated failure category behind a wall of identical samples.
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

### Talk to it over MCP

Or skip the socket entirely and run the graph as a native [MCP](https://modelcontextprotocol.io) server for Claude Code, Claude Desktop, or any MCP client — add it to your client's MCP config:

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

The client then sees the `knowledge_*` tools directly — no socket client to write. See [MCP-over-stdio transport](#mcp-over-stdio-transport) for scopes, attached mode, and the full flag reference.

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

The release version lives in `[workspace.package]` in `Cargo.toml`; cargo-dist derives the
release from it and **requires the pushed tag to match that version**, so the bump and the tag
must agree. Per this repo's worktree rule, prepare the release on a branch and land it via a PR —
never commit release prep directly to `main` — then tag the merge commit.

1. **Bump the version.** In a worktree off `main`, set `version` under `[workspace.package]` in
   `Cargo.toml` to `x.y.z` (all workspace crates inherit it via `version.workspace = true`), then run
   `cargo update -p lcg-core -p lcg-service -p lcg-eval` to sync the workspace entries in `Cargo.lock`.
   Add any newly-introduced workspace member to that command — a crate left out keeps a stale version
   in the lockfile.
2. **Update `CHANGELOG.md`:** rename `## [Unreleased]` to `## [x.y.z] - YYYY-MM-DD`. If no
   `[Unreleased]` section has been maintained, write the section from the merged PRs since the last
   tag (`gh pr list --state merged --search "merged:>=<last-release-date>"`).
3. **Open a PR and merge it** to `main` once CI is green.
4. **Tag the merge commit and push:** `git tag vX.Y.Z <merge-sha> && git push origin vX.Y.Z`.
   The tag (`vX.Y.Z`) must equal the `Cargo.toml` version, or cargo-dist's `plan` step fails.
5. The release workflow builds all three platforms and publishes the GitHub Release
   automatically (~30–45 min).

If a release build fails: delete the tag (`git push --delete origin vX.Y.Z`), fix the issue on a
branch, merge it, then re-tag the corrected commit and re-push.

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
| `LCG_EXTRACTION_LLM` | No | Anthropic model for entity extraction, optional `primary:fallback` format. Only consulted on the Anthropic path (see `ANTHROPIC_API_KEY` below); ignored when a local extraction endpoint is selected. |
| `LCG_EXTRACTION_URL` | No | Fallback HTTP URL used when no `--extractor-uds`/`--extractor-http` flag is passed and `ANTHROPIC_API_KEY` is unset. If this var is also unset in that situation, the binary exits with an error identifying the missing extraction configuration — extraction has no default-socket auto-detection (unlike the embedder), so a running sidecar alone is not enough. |
| `LCG_EXTRACTION_MODEL` | No | Model name sent in local extraction requests (default `local`) — decorative against the bundled sidecar (which ignores the request's `model` field), but meaningful for real OpenAI-compatible servers reached via `--extractor-http`. |
| `LCG_RECORD_LLM` | No | Path to an LLM cassette (JSONL). If set, every extraction call is recorded to this file in addition to running live — see [Record/replay cassettes](#recordreplay-cassettes). Mutually exclusive with `LCG_REPLAY_LLM`. |
| `LCG_REPLAY_LLM` | No | Path to a previously recorded LLM cassette (JSONL). If set, extraction is served entirely from the cassette — no extractor provider is resolved, no credentials are required, and no network call is ever made. Mutually exclusive with `LCG_RECORD_LLM`. See [Record/replay cassettes](#recordreplay-cassettes). |
| `LCG_DEDUP_LLM` | No | If set, enables local dedup adapter |
| `LCG_DEDUP_ADAPTER_URL` | No | URL for the local dedup HTTP adapter (default `http://127.0.0.1:8767`) |
| `LCG_WAL_DIR` | No | Directory for write-ahead log JSONL files (default `.lcg/wal`) |
| `LCG_WAL_MAX_BYTES_PER_FILE` | No | Per-file byte-size rotation threshold for the WAL (default `5242880` = 5 MB); set to `0` to disable byte-size rotation and rely on event count only |
| `LCG_WAL_MAX_EVENTS_PER_FILE` | No | Per-file event-count rotation threshold for the WAL (default `10000`); rotation fires when either this threshold or `LCG_WAL_MAX_BYTES_PER_FILE` is reached |
| `LCG_REPLAY_LOG_INTERVAL_SECS` | No | Throttle interval in seconds between `[WAL PROGRESS]` log lines written to stderr during WAL replay (default `30`). Set to `0` to emit a line on every progress event. |
| `ANTHROPIC_API_KEY` | No | API key for Anthropic entity/relationship extraction. When set (and no explicit `--extractor-uds`/`--extractor-http` flag is passed), extraction uses the hosted Anthropic API for ingestion (`knowledge_process_chunk` / `knowledge_add_episode`) and entity/relation re-classification (`knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`). When unset, extraction requires an explicit `--extractor-uds`/`--extractor-http` flag or `LCG_EXTRACTION_URL` pointing at a local OpenAI-compatible endpoint — it is not auto-detected — see [Extractor: local or hosted](#extractor-local-or-hosted) below. Not needed for read-only, embedding-only, or non-LLM tools. |
| `LIMINIS_WORKSPACE_ROOT` | No* | Absolute path to the workspace root. **Required** for the three corrections IPC methods (`knowledge_validate_corrections`, `knowledge_apply_corrections`, `knowledge_reprocess_entity_types`). If unset, those methods return a `-32000` error. The corrections file is read from `{LIMINIS_WORKSPACE_ROOT}/.liminis/knowledge-corrections.yaml`. |
| `LCG_REPLAY_BATCH_SIZE` | No | Rows per batch during WAL replay (default `64`, valid range `1`–`256`). Lower values reduce peak memory on a large rebuild; higher values replay faster. |
| `LCG_REPLAY_FAILURE_SAMPLES` | No | How many distinct failing lines to retain and report per WAL replay (default `10`). Samples are deduplicated by failure shape, so one bad template cannot crowd out the rest. |
| `LCG_REPLAY_FIDELITY_THRESHOLD` | No | Float `0.0`–`1.0`. Replay warns when the fraction of successfully applied mutations falls below this, i.e. the rebuilt graph is not a faithful reconstruction of the WAL. |
| `LCG_MIGRATION_KEEP_BACKUP` | No | When set, a workspace-layout migration retains its pre-migration backup instead of removing it after a successful migration. |
| `LCG_SHUTDOWN_TIMEOUT_MS` | No | Grace period in milliseconds for in-flight requests to finish on `SIGTERM` before the service exits. |
| `LCG_ATTACHED_CALL_TIMEOUT_MS` | No | Idle-read timeout in milliseconds for MCP attached mode (`--connect`, default `30000`). See [MCP-over-stdio transport](#mcp-over-stdio-transport). |
| `LIMINIS_DEDUP_HYBRID_THRESHOLD` | No | Entity count per `group_id` above which dedup switches from brute-force cosine to the hybrid FTS + vector path. |
| `LIMINIS_LLM_COST_TABLE_PATH` | No | Path to a JSON model-pricing table used to populate `estimated_cost_usd` in `token_usage` telemetry. See [docs/telemetry.md](docs/telemetry.md). |

**Deprecated `GRAPHITI_*` aliases.** Every `LCG_*` variable above that predates the rename also
accepts its old `GRAPHITI_*` spelling — `GRAPHITI_SOCKET_PATH`, `GRAPHITI_DB_PATH`,
`GRAPHITI_EMBEDDING_URL`, `GRAPHITI_EMBEDDING_MODEL`, `GRAPHITI_EMBEDDING_DIM`,
`GRAPHITI_EXTRACTION_LLM`, `GRAPHITI_DEDUP_LLM`, `GRAPHITI_DEDUP_ADAPTER_URL`, and
`GRAPHITI_WAL_DIR`. Using one logs `DEPRECATED: env var <old> is deprecated; rename to <new>` at
startup. They are honoured for now; prefer the `LCG_*` names.

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
FTS + HNSW search indices are currently built and reflect the graph's current contents. The
service builds these indices **eagerly at startup** — immediately after schema init on a fresh
DB, or as part of self-recovery after a WAL-corruption auto-heal (ADR-0009) — before the socket
accepts any request, so `indices_built` is normally `true` from the very first `knowledge_status`
call onward (see ADR-0036). A genuine build failure during that eager startup build fails startup
outright rather than silently leaving indices unbuilt.

`indices_built` still goes back to `false` in narrower, later situations: after
`knowledge_clear_all`, or if a post-rebuild index build genuinely fails (as opposed to the common,
harmless "already built" case). In those cases `false` does not mean search or ingest is broken —
`knowledge_find_entities`/`knowledge_find_relationships`, and, since #208, the ingest hybrid-dedup
path used once a `group_id` passes the dedup threshold, all auto-heal by transparently rebuilding
indices and retrying on their first call after a `false` state. The field exists so a caller can
*observe* readiness proactively (e.g. before reporting a rebuild as fully complete) instead of
discovering it only via a search or ingest attempt. The same field appears on
`knowledge_rebuild_from_wal`'s result (and on `knowledge_rebuild_status`'s `result` for the
background-job path) for the specific rebuild that produced it; it is omitted from dry-run
rebuild results, since a dry run never touches indices.

The response also includes `name_index_trusted` (boolean) and `name_index_fallback_scans`
(integer), reporting the health of the in-process `NameIndex` accelerator behind
case-insensitive entity name lookups (ADR-0038). `name_index_trusted` is `true` unless a write
path is known to have bypassed the index — e.g. a raw-Cypher mutation via
`knowledge_query_cypher` whose follow-up rebuild failed, or a post-replay
`rebuild_name_index()` failure inside `knowledge_rebuild_from_wal` — and goes back to `true`
once the next rebuild succeeds. `name_index_fallback_scans` counts how many times an
endpoint-existence lookup (the #218/#283 "does this entity exist anywhere in the group" check
used during edge-endpoint resolution) missed the index and fell back to a bounded database
scan; it only increments on a miss; a healthy, coherent index keeps this at (or near) `0`. Both
fields are `null` while the service is degraded (no connected database). A rising
`name_index_fallback_scans` count, or a `name_index_trusted: false` that doesn't clear on its
own, signals index desync worth investigating — see issue #283 and
[ADR-0283](docs/adr/0283-name-index-scan-fallback-for-endpoint-authority.md) for the mechanism.

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
Without it, the embedding-dependent IPC methods fail immediately with an embedding error:
`knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_search_passages`,
`knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_reprocess_entity_types`, and
`knowledge_canonicalize_relations` (its ontology-description fallback embeds each residual edge's
`fact`). Read-only methods that do not call the embedder (`health_check`, `knowledge_status`,
`knowledge_list_entities`, `knowledge_get_episodes`) work without the sidecar.

### macOS: Swift CoreML sidecar (default)

This repository ships a Swift CoreML sidecar at [`native/local-inference/`](native/local-inference/)
that serves OpenAI-compatible `/v1/embeddings` (BGE-base-en-v1.5) and `/v1/chat/completions`
(Apple Foundation Models) over UDS at `/tmp/liminis-inference.sock` — fully local inference for
embedding, and a fully local option for extraction: no API key, no network. macOS 26+ and Xcode
command-line tools are required. See
[`native/local-inference/README.md`](native/local-inference/README.md) for build and run
instructions.

`liminis-context-graph` discovers the sidecar's default UDS socket automatically for embedding —
start the sidecar first, then start the binary. Extraction does **not** auto-detect this socket
(see [Extractor: local or hosted](#extractor-local-or-hosted)): the sidecar's Foundation Models
backend is not recommended for extraction quality, so using it there requires the explicit
`--extractor-uds /tmp/liminis-inference.sock` flag rather than happening by default.

### HTTP transport (CI / Linux / custom embedders)

For environments without the Swift sidecar, pass `--embedder-http` pointing at any
OpenAI-compatible embedding endpoint (local or remote):

```bash
liminis-context-graph --embedder-http http://127.0.0.1:8765/v1/embeddings
```

See [ADR 0006](docs/adr/0006-embedder-http-contract.md) and
[ADR 0016](docs/adr/0016-oai-embedding-contract-uds-transport.md) for the wire contract
specification and transport decision record.

## Extractor: local or hosted

Entity/relationship extraction runs against one of two providers, selected with the following
precedence (highest first):

1. **Explicit CLI flag** — `--extractor-uds <path>` or `--extractor-http <url>` always selects the
   local OpenAI-compatible adapter, regardless of whether `ANTHROPIC_API_KEY` is set:

   ```bash
   liminis-context-graph --extractor-uds /tmp/liminis-inference.sock              # Unix domain socket
   liminis-context-graph --extractor-http http://127.0.0.1:8765/v1/chat/completions  # HTTP
   ```

   (`--extractor-uds` and `--extractor-http` are mutually exclusive.)
2. **`ANTHROPIC_API_KEY` set, no explicit flag** — extraction uses the hosted Anthropic API,
   unchanged from prior versions. A reachable local sidecar never silently redirects this traffic.
3. **Neither of the above** — `LCG_EXTRACTION_URL` (HTTP), if set, else the binary exits with a
   clear error identifying the missing configuration.

Unlike the embedder, extraction has **no default-socket auto-detection tier**: a running sidecar
alone never selects it for extraction, even with no `ANTHROPIC_API_KEY` set. This is deliberate,
not an oversight. Extraction requires an explicit signal — a CLI flag or `LCG_EXTRACTION_URL` —
before it will use a local endpoint at all.

> **The bundled sidecar's model is not recommended for extraction quality.** Prior evaluation
> found Apple Foundation Models' context window and capability insufficient for reliable
> entity/relationship extraction (see
> [docs/extraction-quality-evaluation.md](docs/extraction-quality-evaluation.md) for the full
> evaluation, methodology, and model rankings, and #228 for the in-repo eval harness that will keep
> this guidance current). **All figures in that document, historical and #248's alike, describe
> freeform extraction only** — the same corpus/backends run under an ontology (`Open`/`Strict`,
> #266) are not yet measured; see the "Running the ontology mode matrix" section of
> [docs/eval-full-corpus-runbook.md](docs/eval-full-corpus-runbook.md) if you want to produce
> those figures yourself. For local extraction that meets a reasonable quality bar, run a model
> such as `qwen3.6-27b` behind an OpenAI-compatible server
> (e.g. `mlx_lm.server`) and point `--extractor-http`/`--extractor-uds` at it, or set
> `ANTHROPIC_API_KEY` to use the hosted baseline. The bundled sidecar's `/v1/chat/completions`
> route is still reachable for extraction if you want it anyway — pass `--extractor-uds
> /tmp/liminis-inference.sock` explicitly — the engine just never picks it for you.

The resolved choice is reported in a startup log line: `extractor: provider=...,
transport=..., endpoint=...` — `provider` is `anthropic` or `local` depending on which path was
selected. Unlike the embedder, extraction performs no live reachability probe at startup —
Foundation Models' on-device warm-up can be slow, and there is no response shape to auto-detect —
so an unreachable local endpoint surfaces as an error on the first extraction call rather than at
startup. See [ADR 0041](docs/adr/0041-local-openai-compatible-extraction-adapter.md) for the full
design, including why the local adapter uses `response_format: json_object` rather than
function-calling (the bundled sidecar has no `tools`/`tool_choice` support).

## Record/replay cassettes

Every test that exercises the real extraction pipeline used to face a choice: pay for a live
LLM call, or fall back to `MockExtractor`'s fixed `Alice`/`Acme Corp` output regardless of input.
Neither lets you regression-test a prompt change, a response-parsing change, or the ingest
pipeline's real entity/edge yield without spending money on every run. **LLM cassettes** close
that gap: record one real extraction pass to a file, then replay it deterministically and for
free — with no network access — for as long as the recorded calls still match.

### Recording

Set `LCG_RECORD_LLM=<path>` and run a real ingest (`ANTHROPIC_API_KEY` or a local extractor must
still be configured normally — recording wraps whichever provider is resolved). Every extraction
call — `knowledge_add_episode`'s entity/edge extraction, and the `knowledge_reprocess_*` type
classification calls — appends one line to `<path>`. Re-running recording against an existing
path always **appends**, never truncates, matching the WAL's convention.

### Replaying

Set `LCG_REPLAY_LLM=<path>` and run the identical ingest again. Extraction is served entirely
from the cassette: no provider is resolved, no `ANTHROPIC_API_KEY`/`--extractor-*` flag is
needed, and no network call is ever made. `LCG_RECORD_LLM` and `LCG_REPLAY_LLM` are mutually
exclusive — setting both is a startup error.

A replay request that doesn't match any recorded entry — because the episode text differs, or
because a prompt/parsing change altered what's semantically being asked — fails immediately with
an identifiable cassette-miss error rather than silently falling through to a live call or
producing divergent output. **To re-record after a cassette miss**: delete or move aside the
stale cassette (or point `LCG_RECORD_LLM` at a fresh path), re-run the affected ingest with
recording enabled against a live provider, then switch back to `LCG_REPLAY_LLM` to confirm the
new cassette replays cleanly.

### Format

A cassette is plain, uncompressed JSONL — one JSON object per line, no envelope. Each record
carries a `key` (a SHA-256 hex digest used for matching), `call_type` (`extract`,
`classify_entities`, or `classify_relations`), `provider`, `model`, an RFC 3339 `timestamp`,
the human-readable `request` content, and the call's `response`. Records are matched by `key`
alone, independent of file order — a cassette assembled from multiple recording runs (or, for
`LlmRouter`, from more than one primary/fallback leaf) replays correctly as a single flat file.
Two calls with identical semantic content are served FIFO, in the order they were recorded.

**What's in the matching key, precisely** (and what isn't): for `extract`, the rendered entity
and edge system/user prompts plus `episode_body`/`group_id`/`reference_time`/
`custom_instructions`/`source_type` — rendering the prompts (not just hashing the raw options)
means editing a prompt template or the injected ontology correctly invalidates stale cassette
entries. For `classify_entities`/`classify_relations`, the raw call arguments only. Timestamps,
request nonces, and anything transport-specific (headers, API keys, URLs) are never part of the
key, and never reach the cassette at all — the record/replay seam sits at the `Extractor` trait
boundary, strictly above HTTP request construction, so there is nothing credential-shaped for it
to see or need to scrub. See the `crates/core/src/cassette.rs` module doc for the full,
authoritative scope (including one documented, narrow gap around the edge extraction user
prompt).

Because cassettes are plain JSONL with no credential material, they're safe to commit as test
fixtures — see `crates/core/tests/fixtures/README.md` for this repo's fixture-capture
conventions.

### Failure-record sidecar

A failed extraction call — an HTTP error, a malformed/unparseable response, or edge-budget
exhaustion that persists after one retry — is never written to the cassette (its success-only
invariant is unaffected); instead, one record is appended to a sidecar file,
`<cassette-path>.failures.jsonl`. This is created wherever a cassette is being recorded (both
`LCG_RECORD_LLM` and `lcg-eval --record-cassette`) — never in replay mode, since no live failure
can occur there. The file is created eagerly (empty, if no failures occur) alongside the
cassette itself.

Each record is a JSON object with:

| Field | Description |
|-------|-------------|
| `ts_ms` | Unix epoch milliseconds |
| `model` | The model name in force for this call |
| `call_type` | `"entities"` or `"edges"` |
| `chunk_key` | The episode name (production) or corpus chunk title (`lcg-eval`), or `null` |
| `classification` | `"http_error"`, `"truncation"`, or `"malformed"` |
| `raw_body` | The **complete** raw response body — never truncated to a prefix |
| `finish_reason` | The provider's stop/finish reason, or `null` for an HTTP-level failure |
| `completion_tokens` | Output token count, or `null` if unavailable |
| `max_tokens` | The `max_tokens` value in force for the failing call |

A single sidecar file is capped at 20MB; once appending would exceed that, it's rotated to a
numbered `<cassette-path>.failures.N.jsonl` file (matching the WAL's own byte-size rotation
convention) so a long-running service's sidecar can't grow without limit. Individual records are
never truncated to hit this cap — only the aggregate is bounded. See [ADR
0306](docs/adr/0306-extraction-failure-sidecar-and-truncation-visibility.md) for the design
rationale.

## Extraction-quality eval harness

The `lcg-eval` binary (`crates/eval`) measures extraction quality directly against this
engine's own prompts and extractor clients — no captured/copied prompts, so a prompt change
either updates the eval or breaks its build. It closes the gap noted in [ADR
0041](docs/adr/0041-local-openai-compatible-extraction-adapter.md): the local extraction
adapter's quality claim used to rest on a manual-testing caveat instead of anything
measurable. See [docs/extraction-quality-evaluation.md](docs/extraction-quality-evaluation.md)
for the prior research findings this harness re-baselines, and #248 for the maintainer-run
full-corpus model comparison (hosted Anthropic vs. local qwen3.6-27b) built on top of it — see
[docs/eval-full-corpus-runbook.md](docs/eval-full-corpus-runbook.md) for the exact commands.

### Running the harness

```bash
export ANTHROPIC_API_KEY=sk-ant-...   # hosted baseline + LLM-as-judge scoring
cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic \
  --backend local=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=local \
  --reference baseline
```

This runs both backends over the default corpus subset (the first 50 chunks of the #217
public Simple English Wikipedia fixture, `crates/core/tests/fixtures/real_corpus_wal/
corpus_prose.jsonl`) and prints a report with, per backend: strict-string and LLM-as-judge F1
for entities/edges/summaries, latency percentiles, error rate, and structured-output
reliability (clean/recovered/malformed JSON parse counts — FR-007). Pass `--output
report.json` to also write the report as JSON. Run `cargo run -p lcg-eval -- --help` for the
full flag reference.

Each candidate also carries a `truncated` count — `retry_succeeded` (a doubled `max_tokens`
retry recovered) and `exhausted` (it didn't) — surfaced separately from `clean`/`recovered`/
`malformed`. Edge-budget exhaustion is deliberately non-fatal (it returns an empty edge list
rather than erroring), which is otherwise indistinguishable in the report from a chunk where the
model genuinely extracted zero edges; a non-zero `exhausted` count on a chunk means the low
count is suppressed output, not a quality signal. The human-readable report only prints a
`truncated:` line when the count is non-zero, so a clean run's output is unchanged. Pair this
with `--record-cassette` to get the exact raw response for any exhausted call from the
[failure-record sidecar](#failure-record-sidecar).

To validate the judge itself rather than compare backends, point `--reference` and a second
`--backend` at the *same* spec (a baseline-vs-itself run): the judged score should land near
1.0 (pure wording-variance noise floor) while the strict-string score is materially lower —
this is what the `eval.yml` workflow's on-demand smoke pass checks.

### Previewing a run (`--dry-run`)

Before committing to a multi-hour, real-money run, add `--dry-run` to any invocation:

```bash
cargo run --release -p lcg-eval -- \
  --backend baseline=cassette:path=baseline.jsonl \
  --backend candidate=anthropic:model=claude-haiku-4-5-20251001 \
  --reference baseline \
  --all \
  --dry-run
```

This resolves every `--backend` spec exactly the way a real run would — the replay-or-live
decision, a cassette backend's on-disk record count, and the requested scope (`--limit N`
or the full corpus) — and prints the plan without making a single outbound call. It also
names any guard that would abort a real run: two backends resolving to the same cassette
(by path or by byte-identical content), or a cassette that's corrupt or has a duplicate
key. `--dry-run` itself always exits 0 for a syntactically valid invocation, even when the
plan shows a guard that would abort a real run — the point is to see the plan, not to run
the guard as a separate pass-fail check. Combining `--dry-run` with `--record-cassette`
writes nothing.

These are the same guards a real run enforces unconditionally before touching the network:
a duplicate-keyed or otherwise corrupt cassette is rejected at load time (distinguishable by
error type — `Error::CassetteDuplicateKey` vs. `Error::CassetteCorrupt` — not just by exit
code), and two cassette backends that would make the comparison degenerate (identical path
or identical content) are rejected before any extraction happens. A cassette covering fewer
chunks than the requested scope is not an abort condition — it's reported as a coverage
note, since the shortfall already shows up honestly in `error_rate`. `--dry-run` and a real
run share this resolution code exactly (see ADR-0052), so the preview cannot drift from
what actually happens.

A pair of backends `--record-cassette`d fresh in the *same* invocation can't be checked this
way — there's nothing on disk to hash until the run finishes — so that half of the identity
guard runs post-run instead, before the report is ever printed or written: if two freshly
recorded cassettes come out byte-identical, the run still fails loudly, just after capture
rather than before it.

### Adding a candidate backend

`--backend NAME=SPEC` is repeatable. `SPEC` is one of:

- `anthropic[:model=<MODEL>]` — the hosted baseline, via `AnthropicExtractor`. Reads
  `ANTHROPIC_API_KEY`.
- `oai-http:url=<URL>[,model=<MODEL>]` — an OpenAI-compatible local endpoint over HTTP, via
  `OaiExtractor`.
- `oai-uds:path=<SOCKET_PATH>[,model=<MODEL>]` — the same, over a Unix domain socket (e.g. a
  local `mlx_lm.server` instance).
- `cassette:path=<PATH>` — replay a previously recorded cassette instead of making live LLM
  calls, via `ReplayingExtractor`. Makes zero outbound requests; a cassette miss fails loudly
  with `Error::CassetteMiss` rather than falling through to a live call. Cannot be combined
  with `--record-cassette` for the same backend name (recording a replay is meaningless).

No new backend *kind* should be needed for a new model — point an `oai-http`/`oai-uds` spec
at any OpenAI-compatible server. Adding a genuinely new provider means extending
`crates/eval/src/backend.rs`'s `BackendKind`/`build_extractor` the same way `OaiExtractor` was
added to `crates/core/src/extractor.rs` — reuse an existing `Extractor` implementation rather
than writing new HTTP/JSON client logic in the harness (FR-003).

Add `--record-cassette NAME=PATH` to wrap a configured backend in a cassette recorder
(see "Record/replay cassettes" above) so a single corpus pass yields both the eval report and
a recorded cassette — the mechanism #248's full-corpus comparison run relies on. To replay a
cassette recorded this way on a later run without paying for the extraction calls again, use a
`cassette:path=<PATH>` backend spec instead — see `docs/eval-full-corpus-runbook.md`'s
"Resuming a partial run" section for a worked example.

### Running under an ontology (`Open`/`Strict`)

By default every run above is **freeform**: the model invents its own entity/relation type
vocabulary, and `ExtractOptions.ontology` is `None`. Pass `--ontology <PATH>` to load an
`Ontology` from a bare YAML file (not necessarily inside a `.lcg`-rooted workspace — this is a
standalone eval fixture) and thread it through every extraction call instead, exercising the
same `Open`/`Strict` prompt-injection regimes production ingestion uses:

- `--ontology <PATH>` — load the ontology. Omit for the unchanged freeform behavior.
- `--ontology-mode <open|strict>` — which regime to apply; defaults to `strict` when
  `--ontology` is given without it, and overrides any `mode:` the file itself declares. Rejected
  as a usage error if given without `--ontology` (there's nothing to apply the mode to).

```bash
cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic \
  --backend local=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=local \
  --reference baseline \
  --ontology crates/core/tests/fixtures/real_corpus_wal/ontology.yaml \
  --ontology-mode strict
```

The report's top-level `ontology_mode` field records which regime produced it
(`"freeform"`/`"open"`/`"strict"`), and — `Strict` only — each candidate also carries a
`vocabulary_compliance` metric: how often that backend emitted an entity or relation type
outside the ontology's declared vocabulary, tracked separately from
`structured_output.{clean,recovered,malformed}` so a model producing syntactically valid JSON
that simply ignores the closed type list isn't scored as if its structured-output reliability
were perfect. See `docs/eval-full-corpus-runbook.md`'s "Running the ontology mode matrix"
section for the full freeform/`Open`/`Strict` three-command comparison procedure and
`crates/eval/scripts/run_mode_matrix.sh` for a runnable version of it.

### Blind pairwise judging (`--judge-mode`)

The reference-mode report above measures **similarity to `--reference`**, not quality — the
reference is one more model's output, not ground truth, so a candidate that extracts something
the reference missed is scored as a false positive for being right. `--judge-mode pairwise`
(#269) adds a second, reference-agnostic signal: for every pair of configured backends, the
judge sees the source chunk plus the two extractions *unlabelled* (slot A / slot B, no backend
name, model id, or provider reachable by the judge) and picks which better captures the
content, per axis (entities/edges/summary). No backend is privileged.

```bash
cargo run --release -p lcg-eval -- \
  --backend baseline=cassette:path=baseline.jsonl \
  --backend candidate=cassette:path=candidate.jsonl \
  --backend qwen=cassette:path=qwen.jsonl \
  --reference baseline \
  --judge-mode pairwise
```

- `--judge-mode <reference|pairwise|both>` (default `reference`) — `reference` is the
  unchanged pre-#269 behavior (omitting the flag leaves output byte-identical). `pairwise`
  runs only the blind pairwise pass above — the reference-mode judge calls above are skipped
  entirely, not run-and-ignored, so a candidate that's only interested in the pairwise signal
  doesn't pay for reference-mode judge calls it didn't ask for. `both` runs both passes.
- Every chunk is judged in **both** slot orders (an extraction placed in slot A once, slot B
  once), with slot assignment derived deterministically from a hash of the chunk key and the
  two backend names — never wall-clock or RNG, so a re-run reproduces the same result exactly.
  Agreeing verdicts count as a win for the agreed side; disagreeing verdicts count as a tie and
  increment an **order-inconsistency** counter — a judge that flips its answer when the
  operands swap is reporting position bias, not model quality, and that must surface as a
  number rather than be averaged away.
- The report's `pairwise` section (present only when `--judge-mode` requested it — absent
  entirely, not null, under the default `reference` mode) lists, per backend pair and per
  axis: wins, losses, ties, win rate (excluding ties from the denominator), the
  order-inconsistency rate, chunks compared, and chunks skipped (present on only one side of
  the pair — e.g. differing cassette coverage — never silently counted as a loss).
- **The reference-vs-candidate pair is always included** — pairwise mode covers every
  unordered pair among the configured `--backend`s, not just candidates against the
  designated reference.
- **Judge calibration control**: configure the same model as two independently-recorded
  cassettes under different backend names (the pairwise analogue of the reference-mode
  noise-floor pattern above) and judge that pair too. Two independent samples of the same
  model should split near 50/50 on every axis. A run prints a stderr note for **every** pair
  whose win rate falls outside **45–55%** (`pairwise::CALIBRATION_BAND_LOW`/`_HIGH`) — which
  pair is *the* calibration control is operator knowledge the harness can't derive from
  `--backend` specs alone (two independently-recorded `cassette:path=` files of the same
  model, the pattern above, share no spec string to detect it by), so the note doesn't assert
  bias outright: if the flagged pair is your calibration control, the deviation likely means
  judge position bias and every pairwise result in the run should be treated with suspicion;
  if it's a genuine candidate-vs-candidate pair, landing outside the band is the expected,
  desired signal (the whole point of pairwise judging), not evidence of bias. A separate
  warning fires for every pair whenever its order-inconsistency rate exceeds **20%**
  (`pairwise::ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD`) — above that, the judge is flipping
  its answer often enough that the win rate isn't distinguishable from noise; this one is not
  conditional on which pair is the calibration control. Neither warning blocks the run; both
  are stderr-only, so the report artifact itself stays pure data. See
  [ADR-0050](docs/adr/0050-blind-pairwise-judging.md) for the rationale behind both numbers.
- A degenerate pair — two backends whose specs resolve to the *identical* `cassette:path=`
  — is rejected at CLI parse time, before any judge call, naming the offending backend names
  (FR-011). This does **not** reject the same *live* spec (e.g. `anthropic:model=X`)
  configured twice under different names — that's the calibration pattern above, which
  produces two independently-sampled, non-degenerate outputs and must keep working.
- Pairwise mode reuses the same `run_results` reference mode already produced — it makes
  **zero additional extraction calls**, whether the backends are live or `cassette:path=`
  replays, and reuses `--judge-cache` under a disjoint `prompt_name` family so pairwise and
  reference-mode cache entries can never collide. See "Cost implications" below for the
  judge-call multiplier this adds.

### Cost implications

Every corpus chunk costs two extraction calls (entities, then edges) per configured backend,
plus one LLM-as-judge call per scored comparison (entities/edges/summaries) against the
`--reference` backend. Judge calls are the expensive part — they hit a hosted model
(`claude-sonnet-4-6` by default, `--judge-model` to override) regardless of which backends are
under test. The **on-disk judge cache is mandatory, not optional**: pass `--judge-cache
<path>` (default `judge_cache.jsonl` in the current directory) and re-runs against the same
corpus and backends make zero new judge calls (SC-003) — always reuse the same cache path
across repeated runs rather than deleting it. The default corpus subset (50 chunks, override
with `--limit N` / `--all`) is sized to keep a default run affordable; widening it multiplies
cost roughly linearly in chunk count. Without `ANTHROPIC_API_KEY` set, the harness still runs
and reports strict-string F1, but skips judge scoring entirely (no cost, no judged F1 in the
report).

`--judge-mode pairwise`/`both` multiplies judge-call volume further: every unordered backend
pair (C(N,2), not N-1) is judged in both slot orders (FR-004) across all three axes — a
3-backend `pairwise` run costs 3 pairs × 2 orders × 3 axes = 18 judge calls per chunk, versus
reference mode's 2 candidates × 1 order × 3 axes = 6. Still **zero extraction calls** either
way (FR-009) — the judge cache applies identically, so a re-run against the same
`--judge-cache` path costs nothing (SC-005).

## MCP-over-stdio transport

`liminis-context-graph --mcp-stdio` starts a native [Model Context Protocol](https://modelcontextprotocol.io)
server over stdin/stdout, using the official Rust SDK (`rmcp`). Any MCP client (Claude Code,
Claude Desktop, other agents) can query and mutate the knowledge graph directly — no Electron
app, no Node, no custom JSON-RPC client required. This is an *additional* external-facing
surface; the Unix-socket JSON-RPC protocol above is unchanged, and the Liminis app's own MCP
providers (which use a direct pooled socket for better concurrency) are unaffected.

Every MCP tool is derived from the existing `knowledge_*` dispatch methods in
`crates/core/src/handlers.rs` — tool names match the IPC method names verbatim, and each
`tools/call` is translated into an `IpcRequest` and routed straight through the same core
dispatch the socket service uses. No graph logic is duplicated in the MCP transport shell.

### Flags

| Flag | Description |
|------|-------------|
| `--mcp-stdio` | Starts the MCP server over stdin/stdout instead of binding the Unix socket. |
| `--scope=<list>` | Comma-separated list of scopes to advertise in `tools/list` (default `all`). See below. |
| `--connect <path>` | Attached mode: forward every `tools/call` as JSON-RPC over the given Unix socket to an already-running service, instead of opening the database directly. |
| `--allow-remote-close` | Attached mode only: advertise and allow `knowledge_close`, forwarding the shutdown to the remote service. No effect in standalone mode (no `--connect`). |

### DB-access modes

- **Standalone (default, no `--connect`)**: the MCP process opens the `.lcg` database directly,
  reusing the same startup and self-recovery path as the socket service ([ADR 0009](docs/adr/0009-degraded-mode-startup-recovery.md)).
  Zero-dependency — works with no other process running.
- **Attached (`--connect <socket-path>`)**: the MCP process never opens the database; it
  forwards each call over the given socket to a service that already has it open. Use this to
  add MCP access to a workspace where the Liminis app (or another socket-service instance) is
  already running, without contending for lbug's single-writer lock.
  - **Idle timeout.** `LCG_ATTACHED_CALL_TIMEOUT_MS` (default 30s) is a **per-read-line** idle
    timeout, not a whole-call timeout: it resets on every line read off the socket, including
    `{"type":"progress"}` lines. A call that keeps emitting progress (see
    [Progress notifications](#progress-notifications) below) is never bounded by it, no matter
    how long the call runs in total — only genuine silence (no output at all for the full
    timeout window) trips it. If the remote stops responding mid-call (e.g. it crashes), the
    attached client fails that call with a clean timeout error rather than blocking forever.
  - **Reconnect and retry.** If the connection to the remote breaks, the client transparently
    re-dials the same socket path rather than staying wedged. If the break is detected while
    writing the outgoing request — treated as safe to retry, since the write failing is the
    client's best available signal that the request didn't get through — the client
    automatically retries that request exactly once over the freshly-dialed connection. If the
    break is detected only after the request was fully written — while waiting for or reading
    the response — the call is **not** retried automatically, since the remote's execution
    status is unknown and blind retry could double-apply a non-idempotent write (e.g.
    `knowledge_add_episode`); that call fails with a clear "connection lost mid-call" error, but
    the connection is marked dead so the *next* call reconnects fresh. If a reconnect attempt
    itself fails (no listener at that path), the call fails with a clear, descriptive error —
    never a hang — and a later call will try reconnecting again. See
    [ADR-0040](docs/adr/0040-attached-mode-reconnect-retry-boundary.md) for the full rationale.

### Scopes

Scopes are additive and composable (e.g. `--scope=read,admin`). `tools/list` advertises the
union of all active scopes.

| Scope | Methods |
|-------|---------|
| `read` | `knowledge_status`, `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_get_episodes`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`, `knowledge_search_passages`, `knowledge_list_entities`, `knowledge_list_relationships`, `knowledge_get_entity_neighbors`, `knowledge_get_entities_by_source`, `knowledge_rebuild_status`, `knowledge_validate_corrections` |
| `write` | `knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_delete_episode`, `knowledge_delete_by_source`, `knowledge_delete_chunk_episode`, `knowledge_clear_all`, `knowledge_apply_corrections`, `knowledge_merge_entities`, `knowledge_reprocess_entity_types`, `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types`, `knowledge_reprocess_relation_types` |
| `cypher` | `knowledge_query_cypher` |
| `admin` | `knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_rebuild_from_wal`, `knowledge_recover`, `knowledge_recover_full`, `knowledge_close`, `knowledge_build_indices` |
| `all` | every scope above (default) |

**⚠️ `cypher` is a power scope, not bundled into anything else.** `knowledge_query_cypher` executes
raw Cypher with no param interpolation or value coercion — despite being a "query" method, it can
perform arbitrary mutations, and it bypasses the WAL-ordering and embedding invariants that the
structured write tools maintain. It is never implicitly included in `read`, `write`, or `admin`;
operators must opt in explicitly (or via `all`).

**⚠️ `knowledge_close` in attached mode is a footgun without `--allow-remote-close`.** In
**standalone** mode, `knowledge_close` is always advertised under `admin` scope and shuts down
only this MCP process's own DB connection. In **attached** mode, calling it would shut down the
*running remote service* — including the Liminis app's, if that's what you attached to. Without
`--allow-remote-close`, `knowledge_close` is omitted from `tools/list` entirely in attached mode
(not merely rejected when called). Pass `--allow-remote-close` only when you specifically intend
this MCP connection to be able to stop the remote service.

**Recovery and export live under `admin`.** `knowledge_rebuild_from_wal` (rebuild the graph from
the WAL), `knowledge_dump_wal` (snapshot/export the graph into a fresh compacted WAL directory),
and `knowledge_recover` / `knowledge_recover_full` are all `admin`-scope tools — an attached client
only sees them when launched with `--scope=admin` (or `all`). If a mutation goes wrong, this is the
recovery path. Note the WAL replays **forward-only**, so take periodic `knowledge_dump_wal` snapshots
if you want restore points before large or destructive operations.

**`knowledge_rebuild_from_wal` refuses to run against a non-empty database, unless you ask it
not to.** A `from_seq: 0` (default) full rebuild against a database that already contains data
fails fast with an explicit error rather than silently emitting a duplicate-primary-key failure
for every existing `Entity`/`Episodic`/`RelatesToNode_` row — the native write path uses `CREATE`,
not `MERGE`, for those labels. Pass `force_clear: true` to have the call clear the database itself
before replaying (the same DB-file-delete-and-reopen behavior `knowledge_recover`'s
`rebuild_from_workspace_wal` strategy uses), or clear it yourself first with `knowledge_clear_all`.
`dry_run: true` always fails fast on a non-empty database regardless of `force_clear`, since a dry
run must never mutate the database — this lets a preview surface the problem before you commit to
a real rebuild. None of this applies to an incremental `from_seq > 0` resume, which intentionally
targets a database that already has state.

### Relation typing (`canonicalize_relations`, `backfill_relation_types`, `reprocess_relation_types`)

Three tools populate an edge's `relation_type`, with different tradeoffs:

**`knowledge_canonicalize_relations`** maps each edge's **existing raw predicate** onto your
ontology's declared `relation_types`. Its behavior has three caveats worth knowing before you
rely on it:

- **The primary pass is lexical, over the predicate — not the `fact`.** It matches the edge's
  predicate / current `relation_type` against ontology type names, aliases, and keywords. It does
  **not** read the edge's `fact` sentence, so an edge whose `relation_type` was cleared cannot be
  re-mapped from its fact by this pass.
- **`embedding_threshold` tunes only the fallback promoter** (default `0.7`). The fallback embeds
  each residual edge's `fact` against the ontology types' *descriptions* and force-assigns the single
  nearest type at or above the threshold. Lowering it types more edges, but by nearest-neighbor
  force-fit with **no abstention** — an idiosyncratic fact (e.g. "*X is affiliated with Y*") can land
  on a wrong type (e.g. `HOLDS`).
- **Re-runs are only partly idempotent, and clearing `relation_type` can't be undone by
  canonicalize.** A re-run skips an edge only when it's already at its target — a `Mapped` edge
  already equal to the canonical type, or a residual edge already `UNCLASSIFIED`; an edge whose
  classification *changes* is overwritten (including a previously-assigned type), while
  arrow-named "noise" edges keep any existing predicate. Critically, canonicalize's only input is
  the edge's existing predicate / `relation_type` — if you **null that field to "start clean,"
  canonicalize has nothing to map from and cannot rebuild it.** Snapshot with `knowledge_dump_wal`
  before such an operation.

**`knowledge_backfill_relation_types`** (DEPRECATED) does not classify at all — it mints
uppercased fact-prefix pseudo-types (e.g. `THE_SPECIFICATION_DOCUMENT_DEFINES`) for edges with no
`relation_type`, rather than matching against the ontology. Avoid it for building a typed
taxonomy; prefer `knowledge_reprocess_relation_types` below, which supersedes it for that purpose.

**`knowledge_reprocess_relation_types`** is the relation-side twin of
`knowledge_reprocess_entity_types`: for each in-scope edge, it sends the edge's `fact` and the
ontology's declared relation types (name + description) to the configured extraction LLM and asks
it to pick exactly one type, or honestly abstain. This is the tool to reach for when you want
genuine fact-based classification instead of lexical matching or pseudo-typing:

- **Always reads the `fact`, never the predicate.** Unlike `canonicalize_relations`, classification
  is grounded in the edge's natural-language fact sentence against the ontology's declared menu —
  not lexical/alias/keyword matching on the existing predicate string.
- **A declared ontology relation-type menu is always required.** Unlike
  `knowledge_reprocess_entity_types` (whose `untyped` scope works with no ontology via open-ended
  classification), every scope value (`untyped`, `off_ontology`, `all`) fails with a structured
  `{success: false, error: ...}` if the ontology declares no relation types — there is no
  open-ended fallback (see [ADR-0037](docs/adr/0037-relation-classification-abstention-writes-unclassified.md)).
- **Abstention is an honest, real write of `UNCLASSIFIED`.** If the LLM cannot map a fact to any
  declared type, the edge's `relation_type` is set to the literal string `UNCLASSIFIED` — never a
  force-assigned nearest match. This differs from `knowledge_reprocess_entity_types`, where an
  unclassifiable entity is simply left unchanged (see ADR-0037).
- **`scope`** controls candidates: `"untyped"` (default) — `relation_type` NULL/empty, the same
  predicate `backfill_relation_types` uses; `"off_ontology"` — untyped edges plus edges whose
  `relation_type` isn't a declared type (this naturally covers prior `UNCLASSIFIED` sentinels and
  `backfill_relation_types`'s fact-prefix pseudo-types with no special-casing); `"all"` — every
  edge in the group.
- **Idempotent.** An edge whose computed verdict already matches its current `relation_type`
  (including an edge already correctly `UNCLASSIFIED`) is left unchanged — no write, no WAL entry.
- **`dry_run: true`** returns `would_reclassify_count`, a `plan` array of per-edge
  `{edge_id, fact, old_type, new_type}` entries, and a `breakdown` object counting edges per
  assigned `new_type` (including an `UNCLASSIFIED` count) — without mutating the graph.

### Progress notifications

The five long-running operations — `knowledge_rebuild_from_wal`, `knowledge_canonicalize_relations`,
`knowledge_backfill_relation_types`, `knowledge_reprocess_relation_types`, and
`knowledge_reprocess_entity_types` — bridge to MCP progress notifications when the client
attaches a progress token to the `tools/call` request (`_meta.progressToken`), in both
standalone and attached mode. Without a progress token, these calls simply block until they
complete, same as over the socket protocol. In attached mode, each progress notification also
re-arms `LCG_ATTACHED_CALL_TIMEOUT_MS`'s per-read-line idle timer (see
[DB-access modes](#db-access-modes) above), so a progress-tracked call isn't falsely reported as
timed out just because it runs longer than that timeout in total.

### Example MCP client config

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--scope=read,write"],
      "cwd": "/path/to/your/workspace"
    }
  }
}
```

To attach to an already-running socket service instead of opening the DB directly:

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--connect", "/path/to/your/workspace/.lcg/service.sock", "--scope=read"]
    }
  }
}
```

See [ADR 0035](docs/adr/0035-mcp-stdio-transport.md) for the transport's internal architecture.

## Repository layout

```
crates/core/             # lcg-core: library crate — all DB interaction
crates/core/benches/     # performance benchmarks (criterion)
crates/core/examples/    # standalone consumers demonstrating the library API
crates/service/          # lcg-service: binary crate — IPC service (builds `liminis-context-graph`)
crates/eval/             # lcg-eval: binary crate — extraction-quality eval harness
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
