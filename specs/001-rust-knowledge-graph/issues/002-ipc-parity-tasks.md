# Tasks: Issue #2 — IPC Parity (US1)

**Input**: `specs/001-rust-knowledge-graph/issues/002-ipc-parity-plan.md`, `specs/001-rust-knowledge-graph/spec.md`

## Format: `[ID] [P?] [US1] [Constitution-tag?] Description`

- **[P]**: Can run in parallel with other [P] tasks in the same phase
- **[IPC]**: Touches Unix-socket IPC surface — parity test against corpus REQUIRED
- **[LDB]**: Touches LadybugDB driver layer — parity test REQUIRED
- **[HOT]**: Touches search hot path — bench in `benches/` REQUIRED before merge
- **[ADAPTER]**: Touches LLM/embedding adapter — out-of-process boundary MUST be preserved (no ML runtime in Cargo.toml)

---

## Phase 1: Dependencies & LadybugDB Spike

**Purpose**: Add new crate dependencies and validate the three unverified lbug behaviors (REL TABLE creation, FTS query syntax, HNSW vector query syntax) before any dependent implementation begins.

**⚠️ CRITICAL**: All of Phase 2+ depends on the spike results. If a spike test fails, update the plan (AD-3) before proceeding.

- [ ] T001 Update `Cargo.toml` (workspace) to add `tokio = { version = "1", features = ["full"] }`, `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`, `uuid = { version = "1", features = ["v4"] }`, `chrono = { version = "0.4", features = ["serde"] }`, `reqwest = { version = "0.12", features = ["json", "rustls-tls"] }` to `[workspace.dependencies]`; update `liminis-graph-core/Cargo.toml` and `liminis-graph/Cargo.toml` to reference new workspace deps as needed

- [ ] T002 [US1] [LDB] Spike: create `liminis-graph-core/tests/ldb_spike_ipc.rs` with `test_rel_table_creation_and_query` — opens a TempDir DB, creates `RELATES_TO` rel table `FROM Entity TO Entity` with `uuid STRING, fact STRING, group_id STRING` properties, inserts two Entity nodes and one RELATES_TO edge between them, queries it back with `MATCH (a:Entity)-[r:RELATES_TO]->(b:Entity) RETURN r.uuid, r.fact`, asserts the values round-trip; test MUST pass before T009

- [ ] T003 [P] [US1] [LDB] Spike: add `test_fts_index_creation_and_query` to `liminis-graph-core/tests/ldb_spike_ipc.rs` — creates Entity nodes, calls `CALL db.index.fulltext.createNodeFullTextIndex(...)`, queries with `CALL db.index.fulltext.queryNodes(...)`, asserts scored results come back; test MUST pass before T013

- [ ] T004 [P] [US1] [LDB] Spike: add `test_hnsw_vector_query` to `liminis-graph-core/tests/ldb_spike_ipc.rs` — inserts Entity nodes with 768-dim embeddings, creates HNSW index, queries with `CALL QUERY_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', <query_vec>, 5)`, asserts results return with distance scores; test MUST pass before T014

**Checkpoint**: `cargo test -p liminis-graph-core --test ldb_spike_ipc` passes on local Linux and macOS. Adjust AD-3 in the plan if any spike fails before continuing.

---

## Phase 2: Schema & Type Extensions

**Purpose**: Extend the existing DB schema and Rust types to cover relationships and mentions. Depends on Phase 1 spike outcomes.

- [ ] T005 [US1] [LDB] Extend `liminis-graph-core/src/schema.rs` — add `create_edge_tables(conn: &Conn, embedding_dim: usize) -> Result<(), Error>` that executes `CREATE REL TABLE IF NOT EXISTS RELATES_TO (FROM Entity TO Entity, uuid STRING, name STRING, group_id STRING, fact STRING, valid_at TIMESTAMP, invalid_at TIMESTAMP, attributes STRING)`, `CREATE NODE TABLE IF NOT EXISTS RelatesToNode_ (uuid STRING PRIMARY KEY, name STRING, group_id STRING, created_at TIMESTAMP, fact STRING, fact_embedding FLOAT[{dim}], valid_at TIMESTAMP, invalid_at TIMESTAMP, attributes STRING)`, and `CREATE REL TABLE IF NOT EXISTS MENTIONS (FROM Episodic TO Entity, group_id STRING)`; extend `init(conn, dim)` to call `create_edge_tables` so a single `init` sets up the full schema; add FTS index creation calls for `entity_name_fts` and `relates_to_fact_fts` (use syntax validated by T003)

