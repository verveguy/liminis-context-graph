// IPC parity tests: structural JSON-RPC 2.0 correctness for all 11 wire methods.
//
// Each test calls handlers::dispatch() in-process and checks that:
//   1. The response is valid JSON-RPC 2.0 (has "jsonrpc":"2.0" and matching "id")
//   2. The result has the expected shape for that method
//
// Methods that require external embedding/extraction services (find_entities,
// find_relationships, add_episode) are exercised only for error-shape correctness —
// the embedder points at an unreachable address so HTTP fails with a wrapped -32000 error.
//
// To enable exact Python-vs-Rust parity comparison, capture fixtures with
// scripts/record_corpus.py and set PARITY_GOLDEN=1 (see tests/fixtures/README.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{MockEmbedder, OaiEmbedder},
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    telemetry::{NoopSink, TelemetrySink},
    EntityRow,
};
use regex::Regex;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("parity.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

fn make_state(db: Arc<Db>) -> Arc<AppState> {
    // MockExtractor + PassthroughDedupAdapter + default Embedder (unreachable URL in CI)
    // Methods that call embed() will fail with -32000 — that's expected for those tests.
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(OaiEmbedder::from_env()),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

#[allow(dead_code)]
fn make_degraded_state(reason: &str) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(None),
        degraded_reason: Arc::new(Mutex::new(Some(reason.to_string()))),
        embedder: Arc::new(OaiEmbedder::from_env()),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test-degraded.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

fn req(id: i64, method: &str, params: Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    }
}

fn assert_ok_resp(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc field wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("result").is_some(), "expected result, got: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
}

fn assert_err_resp(v: &Value, id: i64, expected_code: i32) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc field wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("error").is_some(), "expected error field: {v}");
    assert_eq!(v["error"]["code"], expected_code, "wrong error code: {v}");
}

async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn parity_build_indices() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(1, "knowledge_build_indices", json!({}), state).await;
    assert_ok_resp(&v, 1);
    assert_eq!(v["result"]["status"], "ok");
}

#[tokio::test]
async fn parity_get_episodes_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        2,
        "knowledge_get_episodes",
        json!({"group_id": "parity_group", "last_n": 10}),
        state,
    )
    .await;
    assert_ok_resp(&v, 2);
    assert!(v["result"].is_object(), "expected object envelope: {v}");
    assert!(
        v["result"]["episodes"].is_array(),
        "expected episodes array: {v}"
    );
    assert_eq!(v["result"]["count"], 0);
}

#[tokio::test]
async fn parity_get_nodes_by_group_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        3,
        "knowledge_get_nodes_by_group",
        json!({"group_ids": ["parity_group"]}),
        state,
    )
    .await;
    assert_ok_resp(&v, 3);
    assert!(v["result"].is_object(), "expected object envelope: {v}");
    assert!(v["result"]["nodes"].is_array(), "expected nodes array: {v}");
    assert_eq!(v["result"]["count"], 0);
}

#[tokio::test]
async fn parity_get_edges_by_group_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        4,
        "knowledge_get_edges_by_group",
        json!({"group_ids": ["parity_group"]}),
        state,
    )
    .await;
    assert_ok_resp(&v, 4);
    assert!(v["result"].is_object(), "expected object envelope: {v}");
    assert!(v["result"]["edges"].is_array(), "expected edges array: {v}");
    assert_eq!(v["result"]["count"], 0);
}

#[tokio::test]
async fn parity_get_edges_by_uuids_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        5,
        "knowledge_get_edges_by_uuids",
        json!({"uuids": []}),
        state,
    )
    .await;
    assert_ok_resp(&v, 5);
    assert!(v["result"].is_object(), "expected object envelope: {v}");
    assert!(v["result"]["edges"].is_array(), "expected edges array: {v}");
    assert_eq!(v["result"]["count"], 0);
}

#[tokio::test]
async fn parity_query_cypher() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        6,
        "knowledge_query_cypher",
        json!({"query": "MATCH (n:Entity) RETURN n.uuid LIMIT 1"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 6);
    assert!(v["result"]["rows"].is_array(), "expected rows array: {v}");
}

#[tokio::test]
async fn parity_delete_episode_noop() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        7,
        "knowledge_delete_episode",
        json!({"episode_uuid": "00000000-0000-0000-0000-000000000001"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 7);
    assert_eq!(v["result"]["status"], "deleted");
}

