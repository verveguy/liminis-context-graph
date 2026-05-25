// Tier 1c deletion tests: delete_by_source, delete_chunk_episode, clear_all.
//
// Each test exercises the IPC handler via handlers::dispatch() in-process.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

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
    WalWriter,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.db");
    let db = Arc::new(Db::open(path.to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

fn make_state(db: Arc<Db>, db_path: &str) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_path.to_string(),
        wal_dir: None,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
    })
}

fn make_state_with_wal(db: Arc<Db>, wal_dir: std::path::PathBuf, db_path: &str) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer =
        Some(WalWriter::new(&wal_dir, 10_000, 0).expect("failed to initialize WalWriter for test"));
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_path.to_string(),
        wal_dir: Some(wal_dir),
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(wal_writer)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
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

async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

fn assert_ok(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("result").is_some(), "expected result, got: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
}

fn assert_err(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("error").is_some(), "expected error field: {v}");
}

async fn process_chunk(id: i64, chunk_id: &str, source_file: &str, state: Arc<AppState>) -> Value {
    dispatch_val(
        id,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": chunk_id,
            "source_file": source_file,
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        state,
    )
    .await
}

async fn status_counts(id: i64, state: Arc<AppState>) -> (u64, u64, u64) {
    let v = dispatch_val(id, "knowledge_status", json!({}), state).await;
    let r = &v["result"];
    (
        r["entity_count"].as_u64().unwrap_or(0),
        r["episode_count"].as_u64().unwrap_or(0),
        r["relationship_count"].as_u64().unwrap_or(0),
    )
}

// ── delete_by_source ──────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_by_source_basic() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    // Index 2 chunks under docs/a.md and 1 under docs/b.md
    assert_ok(
        &process_chunk(1, "chunk-a1", "docs/a.md", Arc::clone(&state)).await,
        1,
    );
    assert_ok(
        &process_chunk(2, "chunk-a2", "docs/a.md", Arc::clone(&state)).await,
        2,
    );
    assert_ok(
        &process_chunk(3, "chunk-b1", "docs/b.md", Arc::clone(&state)).await,
        3,
    );

    let (_, ep_before, _) = status_counts(4, Arc::clone(&state)).await;
    assert_eq!(ep_before, 3, "expected 3 episodes before delete");

    let v = dispatch_val(
        5,
        "knowledge_delete_by_source",
        json!({"source_file": "docs/a.md"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 5);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["deleted_count"], 2, "expected 2 deleted: {v}");
    let uuids = v["result"]["deleted_uuids"].as_array().unwrap();
    assert_eq!(uuids.len(), 2, "expected 2 deleted_uuids: {v}");

    let (_, ep_after, _) = status_counts(6, Arc::clone(&state)).await;
    assert_eq!(ep_after, 1, "docs/b.md episode must survive");
}

#[tokio::test]
async fn delete_by_source_no_match() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    let v = dispatch_val(
        1,
        "knowledge_delete_by_source",
        json!({"source_file": "no-such-file.md"}),
        state,
    )
    .await;
    assert_ok(&v, 1);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["deleted_count"], 0, "{v}");
    let uuids = v["result"]["deleted_uuids"].as_array().unwrap();
    assert!(uuids.is_empty(), "{v}");
}

#[tokio::test]
async fn delete_by_source_missing_param() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    let v = dispatch_val(1, "knowledge_delete_by_source", json!({}), state).await;
    assert_err(&v, 1);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("source_file"),
        "error should mention source_file: {v}"
    );
}

// ── delete_chunk_episode ──────────────────────────────────────────────────────

#[tokio::test]
async fn delete_chunk_episode_basic() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    assert_ok(
        &process_chunk(1, "my-chunk", "file.txt", Arc::clone(&state)).await,
        1,
    );
    let (_, ep_before, _) = status_counts(2, Arc::clone(&state)).await;
    assert_eq!(ep_before, 1, "expected 1 episode before delete");

    let v = dispatch_val(
        3,
        "knowledge_delete_chunk_episode",
        json!({"chunk_id": "my-chunk"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 3);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["deleted_count"], 1, "{v}");

    let (_, ep_after, _) = status_counts(4, Arc::clone(&state)).await;
    assert_eq!(ep_after, 0, "episode should be gone");
}