- [ ] T006 [P] [US1] Extend `liminis-graph-core/src/types.rs` — add:
  - `RelatesToEdge { uuid, name, source_node_uuid, target_node_uuid, group_id, fact, fact_embedding: Vec<f32>, valid_at, invalid_at, attributes }` with `#[derive(Debug, Clone, Default, Serialize, Deserialize)]`
  - `MentionsEdge { episodic_uuid, entity_uuid, group_id }` with same derives
  - `ExtractionResult { entities: Vec<ExtractedEntity>, edges: Vec<ExtractedEdge> }` where `ExtractedEntity { name: String, entity_type: String, summary: String }` and `ExtractedEdge { source_name: String, target_name: String, fact: String }`
  - `EmbeddingResult { embedding: Vec<f32> }`

**Checkpoint**: `cargo build -p liminis-graph-core` clean

---

## Phase 3: Adapter Implementations

**Purpose**: Implement the two out-of-process adapters (embedding + extraction). Can proceed in parallel with Phase 2.

- [ ] T007 [P] [US1] [ADAPTER] Create `liminis-graph-core/src/embedder.rs` — `pub struct Embedder { url: String, model: String, dim: usize }` with `Embedder::from_env() -> Self` reading `LCG_EMBEDDING_URL` (default `http://127.0.0.1:8765`), `LCG_EMBEDDING_MODEL` (default `bge-base-en-v1.5`), `LCG_EMBEDDING_DIM` (default `768`); `pub async fn embed(&self, text: &str) -> Result<Vec<f32>, Error>` that POSTs `{"text": ..., "model": ...}` and deserializes `{"embedding": [...]}` response; validate no ML crate added to Cargo.toml

- [ ] T008 [P] [US1] [ADAPTER] Create `liminis-graph-core/src/extractor.rs` — `pub struct Extractor { api_key: String, model: String }` with `Extractor::from_env() -> Self` reading `ANTHROPIC_API_KEY`, `LCG_EXTRACTION_LLM` (default `claude-haiku-4-5-20251001`); `pub async fn extract(&self, episode_body: &str, group_id: &str) -> Result<ExtractionResult, Error>` that calls Anthropic `/v1/messages` with a structured-output prompt returning `ExtractionResult`; system message placed in `system` field for prompt-cache eligibility (per FR-015); deserializes content JSON from the assistant turn

**Checkpoint**: `cargo build -p liminis-graph-core` clean; `cargo tree -p liminis-graph-core | grep -E "tch|candle|onnxruntime"` returns empty

---

## Phase 4: DB Operations for Relationship Data

**Purpose**: Extend `Conn` with insert and query methods for RELATES_TO, MENTIONS, and the retrieval methods required by the IPC contract. Depends on T005 (schema) and T006 (types).

- [ ] T009 [US1] [LDB] Add to `liminis-graph-core/src/db.rs`:
  - `Conn::insert_relates_to_edge(&self, edge: &RelatesToEdge) -> Result<(), Error>` — inserts RELATES_TO rel and RelatesToNode_ node (shadow for vector search); Entity-first label order enforced on source/target (AD-8)
  - `Conn::insert_mentions_edge(&self, e: &MentionsEdge) -> Result<(), Error>` — inserts MENTIONS rel from Episodic to Entity

- [ ] T010 [P] [US1] [LDB] Add to `db.rs`:
  - `Conn::retrieve_episodes(&self, group_id: &str, last_n: usize) -> Result<Vec<EpisodicRow>, Error>` — `MATCH (ep:Episodic) WHERE ep.group_id = '...' RETURN ... ORDER BY ep.created_at DESC LIMIT n`
  - `Conn::remove_episode(&self, episode_uuid: &str) -> Result<(), Error>` — `MATCH (ep:Episodic {uuid: '...'}) DETACH DELETE ep`

- [ ] T011 [P] [US1] [LDB] Add to `db.rs`:
  - `Conn::get_entities_by_group_ids(&self, group_ids: &[&str]) -> Result<Vec<EntityRow>, Error>`
  - `Conn::get_edges_by_group_ids(&self, group_ids: &[&str]) -> Result<Vec<RelatesToEdge>, Error>`
  - `Conn::get_edges_by_uuids(&self, uuids: &[&str]) -> Result<Vec<RelatesToEdge>, Error>`

- [ ] T012 [P] [US1] [LDB] Add to `db.rs`:
  - `Conn::cypher_query(&self, query: &str) -> Result<Vec<Vec<String>>, Error>` — pass-through raw Cypher, returns rows as Vec<Vec<String>>
  - `Conn::build_indices_and_constraints(&self) -> Result<(), Error>` — calls `create_vector_indexes()` (existing) + new FTS index creation; idempotent

