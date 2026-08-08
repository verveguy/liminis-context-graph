// Integration tests for the three checkpoint handlers (issue #363):
// knowledge_checkpoint_create, knowledge_checkpoint_list, knowledge_checkpoint_delete
//
// AppState-builder pattern mirrors handlers_wal_admin.rs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
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
use tokio_util::sync::CancellationToken;

const EMB_DIM: usize = 4;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("checkpoint_test.db").to_str().unwrap()).unwrap());
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
        embedder: Arc::new(MockEmbedder::new(EMB_DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: Some(wal_dir),
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
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
        embedder: Arc::new(MockEmbedder::new(EMB_DIM)),
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

/// Dispatches a real `knowledge_add_episode` mutation (via `MockExtractor`/`MockEmbedder`, no
/// LLM needed) so `applied_seq` advances and a WAL line is actually written — the only way to
/// get a non-null `applied_seq` for `knowledge_checkpoint_create` to capture.
async fn add_episode(id: i64, name: &str, group_id: &str, state: Arc<AppState>) -> Value {
    dispatch(
        id,
        "knowledge_add_episode",
        json!({
            "name": name,
            "episode_body": format!("{name} body text"),
            "source": "test",
            "source_description": "test/handlers_checkpoint",
            "reference_time": "2026-01-01 00:00:00",
            "group_id": group_id,
        }),
        state,
    )
    .await
}

fn remove_jsonl_files(dir: &std::path::Path) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
            std::fs::remove_file(e.path()).unwrap();
        }
    }
}

// ── knowledge_checkpoint_create ─────────────────────────────────────────────

#[tokio::test]
async fn create_no_wal_dir_configured_errors() {
    let (db, _dir) = make_db(EMB_DIM);
    let state = make_state_no_wal(db);
    let v = dispatch(
        1,
        "knowledge_checkpoint_create",
        json!({"name": "x"}),
        state,
    )
    .await;
    assert!(v.get("error").is_some(), "expected error: {v}");
}

/// FR-008: applied_seq is null until at least one mutation has been applied.
#[tokio::test]
async fn create_fails_when_applied_seq_is_null() {
    let (db, _db_dir) = make_db(EMB_DIM);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    let v = dispatch(
        1,
        "knowledge_checkpoint_create",
        json!({"name": "pre-migration"}),
        state,
    )
    .await;
    assert!(v.get("error").is_some(), "expected error: {v}");
}

/// FR-001, User Story 1 Acceptance Scenario 1: response echoes name/seq/created_at/note.
#[tokio::test]
async fn create_happy_path_echoes_name_seq_and_note() {
    let (db, _db_dir) = make_db(EMB_DIM);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    let write_resp = add_episode(1, "ep1", "g", Arc::clone(&state)).await;
    assert!(
        write_resp.get("result").is_some(),
        "seed mutation must succeed: {write_resp}"
    );

    let v = dispatch(
        2,
        "knowledge_checkpoint_create",
        json!({"name": "pre-migration", "note": "before the change"}),
        Arc::clone(&state),
    )
    .await;
    assert!(v.get("result").is_some(), "expected result: {v}");
    assert_eq!(v["result"]["name"], "pre-migration", "{v}");
    assert_eq!(v["result"]["note"], "before the change", "{v}");
    assert!(v["result"]["seq"].as_u64().is_some(), "{v}");
    assert!(v["result"]["created_at"].as_str().is_some(), "{v}");
}

/// FR-002.
#[tokio::test]
async fn create_rejects_duplicate_name() {
    let (db, _db_dir) = make_db(EMB_DIM);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    add_episode(1, "ep1", "g", Arc::clone(&state)).await;

    let v1 = dispatch(
        2,
        "knowledge_checkpoint_create",
        json!({"name": "dup"}),
        Arc::clone(&state),
    )
    .await;
    assert!(v1.get("result").is_some(), "{v1}");

    let v2 = dispatch(
        3,
        "knowledge_checkpoint_create",
        json!({"name": "dup"}),
        Arc::clone(&state),
    )
    .await;
    assert!(
        v2.get("error").is_some(),
        "expected error on duplicate name: {v2}"
    );

    // FR-004: the original checkpoint must be unaffected by the failed duplicate attempt.
    let list = dispatch(4, "knowledge_checkpoint_list", json!({}), state).await;
    assert_eq!(list["result"]["count"], 1, "{list}");
}

// ── knowledge_checkpoint_list / knowledge_checkpoint_delete ────────────────

