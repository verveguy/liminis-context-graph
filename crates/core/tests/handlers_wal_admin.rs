// Integration tests for the three WAL admin handlers:
// knowledge_prepare_checkpoint, knowledge_rebuild_from_wal, knowledge_rebuild_status

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;
use tokio_util::sync::CancellationToken;

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    schema,
    telemetry::{NoopSink, TelemetrySink},
    EntityRow, WalWriter,
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

/// A standalone `RelatesToNode_` WAL line (param-bound, mirroring the
/// `timestamps_in_params.jsonl` fixture shape). Search queries these nodes directly by
/// property (no two-hop `RELATES_TO` connectivity required), so this is sufficient to
/// exercise `knowledge_find_relationships`.
fn relates_to_wal_line(seq: u64, uuid: &str, name: &str, fact: &str) -> String {
    let line = json!({
        "seq": seq,
        "ts": "2026-05-22T00:00:00.000000+00:00",
        "db": "",
        "cypher": "MERGE (r:RelatesToNode_ {uuid: $uuid}) ON CREATE SET r.name = $name, \
             r.group_id = $group_id, r.created_at = $created_at, r.fact = $fact, \
             r.fact_embedding = $fact_embedding, r.valid_at = $valid_at, \
             r.invalid_at = $invalid_at, r.attributes = $attributes, \
             r.relation_type = $relation_type",
        "params": {
            "uuid": uuid,
            "name": name,
            "group_id": "g",
            "created_at": "2026-05-22T00:00:00.000000+00:00",
            "fact": fact,
            "fact_embedding": [1.0, 0.0, 0.0, 0.0],
            "valid_at": "2026-05-22T00:00:00.000000+00:00",
            "invalid_at": null,
            "attributes": "{}",
            "relation_type": "KNOWS",
        },
    });
    line.to_string()
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

// ── new stat-field assertions (FR-002, Task 9) ───────────────────────────────

fn assert_has_stat_fields(v: &Value, label: &str) {
    for field in &[
        "unrecognised_lines",
        "failed_lines",
        "unparseable_lines",
        "legacy_skipped_lines",
        "lines_skipped",
        "failed_samples",
        "fidelity_warning",
    ] {
        assert!(
            v["result"].get(field).is_some(),
            "{label}: result missing '{field}': {v}"
        );
    }
    assert!(
        v["result"]["failed_samples"].is_array(),
        "{label}: failed_samples must be an array: {v}"
    );
}

/// dry_run response includes all four granular stat fields plus failed_samples.
#[tokio::test]
async fn test_rebuild_from_wal_dry_run_has_stat_fields() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::write(
        wal_dir.path().join("20260522_000000_stat_dryrun.jsonl"),
        entity_wal_line(0, "stat-dry-a") + "\n" + &entity_wal_line(1, "stat-dry-b") + "\n",
    )
    .unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());
    let v = dispatch(
        30,
        "knowledge_rebuild_from_wal",
        json!({"dry_run": true}),
        state,
    )
    .await;
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_has_stat_fields(&v, "dry_run");
    assert_eq!(v["result"]["mutations_replayed"], 2, "{v}");
    assert_eq!(v["result"]["lines_skipped"], 0, "{v}");
    assert_eq!(v["result"]["failed_samples"], json!([]), "{v}");
}

/// streaming path response includes all four granular stat fields plus failed_samples.
#[tokio::test]
async fn test_rebuild_from_wal_streaming_has_stat_fields() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::write(
        wal_dir.path().join("20260522_000000_stat_stream.jsonl"),
        entity_wal_line(0, "stat-stream-a") + "\n" + &entity_wal_line(1, "stat-stream-b") + "\n",
    )
    .unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::Value::Number(31.into()),
        method: "knowledge_rebuild_from_wal".to_string(),
        params: json!({}),
    };
    let resp = handlers::dispatch(req, Arc::clone(&state), Some(tx)).await;
    // Drain any progress events
    while rx.try_recv().is_ok() {}
    let v = serde_json::to_value(resp).unwrap();

    assert_eq!(v["result"]["success"], true, "{v}");
    assert_has_stat_fields(&v, "streaming");
    assert_eq!(v["result"]["mutations_replayed"], 2, "{v}");
    assert_eq!(v["result"]["lines_skipped"], 0, "{v}");
    assert_eq!(v["result"]["failed_samples"], json!([]), "{v}");
}