- [ ] T013 [US1] [LDB] Add to `db.rs`:
  - `Conn::fts_search_entities(&self, query: &str, group_ids: &[&str], limit: usize) -> Result<Vec<(String, f64)>, Error>` — uses FTS syntax validated by T003; returns `(uuid, score)` pairs
  - `Conn::fts_search_edges(&self, query: &str, group_ids: &[&str], limit: usize) -> Result<Vec<(String, f64)>, Error>` — queries `RelatesToNode_` FTS index; returns `(uuid, score)` pairs

- [ ] T014 [US1] [LDB] Add to `db.rs`:
  - `Conn::vector_search_entities(&self, embedding: &[f32], group_ids: &[&str], limit: usize) -> Result<Vec<(String, f64)>, Error>` — uses HNSW query syntax validated by T004
  - `Conn::vector_search_edges(&self, embedding: &[f32], group_ids: &[&str], limit: usize) -> Result<Vec<(String, f64)>, Error>` — queries `RelatesToNode_` HNSW index

- [ ] T015 [US1] [LDB] Add to `db.rs`:
  - `Conn::brute_force_similar_entity(&self, name_embedding: &[f32], group_id: &str, threshold: f32) -> Result<Option<EntityRow>, Error>` — fetches all Entity rows in group, computes cosine similarity in Rust, returns the row with highest similarity if ≥ threshold (AD-4); used by add_episode dedup step

**Checkpoint**: `cargo build -p liminis-graph-core` clean; `cargo clippy -p liminis-graph-core -- -D warnings` clean

---

## Phase 5: Hybrid Search & Bench

**Purpose**: Implement RRF-fused hybrid search for entities and edges. Depends on T013, T014. Bench required (HOT path).

- [ ] T016 [US1] [LDB] [IPC] [HOT] Create `liminis-graph-core/src/search.rs`:
  - `pub fn rrf_fuse(bm25: &[(String, f64)], vector: &[(String, f64)]) -> Vec<String>` — pure Rust RRF: score = Σ 1/(rank+60), returns UUIDs sorted by descending score, UUID tie-breaking for determinism
  - `pub async fn hybrid_entity_search(conn: Arc<Mutex<Conn<'_>>>, embedder: &Embedder, query: &str, group_ids: &[&str], limit: usize) -> Result<Vec<EntityRow>, Error>` — embed query → FTS + vector in parallel (`join!`) → RRF fuse → fetch full EntityRow for each UUID
  - `pub async fn hybrid_edge_search(conn: Arc<Mutex<Conn<'_>>>, embedder: &Embedder, query: &str, group_ids: &[&str], limit: usize) -> Result<Vec<RelatesToEdge>, Error>` — same pattern for RelatesToNode_

- [ ] T017 [P] [US1] [HOT] Create `liminis-graph-core/benches/search.rs` with:
  - `bench_hybrid_entity_search` — populates a TempDir DB with 1000 entities, runs 10 hybrid searches, measures throughput; harness: criterion
  - `bench_hybrid_edge_search` — same pattern for edges
  - Add `[[bench]] name = "search"` entry to `liminis-graph-core/Cargo.toml`

**Checkpoint**: `cargo bench -p liminis-graph-core --bench search -- --test` passes (benchmark smoke-runs without full timing)

---

## Phase 6: Episode Pipeline

**Purpose**: Implement the add_episode pipeline. Depends on T009 (insert_relates_to_edge, insert_mentions_edge), T007 (embedder), T008 (extractor), T015 (brute_force_similar_entity).

- [ ] T018 [US1] [LDB] Create `liminis-graph-core/src/episode.rs`:
  - `pub async fn add_episode(conn: Arc<Mutex<Conn<'_>>>, embedder: &Embedder, extractor: &Extractor, name: &str, body: &str, source: &str, source_description: &str, reference_time: &str, group_id: &str) -> Result<String, Error>`
  - Pipeline: (1) embed `body` → `content_embedding`; (2) extract entities+edges via `extractor.extract(body, group_id)`; (3) for each extracted entity: embed name, call `brute_force_similar_entity`, upsert Entity row (create new or update summary if match found, enforce Entity-first label); (4) embed each extracted edge fact, insert `RelatesToEdge` (source↔target by deduped Entity UUIDs) and `RelatesToNode_` shadow node; (5) insert `EpisodicRow`; (6) for each deduped entity insert `MentionsEdge`; (7) WAL stub (`// TODO issue #3: append WAL line`); return episode UUID
  - All DB writes are in one `spawn_blocking` closure; embedding + extraction calls are async before the blocking section