#[tokio::test]
async fn parity_close() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(8, "knowledge_close", json!({}), state).await;
    assert_ok_resp(&v, 8);
    assert_eq!(v["result"]["status"], "closed");
}

#[tokio::test]
async fn parity_unknown_method_returns_error() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(9, "no_such_method", json!({}), state).await;
    assert_err_resp(&v, 9, -32000);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("no_such_method"),
        "error message should name the method: {v}"
    );
}

#[tokio::test]
async fn parity_find_entities_requires_embedder() {
    // Embedding call fails (no server at default URL) → -32000 error with an HTTP message.
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        10,
        "knowledge_find_entities",
        json!({"query": "Alice", "group_ids": ["g"], "num_results": 5}),
        state,
    )
    .await;
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 10);
    assert!(
        v.get("result").is_some() || v["error"]["code"] == -32000,
        "unexpected response shape: {v}"
    );
}

#[tokio::test]
async fn parity_find_relationships_requires_embedder() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        11,
        "knowledge_find_relationships",
        json!({"query": "works at", "group_ids": ["g"], "num_results": 5}),
        state,
    )
    .await;
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 11);
    assert!(
        v.get("result").is_some() || v["error"]["code"] == -32000,
        "unexpected response shape: {v}"
    );
}

// ── Helpers for Tier 1a handshake tests ──────────────────────────────────────

fn make_state_with_mock_embed(db: Arc<Db>) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

fn make_state_with_workspace(db: Arc<Db>, workspace_root: PathBuf) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(OaiEmbedder::from_env()),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: Some(workspace_root),
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

// ── Tier 1a: health_check ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_health_check_ok() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(20, "health_check", json!({}), state).await;
    assert_ok_resp(&v, 20);
    assert_eq!(v["result"]["ok"], true, "expected ok:true: {v}");
    assert_eq!(v["result"]["healthy"], true, "expected healthy:true: {v}");
}

// ── Tier 1a: knowledge_status ─────────────────────────────────────────────────

#[tokio::test]
async fn test_knowledge_status_empty_db() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(21, "knowledge_status", json!({}), state).await;
    assert_ok_resp(&v, 21);
    let r = &v["result"];
    assert_eq!(r["entity_count"], 0, "expected 0 entities: {v}");
    assert_eq!(r["relationship_count"], 0, "expected 0 relationships: {v}");
    assert_eq!(r["episode_count"], 0, "expected 0 episodes: {v}");
    assert_eq!(r["wal"]["exists"], false, "expected wal.exists:false: {v}");
    assert!(
        r["database_path"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "expected non-empty database_path: {v}"
    );
    assert!(
        r["embedding_model"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "expected non-empty embedding_model: {v}"
    );
    assert!(
        r["embedding_dim"].as_u64().is_some(),
        "expected numeric embedding_dim: {v}"
    );
    assert_eq!(
        r["context_graph_initialized"], true,
        "expected context_graph_initialized:true: {v}"
    );
    assert_eq!(r["connected"], true, "expected connected:true: {v}");
    assert_eq!(r["initializing"], false, "expected initializing:false: {v}");
    assert!(
        r["last_index_time"].is_null(),
        "expected last_index_time:null on empty db: {v}"
    );
    assert!(
        r.get("index_created_at").is_none(),
        "expected index_created_at to be absent from empty-DB response: {v}"
    );
}

#[tokio::test]
async fn test_knowledge_status_counts() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    // Insert one episode via knowledge_process_chunk; MockExtractor yields 2 entities, 1 edge.
    let ingest = dispatch_val(
        22,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-001",
            "source_file": "doc.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 22);

    let v = dispatch_val(23, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&v, 23);
    let r = &v["result"];
    assert_eq!(r["entity_count"], 2, "expected 2 entities: {v}");
    assert_eq!(r["episode_count"], 1, "expected 1 episode: {v}");
    assert_eq!(
        r["relationship_count"], 1,
        "expected 1 RELATES_TO relationship: {v}"
    );
    assert_eq!(
        r["context_graph_initialized"], true,
        "expected context_graph_initialized:true: {v}"
    );
    assert!(
        r["last_index_time"].as_str().is_some(),
        "expected non-null last_index_time after ingestion: {v}"
    );
    let ica = r["index_created_at"]
        .as_str()
        .expect("expected index_created_at to be a string");
    let iso8601 = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").unwrap();
    assert!(
        iso8601.is_match(ica),
        "expected index_created_at to be ISO 8601 UTC, got: {ica}"
    );
}

// ── Tier 1a: knowledge_process_chunk ─────────────────────────────────────────

#[tokio::test]
async fn test_knowledge_process_chunk_ok() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);
    let v = dispatch_val(
        30,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "test-chunk-1",
            "source_file": "test.txt",
            "reference_time": "2024-06-01T12:00:00Z",
        }),
        state,
    )
    .await;
    assert_ok_resp(&v, 30);
    let r = &v["result"];
    assert_eq!(r["success"], true, "expected success:true: {v}");
    assert_eq!(r["chunk_id"], "test-chunk-1");
    assert_eq!(r["source_file"], "test.txt");
    assert!(
        r["episode_uuid"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "expected non-empty episode_uuid: {v}"
    );
    assert!(
        r["nodes_extracted"].as_u64().is_some(),
        "expected numeric nodes_extracted: {v}"
    );
    assert!(
        r["edges_extracted"].as_u64().is_some(),
        "expected numeric edges_extracted: {v}"
    );
    assert!(
        r["duration_seconds"].as_f64().is_some(),
        "expected numeric duration_seconds: {v}"
    );
}

