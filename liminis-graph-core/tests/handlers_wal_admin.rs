// Integration tests for the three WAL admin handlers:
// knowledge_prepare_checkpoint, knowledge_rebuild_from_wal, knowledge_rebuild_status

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use std::time::Duration;
use tokio_util::sync::CancellationToken;

use arc_swap::ArcSwapOption;
use liminis_graph_core::{
    app_state::{AppState, OntologyDriftState},
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
    let db = Arc::new(Db::open(dir.path().join("wal_admin_test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }
    (db, dir)
}

fn make_state_with_wal(db: Arc<Db>, wal_dir: std::path::PathBuf) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = WalWriter::new(&wal_dir, 10_000, 0).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: Some(wal_dir),
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(wal_writer)),
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

fn make_state_no_wal(db: Arc<Db>) -> Arc<AppState> {
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

async fn dispatch(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

fn entity_wal_line(seq: u64, uuid: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {{uuid: '{uuid}'}}) ON CREATE SET n.name = '{uuid}', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-22 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{{}}'","params":{{}}}}"#
    )
}

// ── prepare_checkpoint ────────────────────────────────────────────────────────

/// prepare_checkpoint on a state with no WAL dir configured returns success with zeros.
#[tokio::test]
async fn test_prepare_checkpoint_no_wal_dir() {
    let (db, _dir) = make_db(4);
    let state = make_state_no_wal(db);
    let v = dispatch(1, "knowledge_prepare_checkpoint", json!({}), state).await;

    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 1);
    assert!(v.get("result").is_some(), "expected result: {v}");
    assert_eq!(v["result"]["success"], true);
    assert_eq!(v["result"]["files_flushed"], 0);
    assert_eq!(v["result"]["files_total"], 0);
}

/// prepare_checkpoint on an empty WAL dir returns success with zeros.
#[tokio::test]
async fn test_prepare_checkpoint_empty_wal_dir() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());
    let v = dispatch(2, "knowledge_prepare_checkpoint", json!({}), state).await;

    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["files_flushed"], 0, "{v}");
    assert_eq!(v["result"]["files_total"], 0, "{v}");
}

/// Two consecutive prepare_checkpoint calls are idempotent (second returns files_flushed: 0).
#[tokio::test]
async fn test_prepare_checkpoint_idempotent() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();

    // Pre-seed a JSONL file so files_total is non-zero
    std::fs::write(
        wal_dir.path().join("20260522_000000_aaa111_0000.jsonl"),
        entity_wal_line(0, "pre-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    // First call: no open writer, so files_flushed=0 but files_total reflects the pre-seeded file
    let v1 = dispatch(
        3,
        "knowledge_prepare_checkpoint",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(v1["result"]["success"], true, "{v1}");
    assert_eq!(v1["result"]["files_flushed"], 0, "no open writer: {v1}");
    assert_eq!(v1["result"]["files_total"], 1, "{v1}");

    // Second call: still idempotent
    let v2 = dispatch(4, "knowledge_prepare_checkpoint", json!({}), state).await;
    assert_eq!(v2["result"]["success"], true, "{v2}");
    assert_eq!(v2["result"]["files_flushed"], 0, "{v2}");
    assert_eq!(v2["result"]["files_total"], 1, "{v2}");
}

// ── rebuild_from_wal ─────────────────────────────────────────────────────────

/// rebuild_from_wal with no wal_dir configured returns an error.
#[tokio::test]
async fn test_rebuild_from_wal_no_wal_dir() {
    let (db, _dir) = make_db(4);
    let state = make_state_no_wal(db);
    let v = dispatch(10, "knowledge_rebuild_from_wal", json!({}), state).await;
    assert!(v.get("error").is_some(), "expected error: {v}");
    assert_eq!(v["error"]["code"], -32000, "{v}");
}

/// rebuild_from_wal dry_run=true counts mutations without modifying the DB.
#[tokio::test]
async fn test_rebuild_from_wal_dry_run() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();

    // Write 2 entity lines
    let content = [entity_wal_line(0, "dry-a"), entity_wal_line(1, "dry-b")].join("\n") + "\n";
    std::fs::write(
        wal_dir.path().join("20260522_000000_aaa111_0000.jsonl"),
        &content,
    )
    .unwrap();

    let state = make_state_with_wal(db.clone(), wal_dir.path().to_path_buf());

    let count_before = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };

    let v = dispatch(
        11,
        "knowledge_rebuild_from_wal",
        json!({"dry_run": true}),
        state,
    )
    .await;

    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["mutations_replayed"], 2, "{v}");
    assert_eq!(v["result"]["dry_run"], true, "{v}");

    let count_after = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };
    assert_eq!(count_before, count_after, "dry_run must not modify the DB");
}