**Checkpoint**: Unit test `test_add_episode_pipeline` in `episode.rs` (using mock embedder/extractor returning synthetic data, real TempDir DB) passes

---

## Phase 7: IPC Types & Handlers

**Purpose**: Wire IPC message types and the dispatcher. Depends on T016 (search), T018 (episode), T010–T012 (query methods).

- [ ] T019 [P] [US1] [IPC] Create `liminis-graph-core/src/ipc.rs`:
  - `IpcRequest { jsonrpc: String, id: Value, method: String, params: Value }` with `Deserialize`
  - `IpcResponse` enum: `Ok { jsonrpc, id, result: Value }` and `Err { jsonrpc, id, error: IpcError }` with `Serialize`, `#[serde(untagged)]`
  - `IpcError { code: i32, message: String }` with `Serialize`
  - Helper `IpcResponse::ok(id, result)` and `IpcResponse::err(id, code, msg)` constructors

- [ ] T020 [US1] [IPC] Create `liminis-graph-core/src/handlers.rs`:
  - `pub async fn dispatch(req: IpcRequest, conn: Arc<Mutex<Conn<'_>>>, embedder: Arc<Embedder>, extractor: Arc<Extractor>) -> IpcResponse` — match on `req.method`, extract params via `serde_json::from_value`, call the correct library function, serialize result to `Value`; unknown method → `IpcResponse::err(req.id, -32601, "Method not found")`
  - Implement all 11 method branches per the AD-2 table
  - `group_id` defaulting to `"liminis"` if absent (AD-6)

- [ ] T021 [US1] Update `liminis-graph-core/src/lib.rs` to `pub mod` and re-export `ipc`, `handlers`, `episode`, `search`, `embedder`, `extractor`; also update `types` re-exports for `RelatesToEdge`, `MentionsEdge`, `ExtractionResult`

**Checkpoint**: `cargo build -p liminis-graph-core` clean; `cargo clippy -p liminis-graph-core -- -D warnings` clean

---

## Phase 8: IPC Server Binary

**Purpose**: Wire the tokio UnixListener event loop in the binary. Depends on T020 (handlers).

- [ ] T022 [US1] [IPC] Update `liminis-graph/Cargo.toml` to add `tokio = { workspace = true }`, `serde_json = { workspace = true }`, `liminis-graph-core = { path = "../liminis-graph-core" }`; rewrite `liminis-graph/src/main.rs`:
  - Read `LCG_SOCKET_PATH` (default `.lcg/service.sock`), `LCG_DB_PATH` (default `.lcg/db/liminis.db`)
  - Open DB and connect; call `conn.init_schema(dim)` (idempotent)
  - Construct `Arc<Embedder>` and `Arc<Extractor>` from env
  - Bind `tokio::net::UnixListener`; accept loop spawns a task per connection
  - Per-connection task: `BufReader`/`BufWriter` line loop — deserialize `IpcRequest`, call `dispatch(...)`, serialize `IpcResponse`, write line back; handle EOF and errors gracefully
  - On `knowledge_close` response sent, drain connection and exit process cleanly
  - `#[tokio::main]` entry point

**Checkpoint**: `cargo build --release -p liminis-context-graph` succeeds; `./target/release/liminis-context-graph` starts and listens without error; manual `echo '{"jsonrpc":"2.0","id":1,"method":"knowledge_build_indices","params":{}}' | nc -U .lcg/service.sock` returns a valid JSON-RPC response

---

## Phase 9: Parity Test Corpus & CI Tests

**Purpose**: Record request/response corpus and implement the parity replay test. Depends on Phase 8 being runnable.

**⚠️ NOTE on T023**: Fixture JSON files MUST be recorded from the live upstream Python graphiti-core service against a shared baseline DB (see `tests/fixtures/README.md` capture procedure). They cannot be synthesized arbitrarily; they encode the exact Python wire shapes.