#[tokio::test]
async fn test_knowledge_process_chunk_duplicate_chunk_id() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);
    let params = json!({
        "chunk_text": "Alice works at Acme Corp.",
        "chunk_id": "dup-chunk",
        "source_file": "test.txt",
        "reference_time": "2024-06-01T12:00:00Z",
    });
    let v1 = dispatch_val(
        31,
        "knowledge_process_chunk",
        params.clone(),
        Arc::clone(&state),
    )
    .await;
    let v2 = dispatch_val(32, "knowledge_process_chunk", params, Arc::clone(&state)).await;
    assert_ok_resp(&v1, 31);
    assert_ok_resp(&v2, 32);
    let uuid1 = v1["result"]["episode_uuid"].as_str().unwrap();
    let uuid2 = v2["result"]["episode_uuid"].as_str().unwrap();
    assert_ne!(
        uuid1, uuid2,
        "duplicate chunk_id must produce distinct episode_uuid values"
    );
}

#[tokio::test]
async fn test_knowledge_process_chunk_rejects_empty_chunk_text() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        33,
        "knowledge_process_chunk",
        json!({ "chunk_text": "", "chunk_id": "c1", "source_file": "f.txt" }),
        state,
    )
    .await;
    assert_err_resp(&v, 33, -32000);
}

#[tokio::test]
async fn test_knowledge_process_chunk_rejects_missing_chunk_id() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        34,
        "knowledge_process_chunk",
        json!({ "chunk_text": "some text", "source_file": "f.txt" }),
        state,
    )
    .await;
    assert_err_resp(&v, 34, -32000);
}

// ── Tier 1b: knowledge_search_passages ───────────────────────────────────────

#[tokio::test]
async fn parity_search_passages_empty_db() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);
    let v = dispatch_val(
        40,
        "knowledge_search_passages",
        serde_json::json!({"query": "test passage", "num_results": 5, "min_score": 0.0}),
        state,
    )
    .await;
    assert_ok_resp(&v, 40);
    assert!(
        v["result"]["passages"].is_array(),
        "expected passages array: {v}"
    );
    assert_eq!(
        v["result"]["count"], 0,
        "empty db should yield 0 passages: {v}"
    );
}

#[tokio::test]
async fn parity_search_passages_empty_query() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);
    let v = dispatch_val(
        41,
        "knowledge_search_passages",
        serde_json::json!({"query": "", "num_results": 5}),
        state,
    )
    .await;
    assert_err_resp(&v, 41, -32000);
}

// ── Tier 1b: knowledge_list_entities ─────────────────────────────────────────

#[tokio::test]
async fn parity_list_entities_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(42, "knowledge_list_entities", json!({}), state).await;
    assert_ok_resp(&v, 42);
    assert!(v["result"]["nodes"].is_array(), "expected nodes array: {v}");
    assert_eq!(v["result"]["count"], 0, "empty db: {v}");
}

#[tokio::test]
async fn parity_list_entities_invalid_num_results() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        43,
        "knowledge_list_entities",
        json!({"num_results": 0}),
        state,
    )
    .await;
    assert_err_resp(&v, 43, -32000);
}

// ── Tier 1b: knowledge_list_relationships ────────────────────────────────────

