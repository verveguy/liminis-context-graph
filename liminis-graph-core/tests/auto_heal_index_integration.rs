// FR-008 integration test: empty DB → insert entity → knowledge_find_entities → non-error.
//
// Proves the auto-heal pattern from issue #58: calling a search handler on a freshly
// opened DB (init_schema only, no build_indices_and_constraints) succeeds by triggering
// build_indices_once() internally on the first missing-index binder error from lbug.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwapOption;
use liminis_graph_core::{
    app_state::AppState, db::Db, dedup_adapter::PassthroughDedupAdapter, embedder::MockEmbedder,
    extractor::MockExtractor, handlers, ipc::IpcRequest, telemetry::NoopSink, EntityRow,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;

// lbug installs vector/fts extensions into a global directory (~/.lbdb/extension/) on the
// first Db::open call. Concurrent opens race on directory creation. Serialize them here.
static DB_OPEN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn make_state_without_indices(dim: usize) -> (Arc<AppState>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("auto_heal_test.db");
    let db_path_str = db_path.to_str().unwrap().to_string();
    let db = {
        let _open_guard = DB_OPEN_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        Arc::new(Db::open(&db_path_str).unwrap())
    };
    {
        let conn = db.connect().unwrap();
        // init_schema creates tables and FTS indexes but intentionally NOT HNSW vector indexes.
        // This matches the bug scenario: the service starts with a fresh DB.
        conn.init_schema(dim).unwrap();
    }
    let state = Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(dim)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink: Arc::new(NoopSink),
        db_path: db_path_str,
        wal_dir: None,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        shutdown: Arc::new(AtomicBool::new(false)),
    });
    (state, dir)
}

#[tokio::test]
async fn find_entities_auto_heals_on_fresh_db() {
    let dim = 4;
    let (state, _dir) = make_state_without_indices(dim);

    // Insert an entity directly to ensure the HNSW index lookup is exercised.
    // Without data in the table, lbug may return empty results before hitting the index.
    {
        let db = state.db.load_full().unwrap();
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "test-entity-heal-1".to_string(),
            name: "AutoHealEntity".to_string(),
            group_id: "test-group".to_string(),
            labels: vec![],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0f32; dim],
            summary: "Entity inserted before indices are built".to_string(),
            attributes: "{}".to_string(),
        })
        .unwrap();
    }

    // Search without ever calling knowledge_build_indices — auto-heal fires on the
    // first binder error and retries transparently.
    let request = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: "knowledge_find_entities".to_string(),
        params: json!({"query": "AutoHealEntity", "group_ids": ["test-group"], "num_results": 5}),
    };
    let response = handlers::dispatch(request, Arc::clone(&state), None).await;
    let v: Value = serde_json::to_value(&response).unwrap();

    assert!(
        v.get("result").is_some(),
        "Expected a successful result from knowledge_find_entities after auto-heal, got: {v}"
    );
    assert!(
        v.get("error").is_none(),
        "Expected no error after auto-heal, got: {v}"
    );

    // The indices_built flag must be set so subsequent searches skip the auto-heal path.
    assert!(
        state.indices_built.load(Ordering::Acquire),
        "indices_built flag should be true after a successful auto-heal"
    );
}

#[tokio::test]
async fn find_entities_second_search_skips_auto_heal() {
    let dim = 4;
    let (state, _dir) = make_state_without_indices(dim);

    // Pre-set the flag to true (simulates a session where indices are already built)
    state.indices_built.store(true, Ordering::Release);

    // With indices_built=true the handler returns MISSING_INDEX_USER_MSG rather than
    // attempting another build. On a fresh DB without HNSW indexes, this will return
    // an IPC error (not a raw binder error), verifying FR-003 and FR-007.
    let request = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(2),
        method: "knowledge_find_entities".to_string(),
        params: json!({"query": "anything", "group_ids": ["test-group"], "num_results": 5}),
    };

    // Insert entity so the HNSW lookup is triggered.
    {
        let db = state.db.load_full().unwrap();
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "test-entity-heal-2".to_string(),
            name: "AnotherEntity".to_string(),
            group_id: "test-group".to_string(),
            labels: vec![],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0f32; dim],
            summary: "Second test entity".to_string(),
            attributes: "{}".to_string(),
        })
        .unwrap();
    }

    let response = handlers::dispatch(request, Arc::clone(&state), None).await;
    let v: Value = serde_json::to_value(&response).unwrap();

    // With indices_built=true and data in the DB, lbug will attempt the HNSW lookup,
    // encounter the missing index, and surface a binder error — which the handler must
    // rewrite to MISSING_INDEX_USER_MSG rather than returning the raw trace (FR-003, FR-007).
    let err = v
        .get("error")
        .expect("Expected error response when indices_built=true and HNSW index is missing");
    let msg = err["message"].as_str().unwrap_or("");
    assert!(
        !msg.contains("Binder exception:"),
        "Raw binder error must not be surfaced (FR-007), got: {msg}"
    );
    assert!(
        msg.contains("knowledge_build_indices"),
        "Error must name the recovery step (FR-007), got: {msg}"
    );
}