- [ ] T023 [US1] [IPC] Capture parity fixture corpus:
  - Create `liminis-graph-core/tests/fixtures/README.md` documenting the capture procedure (run Python service against `baseline_db`, record each method call/response pair with a small Python script)
  - Commit ~20 fixture files to `liminis-graph-core/tests/fixtures/ipc_corpus/` covering: `add_episode`, `knowledge_find_entities` (2 queries), `knowledge_find_relationships` (2 queries), `knowledge_get_episodes`, `knowledge_delete_episode`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`, `knowledge_query_cypher`, `knowledge_build_indices`, `knowledge_close`, and at least one error case
  - Commit the baseline DB snapshot to `liminis-graph-core/tests/fixtures/baseline_db/` (LFS or compressed archive per repo policy)

- [ ] T024 [US1] [IPC] Implement `liminis-graph-core/tests/ipc_parity.rs`:
  - Spawn `liminis-graph` binary against a copy of `baseline_db` in a TempDir, with socket at a temp path
  - For each fixture file in `ipc_corpus/`: send request over the socket, receive response, compare `result` fields structurally (serde_json Value equality, ignoring key order)
  - Assert all fixtures pass; print diff on failure for debugging
  - Golden rank-correlation test: if `PARITY_GOLDEN=1`, load `golden_queries.json`, send each `knowledge_find_entities` query, compute Spearman rank correlation against golden top-10 UUID list, assert ≥ 0.9

- [ ] T025 [P] [US1] [IPC] Update `.github/workflows/ci.yml`:
  - Add `parity` job: `ubuntu-latest`, steps: checkout → toolchain → `cargo build --release` → `cargo test --test ipc_parity`
  - Existing `test` and `bench-stub` jobs unchanged
  - `PARITY_GOLDEN` is NOT set in CI (golden test requires offline DB snapshot with Python baseline)

**Checkpoint**: `cargo test --test ipc_parity` passes locally and in CI

---

## Phase 10: Polish & Principle II Gate

**Purpose**: Ensure the library API is a peer to the binary (Principle II); update example.

- [ ] T026 [P] [US1] Update `examples/basic_ingest/main.rs` to demonstrate hybrid search: after inserting entities, call `hybrid_entity_search` with a query string and print the ranked results; stays ≤ 75 lines (was ≤ 50; the search demo warrants a small increase)

- [ ] T027 [P] [US1] Run `cargo tree -p liminis-graph-core | grep -E "tch|candle|onnxruntime"` and `cargo tree -p liminis-graph | grep -E "tch|candle|onnxruntime"` — both MUST return empty; add this as a step in `ci.yml` under the `test` job

**Checkpoint**: All CI jobs green; `cargo run --example basic_ingest` prints entity names and search results

---

## Dependencies & Execution Order

| Phase | Depends On | Notes |
|-------|-----------|-------|
| Phase 1 (T001) | nothing | Must complete first |
| Phase 1 (T002–T004) | T001 | Parallel spike tests; must all pass before Phase 4+ |
| Phase 2 (T005, T006) | T002 (REL TABLE spike) | Parallel with each other |
| Phase 3 (T007, T008) | T001 | Parallel with Phase 2 |
| Phase 4 (T009–T015) | T005, T006; T013→T003; T014→T004 | T009–T012 can start after T005/T006; T013 needs T003 result; T014 needs T004 result; T010–T012 parallel |
| Phase 5 (T016, T017) | T013, T014 | T016 and T017 parallel |
| Phase 6 (T018) | T009, T015, T007, T008 | Serial (pipeline wires everything) |
| Phase 7 (T019, T020, T021) | T016, T018 | T019 parallel with T020; T021 after T019+T020 |
| Phase 8 (T022) | T020, T021 | |
| Phase 9 (T023) | T022 (service runnable) | T024 after T023; T025 after T024 |
| Phase 10 (T026, T027) | Phase 9 green | Parallel with each other |

### Parallel Opportunities

- T002, T003, T004 all parallel (different test functions, no shared state)
- T005, T006, T007, T008 all parallel once T001 done
- T010, T011, T012, T013 parallel (different Conn methods, different files/sections)
- T016, T017 parallel (search.rs + benches/search.rs are independent)
- T026, T027 parallel (example + CI script, no shared files)

---

## Constitution Compliance

| Task(s) | Tag | Gate |
|---------|-----|------|
| T002, T003, T004 | [LDB] | Spike tests ARE the parity gate for lbug query behaviors; must pass on both OS runners |
| T013, T014, T016 | [LDB][HOT] | `benches/search.rs` (T017) exists and bench compiles before T016 merge |
| T019, T020, T022, T024, T025 | [IPC] | Parity corpus (T023) committed; `cargo test --test ipc_parity` passes in CI (T025) |
| T007, T008 | [ADAPTER] | `cargo tree | grep -E "tch|candle|onnxruntime"` returns empty (verified in T027) |
| All | [Principle III] | `grep -r "trait.*Driver\|dyn.*Db\|Box.*Connection" src/` returns empty |