/// background job result stored in rebuild_status also includes all granular stat fields.
#[tokio::test]
async fn test_rebuild_status_result_has_stat_fields() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::write(
        wal_dir.path().join("20260522_000000_stat_bg.jsonl"),
        entity_wal_line(0, "stat-bg-a") + "\n",
    )
    .unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    // Start the background rebuild
    let v = dispatch(
        32,
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

    // Poll until completed (up to 5 seconds), then check the stored result JSON
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch(
            33,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        let status = status_v["result"]["status"].as_str().unwrap_or("?");
        match status {
            "completed" => {
                // The per-replay stats are stored in the nested result.result object
                let inner = &status_v["result"]["result"];
                for field in &[
                    "unrecognised_lines",
                    "failed_lines",
                    "unparseable_lines",
                    "legacy_skipped_lines",
                    "lines_skipped",
                    "failed_samples",
                    "fidelity_warning",
                ] {
                    assert!(
                        inner.get(field).is_some(),
                        "bg-job-result: result.result missing '{field}': {status_v}"
                    );
                }
                assert!(
                    inner["failed_samples"].is_array(),
                    "failed_samples must be array: {status_v}"
                );
                assert_eq!(inner["failed_samples"], json!([]), "{status_v}");
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

/// IPC response includes a non-null fidelity_warning when failed_lines / total > 10% (FR-004, SC-004).
#[tokio::test]
async fn test_rebuild_from_wal_fidelity_warning_surfaced() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();

    // 11 lines referencing a non-existent table → each fails and increments failed_lines.
    // 1 valid Entity MERGE → lines_replayed. Ratio = 11/12 = 91.7% > 10%.
    let fail_line = |seq: u64| -> String {
        format!(
            r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"CREATE (:NonExistentFidelityTable {{uuid: 'f-{seq}'}})","params":{{}}}}"#
        )
    };
    let ok_line = entity_wal_line(11, "fidelity-warn-entity");
    let content: String = (0..11u64)
        .map(fail_line)
        .chain(std::iter::once(ok_line))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(
        wal_dir.path().join("20260522_000000_fidelity_warn.jsonl"),
        &content,
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::Value::Number(34.into()),
        method: "knowledge_rebuild_from_wal".to_string(),
        params: json!({}),
    };
    let resp = handlers::dispatch(req, Arc::clone(&state), Some(tx)).await;
    while rx.try_recv().is_ok() {} // drain progress events
    let v = serde_json::to_value(resp).unwrap();

    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["mutations_replayed"], 1, "{v}");
    assert_eq!(v["result"]["failed_lines"], 11, "{v}");
    assert!(
        !v["result"]["fidelity_warning"].is_null(),
        "fidelity_warning must be a non-null string when >10% of mutations fail: {v}"
    );
    let warning = v["result"]["fidelity_warning"].as_str().unwrap_or("");
    assert!(
        !warning.is_empty(),
        "fidelity_warning must be non-empty: {v}"
    );
}

/// Streaming IPC progress events include files_total, failed_lines_so_far, and
/// legacy_skipped_lines_so_far as numeric fields (FR-003, SC-002).
#[tokio::test]
async fn test_rebuild_from_wal_streaming_progress_has_new_fields() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::write(
        wal_dir.path().join("20260522_000000_progress_fields.jsonl"),
        entity_wal_line(0, "progress-field-a")
            + "\n"
            + &entity_wal_line(1, "progress-field-b")
            + "\n",
    )
    .unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("20260522_000001_progress_fields2.jsonl"),
        entity_wal_line(2, "progress-field-c") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();

    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::Value::Number(40.into()),
        method: "knowledge_rebuild_from_wal".to_string(),
        params: json!({}),
    };
    let _resp = handlers::dispatch(req, Arc::clone(&state), Some(tx)).await;

    // Collect all progress events
    let mut events: Vec<Value> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    assert!(
        !events.is_empty(),
        "at least one progress event must be emitted for 2 WAL files"
    );

    for ev in &events {
        assert!(
            ev["files_total"].is_number(),
            "progress event must include numeric 'files_total': {ev}"
        );
        assert_eq!(
            ev["files_total"].as_u64().unwrap_or(0),
            2,
            "files_total must equal the number of WAL files: {ev}"
        );
        assert!(
            ev["failed_lines_so_far"].is_number(),
            "progress event must include numeric 'failed_lines_so_far': {ev}"
        );
        assert!(
            ev["legacy_skipped_lines_so_far"].is_number(),
            "progress event must include numeric 'legacy_skipped_lines_so_far': {ev}"
        );
    }
}

// ── index lifecycle tests (FR-002, FR-003, FR-004, FR-005) ───────────────────