#[tokio::test]
async fn list_no_wal_dir_configured_errors() {
    let (db, _dir) = make_db(EMB_DIM);
    let state = make_state_no_wal(db);
    let v = dispatch(1, "knowledge_checkpoint_list", json!({}), state).await;
    assert!(v.get("error").is_some(), "expected error: {v}");
}

#[tokio::test]
async fn delete_no_wal_dir_configured_errors() {
    let (db, _dir) = make_db(EMB_DIM);
    let state = make_state_no_wal(db);
    let v = dispatch(
        1,
        "knowledge_checkpoint_delete",
        json!({"name": "x"}),
        state,
    )
    .await;
    assert!(v.get("error").is_some(), "expected error: {v}");
}

/// Edge case: an empty checkpoint set returns an empty list, not an error.
#[tokio::test]
async fn list_on_empty_set_returns_empty_not_error() {
    let (db, _db_dir) = make_db(EMB_DIM);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    let v = dispatch(1, "knowledge_checkpoint_list", json!({}), state).await;
    assert!(v.get("result").is_some(), "{v}");
    assert_eq!(v["result"]["count"], 0, "{v}");
    assert_eq!(
        v["result"]["checkpoints"].as_array().unwrap().len(),
        0,
        "{v}"
    );
}

/// FR-006.
#[tokio::test]
async fn delete_missing_name_errors() {
    let (db, _db_dir) = make_db(EMB_DIM);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    let v = dispatch(
        1,
        "knowledge_checkpoint_delete",
        json!({"name": "nope"}),
        state,
    )
    .await;
    assert!(v.get("error").is_some(), "expected error: {v}");
}

/// User Story 3 / SC-003: create several, delete one, others unaffected.
#[tokio::test]
async fn create_list_delete_round_trip_and_others_unaffected() {
    let (db, _db_dir) = make_db(EMB_DIM);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    add_episode(1, "ep1", "g", Arc::clone(&state)).await;
    dispatch(
        2,
        "knowledge_checkpoint_create",
        json!({"name": "a"}),
        Arc::clone(&state),
    )
    .await;
    add_episode(3, "ep2", "g", Arc::clone(&state)).await;
    dispatch(
        4,
        "knowledge_checkpoint_create",
        json!({"name": "b"}),
        Arc::clone(&state),
    )
    .await;
    add_episode(5, "ep3", "g", Arc::clone(&state)).await;
    dispatch(
        6,
        "knowledge_checkpoint_create",
        json!({"name": "c"}),
        Arc::clone(&state),
    )
    .await;

    let list_before = dispatch(
        7,
        "knowledge_checkpoint_list",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(list_before["result"]["count"], 3, "{list_before}");

    let del = dispatch(
        8,
        "knowledge_checkpoint_delete",
        json!({"name": "b"}),
        Arc::clone(&state),
    )
    .await;
    assert!(del.get("result").is_some(), "{del}");

    let list_after = dispatch(9, "knowledge_checkpoint_list", json!({}), state).await;
    assert_eq!(list_after["result"]["count"], 2, "{list_after}");
    let names: Vec<&str> = list_after["result"]["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a", "c"], "{list_after}");
}

/// FR-003/SC-004: checkpoints persist across a simulated process restart (fresh AppState
/// pointed at the same wal_dir).
#[tokio::test]
async fn checkpoints_survive_a_simulated_restart() {
    let (db1, _db_dir1) = make_db(EMB_DIM);
    let wal_dir = TempDir::new().unwrap();
    let state1 = make_state_with_wal(db1, wal_dir.path().to_path_buf());

    add_episode(1, "ep1", "g", Arc::clone(&state1)).await;
    let create_resp = dispatch(
        2,
        "knowledge_checkpoint_create",
        json!({"name": "pre-migration", "note": "n"}),
        Arc::clone(&state1),
    )
    .await;
    assert!(create_resp.get("result").is_some(), "{create_resp}");
    let seq = create_resp["result"]["seq"].as_u64().unwrap();
    drop(state1);

    // Fresh AppState (simulating a process restart) pointed at the same wal_dir.
    let (db2, _db_dir2) = make_db(EMB_DIM);
    let state2 = make_state_with_wal(db2, wal_dir.path().to_path_buf());

    let list_resp = dispatch(3, "knowledge_checkpoint_list", json!({}), state2).await;
    assert_eq!(list_resp["result"]["count"], 1, "{list_resp}");
    assert_eq!(
        list_resp["result"]["checkpoints"][0]["name"], "pre-migration",
        "{list_resp}"
    );
    assert_eq!(
        list_resp["result"]["checkpoints"][0]["seq"], seq,
        "{list_resp}"
    );
    assert_eq!(
        list_resp["result"]["checkpoints"][0]["note"], "n",
        "{list_resp}"
    );
}