#[tokio::test]
async fn parity_list_relationships_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(44, "knowledge_list_relationships", json!({}), state).await;
    assert_ok_resp(&v, 44);
    assert!(v["result"]["facts"].is_array(), "expected facts array: {v}");
    assert_eq!(v["result"]["count"], 0, "empty db: {v}");
}

// ── Tier 1b: knowledge_get_entity_neighbors ───────────────────────────────────

#[tokio::test]
async fn parity_get_entity_neighbors_missing_uuid() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(45, "knowledge_get_entity_neighbors", json!({}), state).await;
    assert_err_resp(&v, 45, -32000);
}

#[tokio::test]
async fn parity_get_entity_neighbors_nonexistent() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        46,
        "knowledge_get_entity_neighbors",
        json!({"entity_uuid": "00000000-0000-0000-0000-000000000099"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 46);
    assert!(v["result"]["nodes"].is_array(), "expected nodes: {v}");
    assert!(v["result"]["edges"].is_array(), "expected edges: {v}");
    assert_eq!(
        v["result"]["node_count"], 0,
        "no neighbors for nonexistent uuid: {v}"
    );
    assert_eq!(
        v["result"]["edge_count"], 0,
        "no edges for nonexistent uuid: {v}"
    );
}

// ── Tier 1b: knowledge_get_entities_by_source ────────────────────────────────

#[tokio::test]
async fn parity_get_entities_by_source_empty_source() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        47,
        "knowledge_get_entities_by_source",
        json!({"source": ""}),
        state,
    )
    .await;
    assert_err_resp(&v, 47, -32000);
}

#[tokio::test]
async fn parity_get_entities_by_source_no_match() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        48,
        "knowledge_get_entities_by_source",
        json!({"source": "nonexistent-source-xyz"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 48);
    assert!(v["result"]["nodes"].is_array(), "expected nodes: {v}");
    assert_eq!(v["result"]["count"], 0, "no match: {v}");
}

#[tokio::test]
async fn test_knowledge_process_chunk_rejects_bad_reference_time() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        35,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "some text",
            "chunk_id": "c1",
            "source_file": "f.txt",
            "reference_time": "not-a-date",
        }),
        state,
    )
    .await;
    assert_err_resp(&v, 35, -32000);
}

// ── Tier 3: corrections ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_validate_corrections_no_workspace() {
    // workspace_root is None — all corrections methods should return a structured error
    let (db, _dir) = make_db(4);
    let state = make_state(db); // workspace_root: None
    let v = dispatch_val(50, "knowledge_validate_corrections", json!({}), state).await;
    assert_err_resp(&v, 50, -32000);
}

#[tokio::test]
async fn test_validate_corrections_no_file() {
    // workspace_root set but no .liminis/knowledge-corrections.yaml exists
    let (db, _dir) = make_db(4);
    let workspace_dir = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace_dir.path().to_path_buf());
    let v = dispatch_val(51, "knowledge_validate_corrections", json!({}), state).await;
    assert_ok_resp(&v, 51);
    let r = &v["result"];
    assert_eq!(r["valid"], true, "no file should be valid:true: {v}");
    assert_eq!(r["total_corrections"], 0, "should be 0: {v}");
    assert_eq!(r["unapplied_corrections"], 0, "should be 0: {v}");
    assert!(
        r["issues"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "no issues: {v}"
    );
}

#[tokio::test]
async fn test_apply_corrections_no_file() {
    let (db, _dir) = make_db(4);
    let workspace_dir = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace_dir.path().to_path_buf());
    let v = dispatch_val(52, "knowledge_apply_corrections", json!({}), state).await;
    assert_ok_resp(&v, 52);
    let r = &v["result"];
    assert_eq!(r["success"], true, "no file should succeed: {v}");
    assert_eq!(r["applied"], 0, "nothing applied: {v}");
}