/// After a streaming (non-dry-run) WAL reload, all FTS and vector indexes exist and
/// knowledge_find_entities succeeds without triggering an on-demand index build.
#[tokio::test]
async fn test_reload_builds_all_indexes() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();

    // Write 3 entity mutations and 1 relationship mutation to the WAL.
    let content = [
        entity_wal_line(0, "reload-idx-a"),
        entity_wal_line(1, "reload-idx-b"),
        entity_wal_line(2, "reload-idx-c"),
        relates_to_wal_line(3, "reload-idx-rel", "ReloadRelation", "reload fact payload"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        wal_dir.path().join("20260617_000000_reload_idx.jsonl"),
        &content,
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    // Run the reload via the streaming path (progress_tx makes is_streaming=true).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(50),
        method: "knowledge_rebuild_from_wal".to_string(),
        params: json!({}),
    };
    let resp = handlers::dispatch(req, Arc::clone(&state), Some(tx)).await;
    while rx.try_recv().is_ok() {}
    let v = serde_json::to_value(resp).unwrap();

    assert_eq!(v["result"]["success"], true, "reload must succeed: {v}");
    assert_eq!(v["result"]["mutations_replayed"], 4, "{v}");

    // FR-004: the rebuild result must explicitly report indices as built.
    assert_eq!(
        v["result"]["indices_built"], true,
        "rebuild result must report indices_built: true: {v}"
    );

    // FR-002/FR-005: indices_built flag must be set after a successful non-dry-run reload.
    assert!(
        state.indices_built.load(Ordering::Acquire),
        "indices_built must be true after reload"
    );

    // FR-001/A2: knowledge_find_entities must return the actual replayed entity, not just
    // succeed without error — an empty result set must not be mistaken for success.
    let find_req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(51),
        method: "knowledge_find_entities".to_string(),
        params: json!({"query": "reload", "group_ids": ["g"], "num_results": 5}),
    };
    let find_resp = handlers::dispatch(find_req, Arc::clone(&state), None).await;
    let fv = serde_json::to_value(find_resp).unwrap();

    assert!(fv.get("error").is_none(), "no error after reload: {fv}");
    let nodes = fv["result"]["nodes"]
        .as_array()
        .expect("nodes must be an array");
    assert!(
        !nodes.is_empty(),
        "knowledge_find_entities must return non-empty results after reload: {fv}"
    );
    assert!(
        nodes.iter().any(|n| n["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("reload-idx"))),
        "expected a replayed entity in results: {fv}"
    );

    // FR-002/A2: knowledge_find_relationships must likewise return the actual replayed fact.
    let find_rel_req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(52),
        method: "knowledge_find_relationships".to_string(),
        params: json!({"query": "reload fact payload", "group_ids": ["g"], "num_results": 5}),
    };
    let find_rel_resp = handlers::dispatch(find_rel_req, Arc::clone(&state), None).await;
    let rv = serde_json::to_value(find_rel_resp).unwrap();

    assert!(rv.get("error").is_none(), "no error after reload: {rv}");
    let facts = rv["result"]["facts"]
        .as_array()
        .expect("facts must be an array");
    assert!(
        !facts.is_empty(),
        "knowledge_find_relationships must return non-empty results after reload: {rv}"
    );
    assert!(
        facts
            .iter()
            .any(|f| f["uuid"].as_str() == Some("reload-idx-rel")),
        "expected the replayed relationship in results: {rv}"
    );

    // Flag must still be true (search did not reset it).
    assert!(
        state.indices_built.load(Ordering::Acquire),
        "indices_built must remain true after a post-reload search"
    );
}

/// An interrupted reload (drop completed, build not yet run) self-heals on the next search.
/// Simulates the crash scenario: FTS dropped, indices_built=false, data present.
#[tokio::test]
async fn test_interrupted_reload_auto_heals() {
    let (db, _db_dir) = make_db(4);

    // Insert entity data directly so the search has something to work with.
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "interrupted-heal-1".to_string(),
            name: "InterruptedHealEntity".to_string(),
            group_id: "g".to_string(),
            labels: vec![],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0f32; 4],
            summary: "auto-heal after interrupted reload".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        // Drop FTS indexes to simulate a mid-reload interrupt (drop ran, build did not).
        schema::drop_fts_indexes(&conn);
    }

    // State has indices_built=false (default from make_state_no_wal).
    let state = make_state_no_wal(db);
    assert!(
        !state.indices_built.load(Ordering::Acquire),
        "indices_built must start false"
    );

    // FR-005: knowledge_find_entities must auto-heal by rebuilding both FTS and vector indexes.
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(52),
        method: "knowledge_find_entities".to_string(),
        params: json!({"query": "InterruptedHealEntity", "group_ids": ["g"], "num_results": 5}),
    };
    let resp = handlers::dispatch(req, Arc::clone(&state), None).await;
    let v = serde_json::to_value(resp).unwrap();

    assert!(
        v.get("result").is_some(),
        "knowledge_find_entities must succeed after auto-heal of interrupted reload: {v}"
    );
    assert!(
        v.get("error").is_none(),
        "no error expected after auto-heal: {v}"
    );

    // Auto-heal sets the flag so subsequent searches skip it.
    assert!(
        state.indices_built.load(Ordering::Acquire),
        "indices_built must be true after auto-heal"
    );
}