#[tokio::test]
async fn delete_chunk_episode_all_revisions() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    // Process the same chunk_id twice — append-on-revision creates 2 episodes
    assert_ok(
        &process_chunk(1, "rev-chunk", "file.txt", Arc::clone(&state)).await,
        1,
    );
    assert_ok(
        &process_chunk(2, "rev-chunk", "file.txt", Arc::clone(&state)).await,
        2,
    );

    let (_, ep_before, _) = status_counts(3, Arc::clone(&state)).await;
    assert_eq!(ep_before, 2, "expected 2 episodes (2 revisions)");

    let v = dispatch_val(
        4,
        "knowledge_delete_chunk_episode",
        json!({"chunk_id": "rev-chunk"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 4);
    assert_eq!(
        v["result"]["deleted_count"], 2,
        "both revisions must be deleted: {v}"
    );

    let (_, ep_after, _) = status_counts(5, Arc::clone(&state)).await;
    assert_eq!(ep_after, 0, "all revisions should be gone");
}

#[tokio::test]
async fn delete_chunk_episode_no_match() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    let v = dispatch_val(
        1,
        "knowledge_delete_chunk_episode",
        json!({"chunk_id": "nonexistent-chunk"}),
        state,
    )
    .await;
    assert_ok(&v, 1);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["deleted_count"], 0, "{v}");
}

// ── clear_all ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn clear_all_rejected_without_confirm() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    // Populate with known content
    assert_ok(
        &process_chunk(1, "c1", "f.txt", Arc::clone(&state)).await,
        1,
    );
    let (_, ep_before, _) = status_counts(2, Arc::clone(&state)).await;
    assert_eq!(ep_before, 1, "need content to verify no-op");

    // Call without confirm
    let v = dispatch_val(
        3,
        "knowledge_clear_all",
        json!({"confirm": false}),
        Arc::clone(&state),
    )
    .await;
    assert_err(&v, 3);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("confirm"),
        "error should mention 'confirm': {v}"
    );

    // DB unchanged
    let (_, ep_after, _) = status_counts(4, Arc::clone(&state)).await;
    assert_eq!(ep_after, 1, "DB must be unchanged after rejected clear_all");
}

#[tokio::test]
async fn clear_all_wipes_and_reinitializes() {
    let (db, dir) = make_db(4);
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let state = make_state(db, &db_path);

    // Populate
    assert_ok(
        &process_chunk(1, "c1", "f.txt", Arc::clone(&state)).await,
        1,
    );
    let (_, ep_before, _) = status_counts(2, Arc::clone(&state)).await;
    assert_eq!(ep_before, 1, "need content before clear");

    // Clear
    let v = dispatch_val(
        3,
        "knowledge_clear_all",
        json!({"confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 3);
    assert_eq!(v["result"]["success"], true, "{v}");

    // All counts should be zero
    let (entities, episodes, edges) = status_counts(4, Arc::clone(&state)).await;
    assert_eq!(entities, 0, "entity_count must be 0 after clear_all");
    assert_eq!(episodes, 0, "episode_count must be 0 after clear_all");
    assert_eq!(edges, 0, "relationship_count must be 0 after clear_all");
}

#[tokio::test]
async fn clear_all_followed_by_process_chunk() {
    let (db, dir) = make_db(4);
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let state = make_state(db, &db_path);

    // Clear the DB
    let clear = dispatch_val(
        1,
        "knowledge_clear_all",
        json!({"confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&clear, 1);

    // Service must still accept new writes after clear
    let ingest = process_chunk(2, "post-clear-chunk", "fresh.txt", Arc::clone(&state)).await;
    assert_ok(&ingest, 2);
    assert_eq!(ingest["result"]["success"], true, "{ingest}");

    let (_, ep_count, _) = status_counts(3, Arc::clone(&state)).await;
    assert_eq!(ep_count, 1, "new episode should appear after clear_all");
}

#[tokio::test]
async fn clear_all_drops_wal_writer() {
    let (db, dir) = make_db(4);
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let wal_dir = dir.path().join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();
    let state = make_state_with_wal(db, wal_dir, &db_path);

    // Confirm the WAL writer is Some before clear_all
    assert!(
        state.wal_writer.lock().unwrap().is_some(),
        "wal_writer should be Some before clear_all"
    );

    let v = dispatch_val(
        1,
        "knowledge_clear_all",
        json!({"confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 1);

    // After clear_all, wal_writer must be None so the next write triggers lazy re-init
    assert!(
        state.wal_writer.lock().unwrap().is_none(),
        "wal_writer must be None after clear_all"
    );
}