/// rebuild_from_wal non-streaming non-dry-run returns job_id and status "running".
#[tokio::test]
async fn test_rebuild_from_wal_non_streaming_returns_job_id() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();

    std::fs::write(
        wal_dir.path().join("20260522_000000_bbb222_0000.jsonl"),
        entity_wal_line(0, "job-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());
    let v = dispatch(
        12,
        "knowledge_rebuild_from_wal",
        json!({}),
        Arc::clone(&state),
    )
    .await;

    assert_eq!(v["result"]["success"], true, "{v}");
    let job_id = v["result"]["job_id"].as_str().expect("expected job_id");
    assert!(!job_id.is_empty(), "job_id must be non-empty");
    assert_eq!(v["result"]["status"], "running", "{v}");
}

/// rebuild_from_wal with invalid from_seq (boolean) returns a structured error.
#[tokio::test]
async fn test_rebuild_from_wal_rejects_boolean_from_seq() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();

    std::fs::write(
        wal_dir.path().join("20260522_000000_ccc333_0000.jsonl"),
        entity_wal_line(0, "bool-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());
    let v = dispatch(
        13,
        "knowledge_rebuild_from_wal",
        json!({"from_seq": true}),
        state,
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "expected error for boolean from_seq: {v}"
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("boolean"), "error should mention boolean: {v}");
}

/// rebuild_from_wal with negative from_seq returns a structured error.
#[tokio::test]
async fn test_rebuild_from_wal_rejects_negative_from_seq() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();

    std::fs::write(
        wal_dir.path().join("20260522_000000_ddd444_0000.jsonl"),
        entity_wal_line(0, "neg-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());
    let v = dispatch(
        14,
        "knowledge_rebuild_from_wal",
        json!({"from_seq": -1}),
        state,
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "expected error for negative from_seq: {v}"
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
}

// ── rebuild_status ────────────────────────────────────────────────────────────

/// rebuild_status returns not_found for an unknown job_id.
#[tokio::test]
async fn test_rebuild_status_not_found() {
    let (db, _dir) = make_db(4);
    let state = make_state_no_wal(db);
    let v = dispatch(
        20,
        "knowledge_rebuild_status",
        json!({"job_id": "00000000-0000-0000-0000-000000000000"}),
        state,
    )
    .await;

    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 20);
    assert!(v.get("result").is_some(), "expected result: {v}");
    assert_eq!(v["result"]["status"], "not_found", "{v}");
}

/// rebuild_status requires a non-empty job_id.
#[tokio::test]
async fn test_rebuild_status_rejects_empty_job_id() {
    let (db, _dir) = make_db(4);
    let state = make_state_no_wal(db);
    let v = dispatch(21, "knowledge_rebuild_status", json!({"job_id": ""}), state).await;
    assert!(
        v.get("error").is_some(),
        "expected error for empty job_id: {v}"
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
}

/// A completed background rebuild job is reflected in rebuild_status.
#[tokio::test]
async fn test_rebuild_status_completed_after_background_job() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();

    std::fs::write(
        wal_dir.path().join("20260522_000000_eee555_0000.jsonl"),
        entity_wal_line(0, "status-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    // Start the background rebuild
    let v = dispatch(
        22,
        "knowledge_rebuild_from_wal",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(v["result"]["success"], true, "{v}");
    let job_id = v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    // Poll until completed (up to 5 seconds)
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;

        let status_v = dispatch(
            23,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;

        let status = status_v["result"]["status"].as_str().unwrap_or("?");
        match status {
            "completed" => {
                assert!(
                    status_v["result"]["mutations_replayed"]
                        .as_u64()
                        .unwrap_or(0)
                        >= 1,
                    "expected at least 1 mutation replayed: {status_v}"
                );
                return;
            }
            "failed" => panic!("rebuild job failed: {status_v}"),
            "running" => {
                if std::time::Instant::now() > deadline {
                    panic!("rebuild did not complete within 5s: {status_v}");
                }
            }
            other => panic!("unexpected status: {other}: {status_v}"),
        }
    }
}