/// SC-001/SC-002/FR-003 (job-path coverage): a production-representative rebuild — multiple WAL
/// files, hundreds of mutations spanning entities and relationships — run via the background-job
/// path (`knowledge_rebuild_from_wal` → poll `knowledge_rebuild_status` to `Completed`) leaves
/// `knowledge_find_entities`/`knowledge_find_relationships` immediately queryable with zero
/// intervening `knowledge_build_indices` calls. Regression coverage for issue #192: at toy scale
/// (3 mutations) the defect never reproduced because `create_vector_indexes`/`create_fts_indexes`
/// blanket-suppressed every error, but the structural bug applies at any scale — this exercises
/// the fix against a fixture credible for the production report (113 files / 5,565 events).
#[tokio::test]
async fn test_production_scale_rebuild_leaves_search_immediately_queryable() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();

    // Spread ~360 mutations (270 entities + 90 relationships) across 3 WAL files.
    let mut seq = 0u64;
    for file_idx in 0..3 {
        let mut lines = Vec::new();
        for i in 0..90 {
            let n = file_idx * 90 + i;
            lines.push(entity_wal_line(seq, &format!("scale-entity-{n}")));
            seq += 1;
        }
        for i in 0..30 {
            let n = file_idx * 30 + i;
            lines.push(relates_to_wal_line(
                seq,
                &format!("scale-rel-{n}"),
                &format!("ScaleRelation{n}"),
                &format!("scale fact payload {n}"),
            ));
            seq += 1;
        }
        let content = lines.join("\n") + "\n";
        std::fs::write(
            wal_dir
                .path()
                .join(format!("2026061{file_idx}_000000_scale.jsonl")),
            &content,
        )
        .unwrap();
    }

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    // Non-streaming, non-dry-run always routes through the background-job path.
    let v = dispatch(
        60,
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

    // Poll until completed (up to 30 seconds — 360 mutations at production scale is I/O-bound).
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status_v = loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch(
            61,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        match status_v["result"]["status"].as_str().unwrap_or("?") {
            "completed" => break status_v,
            "failed" => panic!("rebuild job failed: {status_v}"),
            "running" => {
                if std::time::Instant::now() > deadline {
                    panic!("rebuild did not complete within 30s: {status_v}");
                }
            }
            other => panic!("unexpected status: {other}: {status_v}"),
        }
    };

    assert_eq!(
        status_v["result"]["mutations_replayed"], 360,
        "{status_v}"
    );
    assert_eq!(
        status_v["result"]["result"]["indices_built"], true,
        "job result must report indices_built: true: {status_v}"
    );

    // Zero intervening knowledge_build_indices calls — go straight to search.
    let find_req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(62),
        method: "knowledge_find_entities".to_string(),
        // num_results=100 (the handler's max clamp) widens the RRF candidate pool
        // (candidate_limit = num_results * 3) to cover all 270 planted entities — the mock
        // embedder returns a constant zero vector for every text, so the vector-search half of
        // the fusion carries no discriminating signal and a small candidate pool can miss the
        // target; this isn't a ranking-quality test, just a findability one (SC-001).
        params: json!({"query": "scale-entity-42", "group_ids": ["g"], "num_results": 100}),
    };
    let find_resp = handlers::dispatch(find_req, Arc::clone(&state), None).await;
    let fv = serde_json::to_value(find_resp).unwrap();
    assert!(fv.get("error").is_none(), "no error: {fv}");
    let nodes = fv["result"]["nodes"].as_array().expect("nodes array");
    assert!(
        !nodes.is_empty(),
        "knowledge_find_entities must return results at production scale: {fv}"
    );
    assert!(
        nodes
            .iter()
            .any(|n| n["uuid"].as_str() == Some("scale-entity-42")),
        "expected scale-entity-42 in results: {fv}"
    );

    let find_rel_req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(63),
        method: "knowledge_find_relationships".to_string(),
        params: json!({"query": "scale fact payload 7", "group_ids": ["g"], "num_results": 90}),
    };
    let find_rel_resp = handlers::dispatch(find_rel_req, Arc::clone(&state), None).await;
    let rv = serde_json::to_value(find_rel_resp).unwrap();
    assert!(rv.get("error").is_none(), "no error: {rv}");
    let facts = rv["result"]["facts"].as_array().expect("facts array");
    assert!(
        !facts.is_empty(),
        "knowledge_find_relationships must return results at production scale: {rv}"
    );
    assert!(
        facts.iter().any(|f| f["uuid"].as_str() == Some("scale-rel-7")),
        "expected scale-rel-7 in results: {rv}"
    );
}
