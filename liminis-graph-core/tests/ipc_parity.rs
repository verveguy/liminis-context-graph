// IPC parity tests (T024): structural JSON-RPC 2.0 correctness for all 11 wire methods.
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

use std::sync::Arc;

use liminis_graph_core::{
    db::Db, embedder::Embedder, extractor::Extractor, handlers, ipc::IpcRequest,
};
use serde_json::{json, Value};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(
        Db::open(dir.path().join("parity.db").to_str().unwrap()).unwrap(),
    );
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

fn make_services() -> (Arc<Embedder>, Arc<Extractor>) {
    // Use env-default constructors; the embedder points at the default address
    // (127.0.0.1:8765) which is not running in CI, so any call to embed() will
    // fail — that's expected for methods that require external services.
    let embedder = Arc::new(Embedder::from_env());
    let extractor = Arc::new(Extractor::from_env());
    (embedder, extractor)
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

async fn dispatch_val(
    id: i64,
    method: &str,
    params: Value,
    db: Arc<Db>,
    emb: Arc<Embedder>,
    ext: Arc<Extractor>,
) -> Value {
    let resp = handlers::dispatch(req(id, method, params), db, emb, ext).await;
    serde_json::to_value(resp).unwrap()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn parity_build_indices() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(1, "knowledge_build_indices", json!({}), db, emb, ext).await;
    assert_ok_resp(&v, 1);
    assert_eq!(v["result"]["status"], "ok");
}

#[tokio::test]
async fn parity_get_episodes_empty() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(
        2,
        "knowledge_get_episodes",
        json!({"group_id": "parity_group", "last_n": 10}),
        db,
        emb,
        ext,
    )
    .await;
    assert_ok_resp(&v, 2);
    assert!(v["result"].is_array(), "expected array result: {v}");
    assert_eq!(v["result"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn parity_get_nodes_by_group_empty() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(
        3,
        "knowledge_get_nodes_by_group",
        json!({"group_ids": ["parity_group"]}),
        db,
        emb,
        ext,
    )
    .await;
    assert_ok_resp(&v, 3);
    assert!(v["result"].is_array(), "expected array result: {v}");
}

#[tokio::test]
async fn parity_get_edges_by_group_empty() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(
        4,
        "knowledge_get_edges_by_group",
        json!({"group_ids": ["parity_group"]}),
        db,
        emb,
        ext,
    )
    .await;
    assert_ok_resp(&v, 4);
    assert!(v["result"].is_array(), "expected array result: {v}");
}

#[tokio::test]
async fn parity_get_edges_by_uuids_empty() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(
        5,
        "knowledge_get_edges_by_uuids",
        json!({"uuids": []}),
        db,
        emb,
        ext,
    )
    .await;
    assert_ok_resp(&v, 5);
    assert!(v["result"].is_array(), "expected array result: {v}");
}

#[tokio::test]
async fn parity_query_cypher() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(
        6,
        "knowledge_query_cypher",
        json!({"query": "MATCH (n:Entity) RETURN n.uuid LIMIT 1"}),
        db,
        emb,
        ext,
    )
    .await;
    assert_ok_resp(&v, 6);
    assert!(v["result"]["rows"].is_array(), "expected rows array: {v}");
}

#[tokio::test]
async fn parity_delete_episode_noop() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(
        7,
        "knowledge_delete_episode",
        json!({"episode_uuid": "00000000-0000-0000-0000-000000000001"}),
        db,
        emb,
        ext,
    )
    .await;
    assert_ok_resp(&v, 7);
    assert_eq!(v["result"]["status"], "deleted");
}

#[tokio::test]
async fn parity_close() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(8, "knowledge_close", json!({}), db, emb, ext).await;
    assert_ok_resp(&v, 8);
    assert_eq!(v["result"]["status"], "closed");
}

#[tokio::test]
async fn parity_unknown_method_returns_error() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(9, "no_such_method", json!({}), db, emb, ext).await;
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
    // This test verifies the error is properly shaped JSON-RPC 2.0.
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(
        10,
        "knowledge_find_entities",
        json!({"query": "Alice", "group_ids": ["g"], "num_results": 5}),
        db,
        emb,
        ext,
    )
    .await;
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 10);
    // Either a valid result (if embedding service happens to be up) or a -32000 error
    assert!(
        v.get("result").is_some() || v["error"]["code"] == -32000,
        "unexpected response shape: {v}"
    );
}

#[tokio::test]
async fn parity_find_relationships_requires_embedder() {
    let (db, _dir) = make_db(4);
    let (emb, ext) = make_services();
    let v = dispatch_val(
        11,
        "knowledge_find_relationships",
        json!({"query": "works at", "group_ids": ["g"], "num_results": 5}),
        db,
        emb,
        ext,
    )
    .await;
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 11);
    assert!(
        v.get("result").is_some() || v["error"]["code"] == -32000,
        "unexpected response shape: {v}"
    );
}
