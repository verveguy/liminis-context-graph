# Implementation Plan: Issue #2 — IPC Parity (US1)

**Branch**: `fabrik/issue-2` | **Date**: 2026-05-19 | **Spec**: `specs/001-rust-knowledge-graph/spec.md#user-story-1`

## Summary

Implement the Unix-socket IPC server and all 11 library-side handlers so existing liminis-framework clients (knowledge-reader, knowledge-writer, semantic-search) work without change. Adds schema tables for relationships (RELATES_TO) and entity mentions (MENTIONS), hybrid BM25+vector+RRF search, the add_episode pipeline (embed → extract → dedup → persist), and a recorded request/response parity corpus run in CI. WAL append is deferred to issue #3; HNSW dedup optimization to issue #4.

## Technical Context

**Language/Version**: Rust stable, 2021 edition  
**Primary Dependencies**: `lbug = "=0.16.1"` (existing), `tokio` (async runtime + UnixListener), `serde`/`serde_json` (JSON-RPC framing), `uuid` (v4 generation), `chrono` (timestamps), `reqwest` (HTTP for embedding/extraction adapters)  
**Storage**: LadybugDB — extends issue #1 foundation (Entity + Episodic tables, HNSW indexes already created)  
**Testing**: `cargo test` + parity replay test using committed fixtures  
**Target Platform**: Linux (ubuntu-latest) + macOS (macos-latest)  
**Project Type**: Cargo workspace — library crate + binary crate (unchanged structure)  
**Performance Goals**: p95 search latency ≤ 500 ms (SC-004 budget applies to this issue's search path)  
**Constraints**: No ML runtime in-process (Principle V); no DB driver abstraction (Principle III); parity test corpus in CI (Principle I)  
**Scale/Scope**: US1 only — 11 IPC methods, ~20 parity fixture files, one 50-query golden set for SC-002

## Architecture Decisions

### AD-1: tokio async runtime in the binary; sync library wrapped in spawn_blocking

The IPC server (`liminis-graph/src/main.rs`) uses `tokio::net::UnixListener`. The library API stays synchronous (AD-3 from issue #1). Each IPC handler wraps its sync library call in `tokio::task::spawn_blocking`. The `Conn` type has a `'db` lifetime that makes it `!Send`, so the DB + Conn are created inside the `spawn_blocking` closure or behind an `Arc<Mutex<Conn>>`. Decision: pass an `Arc<Mutex<Conn>>` to each handler so one shared connection is reused per socket session.

### AD-2: JSON-RPC 2.0, newline-delimited, over Unix socket

Wire format: one UTF-8 JSON object per line. Requests: `{"jsonrpc":"2.0","id":<int>,"method":"knowledge_*","params":{...}}`. Responses: `{"jsonrpc":"2.0","id":<int>,"result":<any>}` or `{"jsonrpc":"2.0","id":<int>,"error":{"code":<int>,"message":"..."}}`. Socket path from `LCG_SOCKET_PATH` env var, defaulting to `.lcg/service.sock` relative to the working directory. The server creates parent dirs if missing.

Wire method names (matching Python service):

| Wire method | Library function |
|---|---|
| `knowledge_add_episode` | `episode::add_episode(...)` |
| `knowledge_find_relationships` | `search::hybrid_edge_search(...)` |
| `knowledge_find_entities` | `search::hybrid_entity_search(...)` |
| `knowledge_get_episodes` | `Conn::retrieve_episodes(...)` |
| `knowledge_delete_episode` | `Conn::remove_episode(...)` |
| `knowledge_get_nodes_by_group` | `Conn::get_entities_by_group_ids(...)` |
| `knowledge_get_edges_by_group` | `Conn::get_edges_by_group_ids(...)` |
| `knowledge_get_edges_by_uuids` | `Conn::get_edges_by_uuids(...)` |
| `knowledge_query_cypher` | `Conn::cypher_query(...)` |
| `knowledge_build_indices` | `Conn::build_indices_and_constraints(...)` |
| `knowledge_close` | `Conn::close()` (flush, then server exits) |

### AD-3: RELATES_TO as a rel table from Entity to Entity; RelatesToNode_ node table for fact-vector search

LadybugDB (Kuzu fork) supports `CREATE REL TABLE`. Whether vector indexes are supported on rel properties must be validated in the spike (T002). The fallback for fact-vector search: add a `RelatesToNode_` node table that mirrors the fact+fact_embedding, linked by UUID. If the spike proves that `CALL QUERY_VECTOR_INDEX` works on a rel column, the RelatesToNode_ shadow table can be dropped in a follow-up. For this issue, the RelatesToNode_ node pattern is used unconditionally for vector search on facts. The RELATES_TO rel table still exists as the primary edge store (preserving schema parity with the Python service).

Schema additions:

```sql
-- Primary relationship edge store (Python-compatible)
CREATE REL TABLE IF NOT EXISTS RELATES_TO (
    FROM Entity TO Entity,
    uuid STRING,
    name STRING,
    group_id STRING,
    fact STRING,
    valid_at TIMESTAMP,
    invalid_at TIMESTAMP,
    attributes STRING
);

-- Shadow node for fact vector search
CREATE NODE TABLE IF NOT EXISTS RelatesToNode_ (
    uuid STRING PRIMARY KEY,
    name STRING,
    group_id STRING,
    created_at TIMESTAMP,
    fact STRING,
    fact_embedding FLOAT[{dim}],
    valid_at TIMESTAMP,
    invalid_at TIMESTAMP,
    attributes STRING
);

-- Episode-to-entity link
CREATE REL TABLE IF NOT EXISTS MENTIONS (
    FROM Episodic TO Entity,
    group_id STRING
);
```

FTS indexes (post-spike validation of syntax):

```sql
CALL db.index.fulltext.createNodeFullTextIndex('entity_name_fts', ['Entity'], ['name'])
CALL db.index.fulltext.createNodeFullTextIndex('relates_to_fact_fts', ['RelatesToNode_'], ['fact'])
```

HNSW vector index for fact embeddings:

```sql
CALL CREATE_VECTOR_INDEX('RelatesToNode_', 'relates_to_fact_embedding_idx', 'fact_embedding', metric := 'cosine')
```

### AD-4: Brute-force cosine dedup in add_episode (HNSW dedup is issue #4)

During `add_episode`, each extracted entity is deduped against existing entities in the same group_id using brute-force cosine similarity: `MATCH (e:Entity) WHERE e.group_id = $gid RETURN e.uuid, e.name, e.name_embedding`. Cosine computed in Rust. If max similarity ≥ 0.85, the existing entity is updated (summary merged); otherwise a new entity is created. Issue #4 replaces this with a `CALL QUERY_VECTOR_INDEX` pre-filter.

### AD-5: Adapters are out-of-process, reached via HTTP; no trait objects required

`embedder::Embedder` is a plain struct that calls `POST $LCG_EMBEDDING_URL/embed` with `{"text": ..., "model": $LCG_EMBEDDING_MODEL}` and expects `{"embedding": [...]}`. `extractor::Extractor` is a plain struct that calls Anthropic `/v1/messages` using `ANTHROPIC_API_KEY`. Both are constructed once at startup and passed by reference. No trait objects needed for the parity tests — the parity test exercises the full stack. Principle V: no `tch`/`candle`/`onnxruntime` in Cargo.toml.

### AD-6: group_id default of "liminis" applied at the IPC dispatch layer

If `params.group_id` is null/absent in the JSON-RPC request, the dispatcher substitutes `"liminis"` before calling the library function. Library functions always take a non-optional `group_id: &str`.

### AD-7: RRF fusion formula and ranking

`rrf_score(rank) = 1.0 / (rank as f64 + 60.0)`. RRF is computed over UUID lists from FTS and vector search independently. Results are re-ranked by summed RRF score (descending), then limited to `num_results`. Tie-breaking by UUID for determinism (parity test requirement).

### AD-8: Entity-first label invariant enforced at insert time

`Conn::insert_entity` checks that `labels[0] == "Entity"`. If not, the labels are reordered so "Entity" is first, with remaining labels preserved in their original order. A warning is logged if reordering occurs. This matches the Python driver's invariant.

### AD-9: Parity test corpus approach

~20 fixture files committed to `tests/fixtures/ipc_corpus/` in `liminis-graph-core`. Each file contains `{"request": {...}, "response": {...}}`. The parity test (`tests/ipc_parity.rs`) starts the Rust service against a pre-populated DB snapshot (`tests/fixtures/baseline_db/`), replays each fixture request, and field-compares the response. The baseline DB snapshot is generated offline from the Python service and committed. CI runs this test on every push. The 50-query golden set for SC-002 rank-correlation is in `tests/fixtures/golden_queries.json`; its Spearman ≥ 0.9 assertion runs in the same test file but is gated by the `PARITY_GOLDEN` env var (skipped in CI until the golden set is captured).

## Constitution Check

- **Principle I (IPC Parity)** — PASS. Every IPC method has a parity fixture. Parity test runs in CI against committed corpus. The socket path, framing, and JSON shapes match the Python service exactly.
- **Principle II (Library and Binary Are Peers)** — PASS. Every IPC-exposed method is callable via the library API (`liminis-graph-core::handlers::dispatch` or direct `Conn::*` methods). The `examples/basic_ingest` example is extended to demonstrate search.
- **Principle III (LadybugDB Only)** — PASS. No driver abstraction. `Conn` is used directly in all handlers. Adapters (embedder, extractor) are for LLM/embedding services, not DB drivers — not a Principle III violation.
- **Principle IV (WAL Is Authoritative)** — N/A for this issue. WAL append stubs are added to `episode.rs` (no-op returning Ok) as placeholders for issue #3. No WAL writes occur.
- **Principle V (LLM/Embedding Adapters Out-of-Process)** — PASS. `embedder.rs` and `extractor.rs` use `reqwest` HTTP calls. No ML runtime in Cargo.toml.

### Performance budget gates

The p95 ≤ 500 ms search latency budget (SC-004) applies to the `knowledge_find_entities` and `knowledge_find_relationships` code paths. A bench in `benches/` is REQUIRED before merge for these paths:

```
liminis-graph-core/benches/search.rs   -- bench_hybrid_entity_search, bench_hybrid_edge_search
```

Brute-force cosine dedup is not a hot path at this issue's scale (dedup optimization is issue #4) — no bench required for the dedup step.

### Workflow gates

- Spec exists at `specs/001-rust-knowledge-graph/spec.md` ✓
- IPC-touching changes: parity tests planned in Phase 9 ✓
- Hot-path search: bench planned in Phase 5 (`benches/search.rs`) ✓
- No WAL code: WAL stubs only, no TDD required ✓
- No constitution deviations requiring ADR ✓

## Project Structure

```text
Cargo.toml                          # add tokio, serde, serde_json, uuid, chrono, reqwest
liminis-graph-core/
├── Cargo.toml                      # add new deps
└── src/
    ├── lib.rs                      # add re-exports for new modules
    ├── db.rs                       # EXTENDED: RELATES_TO/MENTIONS insert, query, search methods
    ├── schema.rs                   # EXTENDED: RelatesToNode_, RELATES_TO, MENTIONS DDL + FTS indexes
    ├── types.rs                    # EXTENDED: RelatesToEdge, MentionsEdge, ExtractionResult, EmbeddingResult
    ├── ipc.rs                      # NEW: IpcRequest, IpcResponse, IpcError (serde JSON-RPC types)
    ├── handlers.rs                 # NEW: dispatch() → all 11 IPC methods  [IPC]
    ├── episode.rs                  # NEW: add_episode pipeline               [LDB]
    ├── search.rs                   # NEW: hybrid_entity_search, hybrid_edge_search, rrf_fuse  [LDB][IPC]
    ├── embedder.rs                 # NEW: HTTP embedding adapter              [ADAPTER]
    └── extractor.rs                # NEW: Anthropic extraction adapter        [ADAPTER]
liminis-graph-core/
├── tests/
│   ├── ldb_spike_ipc.rs            # NEW: spike for REL TABLE + FTS + HNSW query syntax  [LDB]
│   └── ipc_parity.rs               # NEW: parity replay test                 [IPC]
└── benches/
    └── search.rs                   # NEW: bench_hybrid_entity_search, bench_hybrid_edge_search  [HOT]
liminis-graph-core/tests/fixtures/
├── ipc_corpus/                     # NEW: ~20 JSON request/response files    [IPC]
│   ├── add_episode_01.json
│   ├── find_entities_01.json
│   ├── ...
├── baseline_db/                    # NEW: pre-populated DB snapshot (committed binary)
│   └── liminis.db
└── golden_queries.json             # NEW: 50-query golden set for SC-002

liminis-graph/
└── src/
    └── main.rs                     # EXTENDED: tokio UnixListener event loop  [IPC]
```

## Data Model

### Request/Response shapes (abbreviated)

```rust
// ipc.rs
#[derive(Deserialize)]
pub struct IpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,   // int or string
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum IpcResponse {
    Ok  { jsonrpc: String, id: serde_json::Value, result: serde_json::Value },
    Err { jsonrpc: String, id: serde_json::Value, error: IpcError },
}

#[derive(Serialize)]
pub struct IpcError { pub code: i32, pub message: String }
```

### Key param shapes (what liminis-framework sends)

```json
// knowledge_add_episode
{ "name": "str", "episode_body": "str", "source": "message|text|json",
  "source_description": "str", "reference_time": "ISO8601", "group_id": "liminis" }

// knowledge_find_entities / knowledge_find_relationships
{ "query": "str", "group_ids": ["liminis"], "num_results": 10 }

// knowledge_get_episodes
{ "group_id": "liminis", "last_n": 50 }

// knowledge_delete_episode
{ "episode_uuid": "str" }

// knowledge_get_nodes_by_group / knowledge_get_edges_by_group
{ "group_ids": ["liminis"] }

// knowledge_get_edges_by_uuids
{ "uuids": ["str", ...] }

// knowledge_query_cypher
{ "query": "MATCH (e:Entity) RETURN e LIMIT 10" }

// knowledge_build_indices / knowledge_close
{}
```

## Complexity Tracking

| Item | Notes |
|------|-------|
| RelatesToNode_ shadow node | Required because lbug vector indexes only work on node tables; shadow node is the simplest workaround; will be evaluated for removal in issue #4 once spike confirms vector-on-rel support |
| Brute-force cosine dedup | Deliberate — HNSW dedup is issue #4; for US1 scale (~500 episodes) brute-force is acceptable |