/// FR-007/SC-005: a checkpoint whose seq falls outside the WAL content currently available for
/// replay is reported as unreachable, not identical to a healthy checkpoint.
#[tokio::test]
async fn list_marks_checkpoint_unreachable_when_wal_no_longer_covers_it() {
    let (db, _db_dir) = make_db(EMB_DIM);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    add_episode(1, "ep1", "g", Arc::clone(&state)).await;
    let create_resp = dispatch(
        2,
        "knowledge_checkpoint_create",
        json!({"name": "orphan"}),
        Arc::clone(&state),
    )
    .await;
    assert!(create_resp.get("result").is_some(), "{create_resp}");

    // Simulate the WAL directory's content being replaced (e.g. a partial/manual WAL swap)
    // so it no longer covers the checkpoint's seq.
    remove_jsonl_files(wal_dir.path());
    std::fs::write(
        wal_dir.path().join("replacement_0000.jsonl"),
        r#"{"seq":999,"ts":"2026-08-08T00:00:00.000000+00:00","db":"default","cypher":"MERGE (n:Entity {uuid: $uuid})","params":{"uuid":"z"}}"#
            .to_string()
            + "\n",
    )
    .unwrap();

    let list_resp = dispatch(3, "knowledge_checkpoint_list", json!({}), state).await;
    assert_eq!(list_resp["result"]["count"], 1, "{list_resp}");
    assert_eq!(
        list_resp["result"]["checkpoints"][0]["reachable"], false,
        "{list_resp}"
    );
}

// ── FR-009 / SC-006: end-to-end recovery workflow ───────────────────────────

/// User Story 2: create a checkpoint, apply a subsequent "bad" mutation, then bound a rebuild
/// to the checkpoint's seq and verify the bad mutation's effects are gone and applied_seq
/// equals the checkpoint's seq.
#[tokio::test]
async fn recovery_via_checkpoint_and_bounded_rebuild() {
    let (db, _db_dir) = make_db(EMB_DIM);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db.clone(), wal_dir.path().to_path_buf());

    // Good mutation, then a checkpoint labeling this known-good position.
    let good = add_episode(1, "good-episode", "g", Arc::clone(&state)).await;
    assert!(good.get("result").is_some(), "{good}");

    let checkpoint_resp = dispatch(
        2,
        "knowledge_checkpoint_create",
        json!({"name": "pre-bad-mutation"}),
        Arc::clone(&state),
    )
    .await;
    assert!(checkpoint_resp.get("result").is_some(), "{checkpoint_resp}");
    let checkpoint_seq = checkpoint_resp["result"]["seq"].as_u64().unwrap();

    // Bad mutation after the checkpoint.
    let bad = add_episode(3, "bad-episode", "g", Arc::clone(&state)).await;
    assert!(bad.get("result").is_some(), "{bad}");

    // Sanity: both episodes are present before recovery.
    {
        let conn = db.connect().unwrap();
        assert_eq!(
            conn.count_nodes("Episodic").unwrap(),
            2,
            "expected both episodes present before recovery"
        );
    }
    drop(db);

    // Recover: bound the rebuild to the checkpoint's seq (#362's to_seq). Use the streaming
    // path (progress_tx: Some) so the replay runs synchronously within this call instead of
    // as a background job.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let rebuild_req = req(
        4,
        "knowledge_rebuild_from_wal",
        json!({"from_seq": 0, "to_seq": checkpoint_seq, "force_clear": true}),
    );
    let rebuild_resp = handlers::dispatch(rebuild_req, Arc::clone(&state), Some(tx)).await;
    while rx.try_recv().is_ok() {}
    let rebuild_v = serde_json::to_value(rebuild_resp).unwrap();
    assert_eq!(rebuild_v["result"]["success"], true, "{rebuild_v}");

    // force_clear replaces state.db with a fresh handle at the same path (see
    // clear_db_for_rebuild) — the original `db` Arc from before the rebuild is now stale.
    let db_after = state
        .db
        .load_full()
        .expect("db must be present after rebuild");

    // The bad episode's effects must be gone; only the good one survives.
    {
        let conn = db_after.connect().unwrap();
        assert_eq!(
            conn.count_nodes("Episodic").unwrap(),
            1,
            "bad mutation's effects must be excluded after bounded rebuild"
        );
        assert_eq!(
            conn.get_applied_seq().unwrap(),
            Some(checkpoint_seq),
            "applied_seq must equal the checkpoint's seq after recovery"
        );
    }
}