#[tokio::test]
async fn test_apply_corrections_dry_run() {
    let (db, _dir) = make_db(4);
    let workspace_dir = TempDir::new().unwrap();

    // Create .liminis/knowledge-corrections.yaml with two unapplied retract entries
    let liminis_dir = workspace_dir.path().join(".liminis");
    std::fs::create_dir_all(&liminis_dir).unwrap();
    let corrections_path = liminis_dir.join("knowledge-corrections.yaml");
    std::fs::write(
        &corrections_path,
        "corrections:\n  - id: r1\n    type: retract\n    edge_uuid: nonexistent-uuid-1\n  - id: r2\n    type: retract\n    edge_uuid: nonexistent-uuid-2\n",
    )
    .unwrap();

    let before = std::fs::read_to_string(&corrections_path).unwrap();
    let state = make_state_with_workspace(db, workspace_dir.path().to_path_buf());
    let v = dispatch_val(
        53,
        "knowledge_apply_corrections",
        json!({"dry_run": true}),
        state,
    )
    .await;
    assert_ok_resp(&v, 53);
    let r = &v["result"];
    // Edge existence is validated even in dry_run (FR-015). Both retract entries reference
    // nonexistent edge UUIDs, so success is false and errors has one entry per failing correction.
    assert_eq!(
        r["success"], false,
        "dry_run with nonexistent edges must fail: {v}"
    );
    assert_eq!(r["applied"], 0, "dry_run must not apply: {v}");
    let errs = r["errors"].as_array().expect("errors must be an array");
    assert_eq!(
        errs.len(),
        2,
        "expected one error per nonexistent edge: {v}"
    );

    // File must be byte-identical after dry_run — patch_applied_at is not called in dry_run
    let after = std::fs::read_to_string(&corrections_path).unwrap();
    assert_eq!(
        before, after,
        "dry_run must not modify the corrections file"
    );
}

