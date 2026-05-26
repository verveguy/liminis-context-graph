// IPC response-shape conformance tests (FR-007 / SC-003).
//
// Asserts that every collection-returning IPC method returns a JSON object
// with a named collection key and a numeric `count` field — never a bare array.
// Tests run against an empty in-memory DB, so all results are empty collections
// ({count: 0, key: []}). That is sufficient to verify envelope shape.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use liminis_graph_core::{
    app_state::AppState,
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    telemetry::{NoopSink, TelemetrySink},
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("shapes.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

fn make_state(db: Arc<Db>) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "shapes_test.db".to_string(),
        wal_dir: None,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
    })
}

async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    };
    let resp = handlers::dispatch(req, state, None).await;
    serde_json::to_value(resp).unwrap()
}

fn assert_envelope(v: &Value, id: i64, collection_key: &str) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("result").is_some(), "missing result: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
    let result = &v["result"];
    assert!(
        result.is_object(),
        "result must be an object, not a bare array — method returned: {result}"
    );
    assert!(
        result[collection_key].is_array(),
        "result must have '{collection_key}' array key: {result}"
    );
    assert!(
        result["count"].is_number(),
        "result must have numeric 'count' field: {result}"
    );
    assert_eq!(
        result["count"].as_u64().unwrap(),
        result[collection_key].as_array().unwrap().len() as u64,
        "count must equal len({collection_key}): {result}"
    );
}

#[tokio::test]
async fn shape_find_entities() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        1,
        "knowledge_find_entities",
        json!({"query": "test", "num_results": 5}),
        state,
    )
    .await;
    assert_envelope(&v, 1, "nodes");
}

#[tokio::test]
async fn shape_find_relationships() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        2,
        "knowledge_find_relationships",
        json!({"query": "test", "num_results": 5}),
        state,
    )
    .await;
    assert_envelope(&v, 2, "facts");
}

#[tokio::test]
async fn shape_get_episodes() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        3,
        "knowledge_get_episodes",
        json!({"group_id": "conformance_group", "last_n": 10}),
        state,
    )
    .await;
    assert_envelope(&v, 3, "episodes");
}

#[tokio::test]
async fn shape_get_nodes_by_group() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        4,
        "knowledge_get_nodes_by_group",
        json!({"group_ids": ["conformance_group"]}),
        state,
    )
    .await;
    assert_envelope(&v, 4, "nodes");
}

#[tokio::test]
async fn shape_get_edges_by_group() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        5,
        "knowledge_get_edges_by_group",
        json!({"group_ids": ["conformance_group"]}),
        state,
    )
    .await;
    assert_envelope(&v, 5, "edges");
}

#[tokio::test]
async fn shape_get_edges_by_uuids() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        6,
        "knowledge_get_edges_by_uuids",
        json!({"uuids": []}),
        state,
    )
    .await;
    assert_envelope(&v, 6, "edges");
}

#[tokio::test]
async fn shape_search_passages() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        7,
        "knowledge_search_passages",
        json!({"query": "test", "num_results": 5, "min_score": 0.0}),
        state,
    )
    .await;
    assert_envelope(&v, 7, "passages");
}

#[tokio::test]
async fn shape_list_entities() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        8,
        "knowledge_list_entities",
        json!({"num_results": 10}),
        state,
    )
    .await;
    assert_envelope(&v, 8, "nodes");
}

#[tokio::test]
async fn shape_list_relationships() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        9,
        "knowledge_list_relationships",
        json!({"num_results": 10}),
        state,
    )
    .await;
    assert_envelope(&v, 9, "facts");
}

#[tokio::test]
async fn shape_get_entity_neighbors() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        10,
        "knowledge_get_entity_neighbors",
        json!({"entity_uuid": "00000000-0000-0000-0000-000000000001"}),
        state,
    )
    .await;
    // Dual-collection response: primary collection is nodes, with count = node_count.
    let result = &v["result"];
    assert!(result.is_object(), "result must be object: {result}");
    assert!(
        result["count"].is_number(),
        "result must have numeric 'count': {result}"
    );
    assert!(
        result["nodes"].is_array(),
        "result must have nodes array: {result}"
    );
    assert!(
        result["edges"].is_array(),
        "result must have edges array: {result}"
    );
    // count must equal node_count and nodes.len() per ADR-0050 multi-collection convention.
    assert_eq!(
        result["count"].as_u64().unwrap(),
        result["node_count"].as_u64().unwrap(),
        "count must equal node_count: {result}"
    );
    assert_eq!(
        result["count"].as_u64().unwrap(),
        result["nodes"].as_array().unwrap().len() as u64,
        "count must equal nodes.len(): {result}"
    );
}

#[tokio::test]
async fn shape_get_entities_by_source() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        11,
        "knowledge_get_entities_by_source",
        json!({"source": "conformance-test-source"}),
        state,
    )
    .await;
    assert_envelope(&v, 11, "nodes");
}