#[tokio::test]
async fn test_reprocess_entity_types_no_entities() {
    let (db, _dir) = make_db(4);
    let workspace_dir = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace_dir.path().to_path_buf());
    let v = dispatch_val(
        54,
        "knowledge_reprocess_entity_types",
        json!({"group_id": "test_group"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 54);
    let r = &v["result"];
    assert_eq!(r["success"], true, "no entities to reprocess: {v}");
    assert_eq!(r["reclassified_count"], 0, "nothing to reclassify: {v}");
}

// ── Tier 1b regression: two-hop RELATES_TO traversal ─────────────────────────
//
// These tests verify that list_relationships and get_entity_neighbors return
// populated results after ingestion via add_episode (the Rust write path).
// They guard against regressions where the two-hop write (Entity→RelatesToNode_→Entity)
// or two-hop read (MATCH ...→rn:RelatesToNode_→...) is accidentally removed.

#[tokio::test]
async fn test_list_relationships_after_ingest() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    // Ingest one episode; MockExtractor yields Alice-[works_at]->Acme Corp.
    let ingest = dispatch_val(
        60,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-list-rel",
            "source_file": "doc.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 60);

    let v = dispatch_val(
        61,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 61);
    let facts = v["result"]["facts"]
        .as_array()
        .expect("expected facts array");
    assert!(
        !facts.is_empty(),
        "expected ≥1 relationship after ingest, got 0 — two-hop write/read may be broken: {v}"
    );
    let fact = &facts[0];
    assert!(
        fact["uuid"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "fact uuid should be non-empty: {v}"
    );
    assert!(
        fact["fact"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "fact.fact should be non-empty: {v}"
    );
}

#[tokio::test]
async fn test_get_entity_neighbors_after_ingest() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    // Ingest one episode; MockExtractor yields Alice-[works_at]->Acme Corp.
    let ingest = dispatch_val(
        62,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-neighbors",
            "source_file": "doc.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 62);

    // Get the source entity UUID from list_relationships.
    let lr = dispatch_val(
        63,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&lr, 63);
    let facts = lr["result"]["facts"]
        .as_array()
        .expect("expected facts array");
    assert!(!facts.is_empty(), "expected ≥1 relationship: {lr}");
    let src_uuid = facts[0]["source_node_uuid"]
        .as_str()
        .expect("expected source_node_uuid")
        .to_string();
    assert!(!src_uuid.is_empty(), "source_node_uuid must be non-empty");

    let v = dispatch_val(
        64,
        "knowledge_get_entity_neighbors",
        json!({"entity_uuid": src_uuid}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 64);
    let edge_count = v["result"]["edge_count"].as_u64().unwrap_or(0);
    assert!(
        edge_count >= 1,
        "expected ≥1 neighbor edge for entity {src_uuid}, got {edge_count} — \
         two-hop write/read may be broken: {v}"
    );
}

// ── Tier 1b: source-info enrichment (episode_uuids / source_descriptions) ────
//
// These tests ingest an episode with a known source_description, then call all
// four Tier 1b list/neighbor methods and assert that each returned node and edge
// carries non-empty episode_uuids and source_descriptions arrays that include the
// expected episode UUID and source_description value.

#[tokio::test]
async fn test_source_info_enrichment_list_entities() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        70,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-src-01",
            "source_file": "enrich.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 70);
    let ep_uuid = ingest["result"]["episode_uuid"]
        .as_str()
        .expect("expected episode_uuid")
        .to_string();
    // source_description is "<source_file>:<chunk_id>"
    let expected_src_desc = "enrich.txt:chunk-src-01";

    let v = dispatch_val(71, "knowledge_list_entities", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&v, 71);
    let nodes = v["result"]["nodes"]
        .as_array()
        .expect("expected nodes array");
    assert!(!nodes.is_empty(), "expected ≥1 node after ingest: {v}");
    for node in nodes {
        let ep_uuids = node["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = node["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert!(
            !ep_uuids.is_empty(),
            "expected non-empty episode_uuids for node: {node}"
        );
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "episode_uuids and source_descriptions must be same length: {node}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in node episode_uuids: {node}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description {expected_src_desc} in node: {node}"
        );
    }
}

#[tokio::test]
async fn test_source_info_enrichment_list_relationships() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        72,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-src-02",
            "source_file": "enrich.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 72);
    let ep_uuid = ingest["result"]["episode_uuid"]
        .as_str()
        .expect("expected episode_uuid")
        .to_string();
    let expected_src_desc = "enrich.txt:chunk-src-02";

    let v = dispatch_val(
        73,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 73);
    let facts = v["result"]["facts"]
        .as_array()
        .expect("expected facts array");
    assert!(
        !facts.is_empty(),
        "expected ≥1 relationship after ingest: {v}"
    );
    for fact in facts {
        let ep_uuids = fact["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = fact["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert!(
            !ep_uuids.is_empty(),
            "expected non-empty episode_uuids for edge: {fact}"
        );
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "episode_uuids and source_descriptions must be same length: {fact}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in edge episode_uuids: {fact}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description {expected_src_desc} in edge: {fact}"
        );
    }
}

#[tokio::test]
async fn test_source_info_enrichment_get_entity_neighbors() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        74,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-src-03",
            "source_file": "enrich.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 74);
    let ep_uuid = ingest["result"]["episode_uuid"]
        .as_str()
        .expect("expected episode_uuid")
        .to_string();
    let expected_src_desc = "enrich.txt:chunk-src-03";

    // Get a source entity UUID via list_relationships.
    let lr = dispatch_val(
        75,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&lr, 75);
    let facts = lr["result"]["facts"].as_array().expect("expected facts");
    assert!(!facts.is_empty(), "expected ≥1 relationship: {lr}");
    let src_uuid = facts[0]["source_node_uuid"]
        .as_str()
        .expect("expected source_node_uuid")
        .to_string();

    let v = dispatch_val(
        76,
        "knowledge_get_entity_neighbors",
        json!({"entity_uuid": src_uuid}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 76);
    let nodes = v["result"]["nodes"].as_array().expect("expected nodes");
    let edges = v["result"]["edges"].as_array().expect("expected edges");
    assert!(
        !nodes.is_empty() || !edges.is_empty(),
        "expected results after ingest: {v}"
    );

    for node in nodes {
        let ep_uuids = node["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = node["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "positional alignment: {node}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in neighbor node: {node}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description in neighbor node: {node}"
        );
    }

    for edge in edges {
        let ep_uuids = edge["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = edge["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "positional alignment: {edge}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in neighbor edge: {edge}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description in neighbor edge: {edge}"
        );
    }
}

#[tokio::test]
async fn test_source_info_enrichment_get_entities_by_source() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        77,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-src-04",
            "source_file": "unique-enrich.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 77);
    let ep_uuid = ingest["result"]["episode_uuid"]
        .as_str()
        .expect("expected episode_uuid")
        .to_string();
    let expected_src_desc = "unique-enrich.txt:chunk-src-04";

    let v = dispatch_val(
        78,
        "knowledge_get_entities_by_source",
        json!({"source": "unique-enrich.txt"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 78);
    let nodes = v["result"]["nodes"].as_array().expect("expected nodes");
    assert!(!nodes.is_empty(), "expected ≥1 node for source match: {v}");
    for node in nodes {
        let ep_uuids = node["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = node["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert!(
            !ep_uuids.is_empty(),
            "expected non-empty episode_uuids: {node}"
        );
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "positional alignment: {node}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in node: {node}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description {expected_src_desc} in node: {node}"
        );
    }
}

// ── Python-DB index name regression tests (FR-005) ───────────────────────────
//
// These tests open the Python-populated baseline_db fixture without any schema
// init or index creation, then call every method that queries an index by name.
// They guard against the class of bug in issue #49: Rust using a different index
// name than the upstream Python graphiti-core service used when creating the DB.
//
// The fixture at tests/fixtures/baseline_db/liminis.db is NOT committed to git.
// These tests skip gracefully when the file is absent. To populate it, run
// scripts/record_corpus.py against a live upstream Python graphiti-core service
// (see tests/fixtures/README.md).

/// Copies the baseline_db fixture into a fresh TempDir and returns the path
/// inside the copy alongside the TempDir (which must stay alive for the test).
/// Protects the original fixture from the write transactions that Db::open
/// issues (INSTALL / LOAD EXTENSION are write transactions in lbug).
fn open_baseline_db() -> Option<(PathBuf, TempDir)> {
    let src =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/baseline_db/liminis.db");
    if !src.exists() {
        return None;
    }
    let tmp = TempDir::new().ok()?;
    let dst = tmp.path().join("liminis.db");
    copy_path(&src, &dst).ok()?;
    Some((dst, tmp))
}

fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_path(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

#[test]
fn python_db_index_names_fts_entities() {
    let Some((path, _tmp)) = open_baseline_db() else {
        eprintln!(
            "SKIP python_db_index_names_fts_entities: \
             tests/fixtures/baseline_db/liminis.db absent — \
             run scripts/record_corpus.py to populate it"
        );
        return;
    };
    let db = Db::open(path.to_str().expect("baseline_db path is not valid UTF-8"))
        .expect("open baseline_db copy");
    let conn = db.connect().expect("connect");
    let result = conn.fts_search_entities("test", &["*"], 5);
    assert!(
        result.is_ok(),
        "fts_search_entities failed against Python DB (index name mismatch?): {:?}",
        result.err()
    );
}

#[test]
fn python_db_index_names_fts_edges() {
    let Some((path, _tmp)) = open_baseline_db() else {
        eprintln!(
            "SKIP python_db_index_names_fts_edges: \
             tests/fixtures/baseline_db/liminis.db absent — \
             run scripts/record_corpus.py to populate it"
        );
        return;
    };
    let db = Db::open(path.to_str().expect("baseline_db path is not valid UTF-8"))
        .expect("open baseline_db copy");
    let conn = db.connect().expect("connect");
    let result = conn.fts_search_edges("test", &["*"], 5);
    assert!(
        result.is_ok(),
        "fts_search_edges failed against Python DB (index name mismatch?): {:?}",
        result.err()
    );
}

#[test]
fn python_db_index_names_vector_entities() {
    let Some((path, _tmp)) = open_baseline_db() else {
        eprintln!(
            "SKIP python_db_index_names_vector_entities: \
             tests/fixtures/baseline_db/liminis.db absent — \
             run scripts/record_corpus.py to populate it"
        );
        return;
    };
    let db = Db::open(path.to_str().expect("baseline_db path is not valid UTF-8"))
        .expect("open baseline_db copy");
    let conn = db.connect().expect("connect");
    // Python DBs use 768-dim bge-base-en-v1.5 embeddings; zero-vector confirms index resolves.
    let result = conn.vector_search_entities(&vec![0.0_f32; 768], &["*"], 5);
    assert!(
        result.is_ok(),
        "vector_search_entities failed against Python DB (index name mismatch?): {:?}",
        result.err()
    );
}

#[test]
fn python_db_index_names_vector_edges() {
    let Some((path, _tmp)) = open_baseline_db() else {
        eprintln!(
            "SKIP python_db_index_names_vector_edges: \
             tests/fixtures/baseline_db/liminis.db absent — \
             run scripts/record_corpus.py to populate it"
        );
        return;
    };
    let db = Db::open(path.to_str().expect("baseline_db path is not valid UTF-8"))
        .expect("open baseline_db copy");
    let conn = db.connect().expect("connect");
    // Python DBs use 768-dim bge-base-en-v1.5 embeddings; zero-vector confirms index resolves.
    let result = conn.vector_search_edges(&vec![0.0_f32; 768], &["*"], 5);
    assert!(
        result.is_ok(),
        "vector_search_edges failed against Python DB (index name mismatch?): {:?}",
        result.err()
    );
}

// ── FR-007/SC-001: relation_type surfaces in knowledge_list_relationships ─────

// After ingestion via MockExtractor (which returns WORKS_AT), every edge in the
// knowledge_list_relationships response must include a non-null relation_type field.
#[tokio::test]
async fn list_relationships_includes_relation_type() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        200,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-rt-ipc",
            "source_file": "rt_test.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 200);

    let v = dispatch_val(
        201,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 201);

    let facts = v["result"]["facts"]
        .as_array()
        .expect("expected facts array");
    assert!(
        !facts.is_empty(),
        "expected ≥1 relationship after ingest: {v}"
    );

    for fact in facts {
        let rt = fact["relation_type"]
            .as_str()
            .expect("every fact must have a string relation_type field");
        assert_eq!(
            rt, "WORKS_AT",
            "MockExtractor always returns WORKS_AT; got '{rt}'"
        );
    }
}

// ── knowledge_dump_wal ────────────────────────────────────────────────────────

/// SC-004: dump on an empty graph returns success with zero counts.
#[tokio::test]
async fn parity_dump_wal_empty_graph() {
    let (db, dir) = make_db(4);
    let state = make_state(db);

    // Use an explicit target_dir inside the TempDir so the test is self-contained.
    let target_dir = dir.path().join("dump-out");
    let v = dispatch_val(
        50,
        "knowledge_dump_wal",
        json!({ "target_dir": target_dir.to_str().unwrap() }),
        state,
    )
    .await;

    assert_ok_resp(&v, 50);
    let r = &v["result"];
    assert_eq!(r["success"], true, "success field: {v}");
    assert_eq!(r["nodes_dumped"], 0, "nodes_dumped: {v}");
    assert_eq!(r["edges_dumped"], 0, "edges_dumped: {v}");
    assert_eq!(r["files_written"], 0, "files_written: {v}");
    assert!(
        r["target_dir"].is_string(),
        "target_dir must be a string: {v}"
    );
}

// ── knowledge_merge_entities ──────────────────────────────────────────────────

/// Validation error: neither canonical_uuid nor canonical_name provided → success: false.
#[tokio::test]
async fn parity_merge_entities_missing_canonical() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        55,
        "knowledge_merge_entities",
        json!({ "merge_all_by_name": true }),
        state,
    )
    .await;
    assert_ok_resp(&v, 55);
    let r = &v["result"];
    assert_eq!(
        r["success"], false,
        "must fail when no canonical provided: {v}"
    );
    assert!(
        r["errors"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "errors must be non-empty: {v}"
    );
}

/// Canonical not found on empty graph → success: false with canonical error.
#[tokio::test]
async fn parity_merge_entities_canonical_not_found() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        56,
        "knowledge_merge_entities",
        json!({ "canonical_name": "Brett", "merge_all_by_name": true }),
        state,
    )
    .await;
    assert_ok_resp(&v, 56);
    let r = &v["result"];
    assert_eq!(
        r["success"], false,
        "must fail when canonical not found: {v}"
    );
    assert!(
        r["errors"]
            .as_array()
            .map(|a| a
                .iter()
                .any(|e| e.as_str().map(|s| s.contains("not found")).unwrap_or(false)))
            .unwrap_or(false),
        "error must mention 'not found': {v}"
    );
}

/// Single entity with given name → merged_count: 0, success: true (noop through handler).
#[tokio::test]
async fn parity_merge_entities_noop_single_entity() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "brett-parity-001".to_string(),
            name: "Brett".to_string(),
            group_id: "liminis".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "parity test entity".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }
    let state = make_state(db);
    let v = dispatch_val(
        57,
        "knowledge_merge_entities",
        json!({ "canonical_name": "Brett", "merge_all_by_name": true }),
        state,
    )
    .await;
    assert_ok_resp(&v, 57);
    let r = &v["result"];
    assert_eq!(r["success"], true, "single entity must succeed: {v}");
    assert_eq!(r["merged_count"], 0, "nothing to merge: {v}");
    assert_eq!(r["skipped"], 0, "nothing skipped: {v}");
    assert!(
        r["canonical_uuid"].is_string(),
        "canonical_uuid must be present: {v}"
    );
    assert!(
        r["edges_rewritten"].is_number(),
        "edges_rewritten must be numeric: {v}"
    );
    assert!(
        r["edges_deduplicated"].is_number(),
        "edges_deduplicated must be numeric: {v}"
    );
    assert!(r["errors"].is_array(), "errors must be an array: {v}");
}
