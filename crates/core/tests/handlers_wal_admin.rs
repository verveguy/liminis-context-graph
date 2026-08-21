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
    let wal_writer = WalWriter::new(wal_dir.join("liminis"), 10_000, 0).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_root: Some(wal_dir),
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writers: Arc::new(Mutex::new(
            wal_writer
                .into_iter()
                .map(|w| ("liminis".to_string(), w))
                .collect(),
        )),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
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
        wal_root: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writers: Arc::new(Mutex::new(HashMap::new())),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
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

/// A param-bound `Entity` WAL line — mirrors the real production WAL-content shape (see
/// `db.rs`'s `insert_entity`, which always binds `group_id` via `$group_id`, never a Cypher
/// string literal). Unlike `entity_wal_line`'s literal-Cypher/empty-`params` simplification,
/// this is the shape `wal::scan_wal_content_group_ids` actually needs to discover the embedded
/// `group_id` (issue #432).
fn entity_wal_line_with_group(seq: u64, uuid: &str, group_id: &str) -> String {
    let line = json!({
        "seq": seq,
        "ts": "2026-05-22T00:00:00.000000+00:00",
        "db": "",
        "cypher": "MERGE (n:Entity {uuid: $uuid}) ON CREATE SET n.name = $name, \
             n.group_id = $group_id, n.labels = $labels, n.created_at = $created_at, \
             n.name_embedding = $name_embedding, n.summary = $summary, \
             n.attributes = $attributes",
        "params": {
            "uuid": uuid,
            "name": uuid,
            "group_id": group_id,
            "labels": ["Entity", "t"],
            "created_at": "2026-05-22T00:00:00.000000+00:00",
            "name_embedding": [1.0, 0.0, 0.0, 0.0],
            "summary": "s",
            "attributes": "{}",
        },
    });
    line.to_string()
}

/// A bare `CREATE` `Entity` WAL line — mirrors `db.rs`'s `insert_entity` byte-for-byte (a
/// literal `CREATE`, never `MERGE`). Unlike `entity_wal_line_with_group`'s `MERGE ... ON CREATE
/// SET` shape (which contains a `SET` token and would misclassify as an FR-004 "unsafe
/// mutation" line for issue #462's split-group detection), this is what
/// `wal::scan_wal_content_by_group`'s `is_bare_create`/`create_uuids` capture actually needs to
/// exercise the split-group *row-scoped clear* path rather than the refusal path.
fn bare_create_entity_wal_line(seq: u64, uuid: &str, group_id: &str) -> String {
    let line = json!({
        "seq": seq,
        "ts": "2026-05-22T00:00:00.000000+00:00",
        "db": "",
        "cypher": "CREATE (:Entity {uuid: $uuid, name: $name, group_id: $group_id, \
             labels: $labels, created_at: $created_at, name_embedding: $name_embedding, \
             summary: $summary, attributes: $attributes})",
        "params": {
            "uuid": uuid,
            "name": uuid,
            "group_id": group_id,
            "labels": ["Entity", "t"],
            "created_at": "2026-05-22T00:00:00.000000+00:00",
            "name_embedding": [1.0, 0.0, 0.0, 0.0],
            "summary": "s",
            "attributes": "{}",
        },
    });
    line.to_string()
}

/// A `MATCH ... SET` line referencing `group_id` (issue #462's FR-004 refusal trigger) — a
/// mutating non-`CREATE` line that implies a replay can't safely tell whether it's safe to
/// row-scope-clear a split group or must clear the whole thing.
fn unsafe_set_wal_line_with_group(seq: u64, uuid: &str, group_id: &str) -> String {
    let line = json!({
        "seq": seq,
        "ts": "2026-05-22T00:00:00.000000+00:00",
        "db": "",
        "cypher": "MATCH (n:Entity {uuid: $uuid}) SET n.summary = $summary",
        "params": {
            "uuid": uuid,
            "summary": "edited",
            "group_id": group_id,
        },
    });
    line.to_string()
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    // Pre-seed a JSONL file so files_total is non-zero
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_aaa111_0000.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    // Write 2 entity lines
    let content = [entity_wal_line(0, "dry-a"), entity_wal_line(1, "dry-b")].join("\n") + "\n";
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_aaa111_0000.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_bbb222_0000.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_ccc333_0000.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_ddd444_0000.jsonl"),
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

/// rebuild_from_wal with invalid to_seq (boolean) returns a structured error.
#[tokio::test]
async fn test_rebuild_from_wal_rejects_boolean_to_seq() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_eee666_0000.jsonl"),
        entity_wal_line(0, "bool-to-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());
    let v = dispatch(
        15,
        "knowledge_rebuild_from_wal",
        json!({"to_seq": true}),
        state,
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "expected error for boolean to_seq: {v}"
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("boolean"), "error should mention boolean: {v}");
}

/// rebuild_from_wal with negative to_seq returns a structured error.
#[tokio::test]
async fn test_rebuild_from_wal_rejects_negative_to_seq() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_fff777_0000.jsonl"),
        entity_wal_line(0, "neg-to-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());
    let v = dispatch(
        16,
        "knowledge_rebuild_from_wal",
        json!({"to_seq": -1}),
        state,
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "expected error for negative to_seq: {v}"
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
}

/// rebuild_from_wal with to_seq < from_seq is rejected before any WAL line is read or the
/// database is touched — even when force_clear: true is also set (FR-003).
#[tokio::test]
async fn test_rebuild_from_wal_rejects_to_seq_less_than_from_seq() {
    let (db, _db_dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher("CREATE (:Entity {uuid: 'pre-existing-362'})")
            .unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_ggg888_0000.jsonl"),
        entity_wal_line(0, "order-entity") + "\n" + &entity_wal_line(1, "order-entity-2") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal(db.clone(), wal_dir.path().to_path_buf());
    let v = dispatch(
        17,
        "knowledge_rebuild_from_wal",
        json!({"from_seq": 5, "to_seq": 4, "force_clear": true}),
        state,
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "expected error for to_seq < from_seq: {v}"
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("to_seq") && msg.contains("from_seq"),
        "error should mention both to_seq and from_seq: {v}"
    );

    // The database must be left completely untouched by the rejected request, even though
    // force_clear: true was set.
    let count = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };
    assert_eq!(
        count, 1,
        "pre-existing entity must survive a rejected to_seq < from_seq request"
    );
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_eee555_0000.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_stat_dryrun.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_stat_stream.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_stat_bg.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

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
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_fidelity_warn.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260522_000000_progress_fields.jsonl"),
        entity_wal_line(0, "progress-field-a")
            + "\n"
            + &entity_wal_line(1, "progress-field-b")
            + "\n",
    )
    .unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

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
        wal_dir
            .path()
            .join("liminis")
            .join("20260617_000000_reload_idx.jsonl"),
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
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

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
                .join("liminis")
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

    assert_eq!(status_v["result"]["mutations_replayed"], 360, "{status_v}");
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
        facts
            .iter()
            .any(|f| f["uuid"].as_str() == Some("scale-rel-7")),
        "expected scale-rel-7 in results: {rv}"
    );
}

/// SC-003/FR-004/FR-005/FR-006: when the post-replay index build genuinely fails (not merely
/// "already exists"), the rebuild result and a subsequent `knowledge_status` call must both
/// report `indices_built: false`, distinguishable from the all-succeeded case — while the
/// replay itself (which never touched the affected column) still reports `success: true`
/// (Assumption A3: a successful replay is not retroactively reported as failed just because
/// index build failed).
///
/// Forces the failure by dropping the `fact_embedding` column from `RelatesToNode_` before the
/// rebuild — `create_vector_indexes`'s 'RelatesToNode_' step then hits a genuine "column does
/// not exist" error rather than "already exists". Unlike dropping the table itself, the table
/// remains intact and queryable, so this doesn't collaterally break `knowledge_status`'s own
/// node-count query (which reads `RelatesToNode_` unconditionally) — isolating the assertion to
/// the index-build signal this test actually targets. The WAL only carries Entity mutations, so
/// replay fidelity is unaffected (SC-004).
#[tokio::test]
async fn test_rebuild_reports_indices_built_false_on_genuine_build_failure() {
    let (db, _db_dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        // init_schema already built the FTS/HNSW indexes covering RelatesToNode_; both must be
        // dropped first or the column drop fails with "column used by index ...".
        conn.drop_vector_indexes();
        schema::drop_fts_indexes(&conn);
        conn.run_cypher("ALTER TABLE RelatesToNode_ DROP fact_embedding")
            .expect("drop fact_embedding column");
    }

    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    let content = [
        entity_wal_line(0, "sc003-entity-a"),
        entity_wal_line(1, "sc003-entity-b"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260618_000000_sc003.jsonl"),
        &content,
    )
    .unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(70),
        method: "knowledge_rebuild_from_wal".to_string(),
        params: json!({}),
    };
    let resp = handlers::dispatch(req, Arc::clone(&state), Some(tx)).await;
    while rx.try_recv().is_ok() {}
    let v = serde_json::to_value(resp).unwrap();

    // Replay itself succeeded and is unaffected — the WAL never referenced the dropped table.
    assert_eq!(
        v["result"]["success"], true,
        "replay must still report success even though index build failed: {v}"
    );
    assert_eq!(v["result"]["mutations_replayed"], 2, "{v}");

    // FR-004/FR-006: index-build failure must be independently, explicitly observable.
    assert_eq!(
        v["result"]["indices_built"], false,
        "rebuild result must report indices_built: false on genuine build failure: {v}"
    );

    // FR-005: knowledge_status must reflect the same non-ready state without a search attempt.
    let status_v = dispatch(71, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_eq!(
        status_v["result"]["indices_built"], false,
        "knowledge_status must report indices_built: false: {status_v}"
    );

    // The in-memory flag must also reflect the real (failed) outcome, not a stale prior success.
    assert!(
        !state.indices_built.load(Ordering::Acquire),
        "indices_built flag must be false after a genuine build failure"
    );
}

// ── FR-005: non-empty-database guard for from_seq: 0 rebuilds (issue #239) ────────────────────

/// Like `make_db`, but also returns the real on-disk DB path so `clear_db_for_rebuild` (invoked
/// via `force_clear: true`) can locate and reopen the same file `db` was opened from — unlike
/// most of this file's other tests, which use a placeholder `db_path` that never has to resolve
/// to a real file because none of those handlers touch it.
fn make_db_with_path(dim: usize) -> (Arc<Db>, TempDir, String) {
    let dir = TempDir::new().unwrap();
    let db_path = dir
        .path()
        .join("wal_admin_test.db")
        .to_str()
        .unwrap()
        .to_string();
    let db = Arc::new(Db::open(&db_path).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }
    (db, dir, db_path)
}

fn make_state_with_wal_and_path(
    db: Arc<Db>,
    wal_dir: std::path::PathBuf,
    db_path: String,
) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = WalWriter::new(wal_dir.join("liminis"), 10_000, 0).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path,
        wal_root: Some(wal_dir),
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writers: Arc::new(Mutex::new(
            wal_writer
                .into_iter()
                .map(|w| ("liminis".to_string(), w))
                .collect(),
        )),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
    })
}

/// SC-002: a `from_seq: 0` rebuild against a non-empty database fails fast by default, with a
/// clear, explicit error — not a flood of duplicate-primary-key `failed_samples`.
#[tokio::test]
async fn test_rebuild_from_wal_non_empty_db_fails_fast_by_default() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher("CREATE (:Entity {uuid: 'pre-existing-entity', group_id: 'liminis'})")
            .unwrap();
    }
    let entity_count_before = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };

    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_fff666_0000.jsonl"),
        entity_wal_line(0, "fr005-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db.clone(), wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        30,
        "knowledge_rebuild_from_wal",
        json!({}),
        Arc::clone(&state),
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "expected an explicit error for a from_seq:0 rebuild against a non-empty DB: {v}"
    );
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("already contains data") || msg.contains("non-empty"),
        "error must name the non-empty database as the cause: {v}"
    );
    assert!(
        msg.contains("force_clear") || msg.contains("knowledge_clear_all"),
        "error must state the corrective action: {v}"
    );

    // No job was created and no replay happened — no rebuild_jobs entry, no partial state.
    let entity_count_after = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };
    assert_eq!(
        entity_count_before, entity_count_after,
        "fail-fast must occur before any write — entity count must be untouched"
    );
}

/// SC-002: `force_clear: true` clears the database before replaying, producing a clean rebuild
/// with no duplicate-key noise.
#[tokio::test]
async fn test_rebuild_from_wal_non_empty_db_force_clear_succeeds() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'stale-entity-to-be-cleared', group_id: 'liminis'})",
        )
        .unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_ggg777_0000.jsonl"),
        entity_wal_line(0, "force-clear-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db, wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        31,
        "knowledge_rebuild_from_wal",
        json!({"force_clear": true}),
        Arc::clone(&state),
    )
    .await;

    assert_eq!(
        v["result"]["success"], true,
        "force_clear:true must allow the rebuild to proceed: {v}"
    );
    let job_id = v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    // Poll until completed (up to 5 seconds).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch(
            32,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        let status = status_v["result"]["status"].as_str().unwrap_or("?");
        match status {
            "completed" => {
                assert_eq!(
                    status_v["result"]["result"]["failed_lines"], 0,
                    "clean rebuild after force_clear must have zero duplicate-key failures: {status_v}"
                );
                break;
            }
            "failed" => panic!("rebuild job failed: {status_v}"),
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "rebuild job did not complete within 5s: {status_v}"
                );
            }
        }
    }

    // The stale pre-existing entity must be gone; only the WAL-replayed entity remains.
    let db_after = state
        .db
        .load_full()
        .expect("db must be present after clear+rebuild");
    let conn = db_after.connect().unwrap();
    assert!(
        conn.get_entity_by_uuid("stale-entity-to-be-cleared")
            .unwrap()
            .is_none(),
        "force_clear must have removed the stale pre-existing entity"
    );
    assert!(
        conn.get_entity_by_uuid("force-clear-entity")
            .unwrap()
            .is_some(),
        "the WAL-replayed entity must exist after the clean rebuild"
    );
}

/// A dry run against a non-empty database must fail fast the same way, regardless of
/// `force_clear` — dry runs never mutate the DB, so "clearing" has no meaning there, and the
/// whole point is to surface the problem before the operator commits to a real run.
#[tokio::test]
async fn test_rebuild_from_wal_non_empty_db_dry_run_fails_fast_even_with_force_clear() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher("CREATE (:Entity {uuid: 'dry-run-pre-existing', group_id: 'liminis'})")
            .unwrap();
    }
    let entity_count_before = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };

    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_hhh888_0000.jsonl"),
        entity_wal_line(0, "dry-run-entity") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db.clone(), wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        33,
        "knowledge_rebuild_from_wal",
        json!({"dry_run": true, "force_clear": true}),
        Arc::clone(&state),
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "dry_run against a non-empty DB must fail fast even with force_clear:true: {v}"
    );

    let entity_count_after = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };
    assert_eq!(
        entity_count_before, entity_count_after,
        "dry_run must never mutate the DB, including via force_clear"
    );
}

/// FR-006 regression guard: a non-empty database with `from_seq > 0` (incremental resume) must
/// not be blocked by the FR-005 guard — that protection is scoped to `from_seq: 0` full rebuilds
/// only.
#[tokio::test]
async fn test_rebuild_from_wal_non_empty_db_from_seq_gt_zero_unaffected() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher("CREATE (:Entity {uuid: 'resume-pre-existing'})")
            .unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    let content = [
        entity_wal_line(0, "resume-skip"),
        entity_wal_line(1, "resume-apply"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_iii999_0000.jsonl"),
        &content,
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db, wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        34,
        "knowledge_rebuild_from_wal",
        json!({"from_seq": 1}),
        Arc::clone(&state),
    )
    .await;

    assert_eq!(
        v["result"]["success"], true,
        "from_seq > 0 against a non-empty DB must proceed unaffected by the FR-005 guard: {v}"
    );
    assert!(
        v.get("error").is_none(),
        "must not surface the non-empty-database error for an incremental resume: {v}"
    );
}

// ── Issue #432: force_clear guard must check the WAL content's embedded group_id(s), not
// only the request's own group_id (the WAL directory's owning group) ──────────────────────

/// Reproduces the issue: a WAL directory owned by `liminis` (the request's default group_id)
/// whose content is a migrated legacy stream carrying rows for a *different* embedded
/// `group_id` (`apollo_program`) that already has pre-existing data sharing the same uuid the
/// WAL line recreates. Before the fix, the guard only checked `liminis` (empty), so
/// `force_clear: true` cleared nothing and replay collided with the pre-existing
/// `apollo_program` entity, producing a duplicate-primary-key failure. After the fix, the guard
/// discovers `apollo_program` by scanning the WAL content, finds it non-empty, and clears it
/// before replay — so the rebuild completes with zero failures (SC-001).
#[tokio::test]
async fn test_rebuild_from_wal_migrated_legacy_stream_force_clear_clears_embedded_group() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        // Pre-existing data lives under the *embedded* group_id, not the WAL directory's own
        // owning group_id ("liminis") — simulating a prior full rebuild of this same migrated
        // legacy content.
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'shared-uuid', group_id: 'apollo_program', \
             name: 'stale', labels: ['Entity'], created_at: timestamp('2026-05-22 00:00:00'), \
             name_embedding: [0.0, 0.0, 0.0, 0.0], summary: 'stale', attributes: '{}'})",
        )
        .unwrap();
    }

    // The WAL directory is owned by "liminis" (the request's default group_id — this is where
    // migrate_wal_root_if_needed would have relocated a pre-#378 flat stream), but its content's
    // params.group_id is "apollo_program", the same uuid as the pre-existing entity above.
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_jjj432_0000.jsonl"),
        entity_wal_line_with_group(0, "shared-uuid", "apollo_program") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db.clone(), wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        35,
        "knowledge_rebuild_from_wal",
        json!({"force_clear": true}),
        Arc::clone(&state),
    )
    .await;

    assert_eq!(
        v["result"]["success"], true,
        "force_clear:true must allow the rebuild to proceed: {v}"
    );
    let job_id = v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch(
            36,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        let status = status_v["result"]["status"].as_str().unwrap_or("?");
        match status {
            "completed" => {
                assert_eq!(
                    status_v["result"]["result"]["failed_lines"], 0,
                    "the guard must have cleared the embedded apollo_program group before \
                     replay, so there must be zero duplicate-key failures: {status_v}"
                );
                break;
            }
            "failed" => panic!("rebuild job failed: {status_v}"),
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "rebuild job did not complete within 5s: {status_v}"
                );
            }
        }
    }

    let db_after = state
        .db
        .load_full()
        .expect("db must be present after clear+rebuild");
    let conn = db_after.connect().unwrap();
    let entity = conn
        .get_entity_by_uuid("shared-uuid")
        .unwrap()
        .expect("the WAL-replayed entity must exist after the clean rebuild");
    assert_eq!(
        entity.summary, "s",
        "the entity's fields must come from the WAL replay, not the stale pre-existing row \
         (the stale row's summary was 'stale', the WAL line's is 's')"
    );
}

/// FR-004 companion: same setup as the force_clear test above, but `force_clear` is omitted.
/// The refusal error must name the *embedded* colliding group (`apollo_program`), not only the
/// request's own group_id (`liminis`, which is actually empty) — otherwise an operator reading
/// the error would look at entirely the wrong group.
#[tokio::test]
async fn test_rebuild_from_wal_migrated_legacy_stream_no_force_clear_names_embedded_group() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'shared-uuid-2', group_id: 'apollo_program', \
             name: 'stale', labels: ['Entity'], created_at: timestamp('2026-05-22 00:00:00'), \
             name_embedding: [0.0, 0.0, 0.0, 0.0], summary: 'stale', attributes: '{}'})",
        )
        .unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_kkk432_0000.jsonl"),
        entity_wal_line_with_group(0, "shared-uuid-2", "apollo_program") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db.clone(), wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        37,
        "knowledge_rebuild_from_wal",
        json!({}),
        Arc::clone(&state),
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "expected an explicit error: the embedded group_id already contains data: {v}"
    );
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("apollo_program"),
        "error must name the embedded colliding group_id, not only the request's own \
         (empty) group_id: {v}"
    );
    assert!(
        msg.contains("force_clear") || msg.contains("knowledge_delete_by_group"),
        "error must state the corrective action: {v}"
    );

    // Fail-fast must occur before any write.
    let conn = db.connect().unwrap();
    assert_eq!(
        conn.count_nodes("Entity").unwrap(),
        1,
        "no replay must have happened — entity count must be untouched"
    );
}

/// FR-007/SC-003 regression guard: when the WAL content's embedded `group_id` already equals
/// the request's own group_id (the common, non-legacy case), behavior must be unchanged from
/// today — a single group is checked and cleared, exactly as before this fix.
#[tokio::test]
async fn test_rebuild_from_wal_common_case_embedded_group_matches_request_unaffected() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'stale-common-case', group_id: 'liminis', \
             name: 'stale', labels: ['Entity'], created_at: timestamp('2026-05-22 00:00:00'), \
             name_embedding: [0.0, 0.0, 0.0, 0.0], summary: 'stale', attributes: '{}'})",
        )
        .unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_lll432_0000.jsonl"),
        entity_wal_line_with_group(0, "common-case-entity", "liminis") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db.clone(), wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        38,
        "knowledge_rebuild_from_wal",
        json!({"force_clear": true}),
        Arc::clone(&state),
    )
    .await;

    assert_eq!(v["result"]["success"], true, "{v}");
    let job_id = v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch(
            39,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        let status = status_v["result"]["status"].as_str().unwrap_or("?");
        match status {
            "completed" => {
                assert_eq!(
                    status_v["result"]["result"]["failed_lines"], 0,
                    "{status_v}"
                );
                break;
            }
            "failed" => panic!("rebuild job failed: {status_v}"),
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "rebuild job did not complete within 5s: {status_v}"
                );
            }
        }
    }

    let db_after = state
        .db
        .load_full()
        .expect("db must be present after clear+rebuild");
    let conn = db_after.connect().unwrap();
    assert!(
        conn.get_entity_by_uuid("stale-common-case")
            .unwrap()
            .is_none(),
        "the stale pre-existing entity in the request's own group must have been cleared"
    );
    assert!(
        conn.get_entity_by_uuid("common-case-entity")
            .unwrap()
            .is_some(),
        "the WAL-replayed entity must exist after the clean rebuild"
    );
}

// ── Issue #462: force_clear must not clear a group's data outside the replay it's about to
// run — a group referenced by the replayed directory's content can also have independent rows
// in a separate, un-replayed WAL stream elsewhere ──────────────────────────────────────────

/// SC-003: content for group `apollo_program` exists in the directory being replayed
/// ("liminis", a migrated-legacy layout), and additional rows for `apollo_program` exist in a
/// separate, independent WAL stream (`apollo_program`'s own post-#378 directory) that this
/// rebuild never touches. A `force_clear: true` rebuild against "liminis" must still clear and
/// correctly recreate the embedded legacy row (#432's collision guard, FR-002) without
/// destroying the independently-streamed row that lives only in the other directory (FR-001).
#[tokio::test]
async fn test_rebuild_from_wal_split_stream_force_clear_preserves_independent_stream() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        // Stale data for the uuid the "liminis" directory's replay will recreate — proves
        // FR-002 (the collision guard) still holds for the split case: it must be cleared
        // before replay, not left to collide.
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'legacy-embedded', group_id: 'apollo_program', \
             name: 'stale', labels: ['Entity'], created_at: timestamp('2026-05-22 00:00:00'), \
             name_embedding: [0.0, 0.0, 0.0, 0.0], summary: 'stale', attributes: '{}'})",
        )
        .unwrap();
        // Data that only lives in apollo_program's own independent stream — this replay must
        // never touch it (FR-001).
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'independent-stream-row', group_id: 'apollo_program', \
             name: 'independent', labels: ['Entity'], \
             created_at: timestamp('2026-05-22 00:00:00'), \
             name_embedding: [0.0, 0.0, 0.0, 0.0], summary: 'independent', attributes: '{}'})",
        )
        .unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    // The directory being replayed: owned by "liminis" (migrated-legacy layout, as in #432's
    // scenario), but its content embeds apollo_program via a bare CREATE line.
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_mmm462_0000.jsonl"),
        bare_create_entity_wal_line(0, "legacy-embedded", "apollo_program") + "\n",
    )
    .unwrap();
    // apollo_program's own, independent, post-#378 stream — never replayed by this request.
    // Its mere existence (not its content) is what makes apollo_program "split" for issue #462
    // purposes; `independent-stream-row` above was written directly to the DB to simulate what
    // this stream's own prior replay would have produced.
    std::fs::create_dir_all(wal_dir.path().join("apollo_program")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("apollo_program")
            .join("20260701_000000_nnn462_0000.jsonl"),
        bare_create_entity_wal_line(0, "independent-stream-row", "apollo_program") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db.clone(), wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        40,
        "knowledge_rebuild_from_wal",
        json!({"force_clear": true}),
        Arc::clone(&state),
    )
    .await;

    assert_eq!(
        v["result"]["success"], true,
        "force_clear:true must allow the rebuild to proceed: {v}"
    );
    let job_id = v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch(
            41,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        let status = status_v["result"]["status"].as_str().unwrap_or("?");
        match status {
            "completed" => {
                assert_eq!(
                    status_v["result"]["result"]["failed_lines"], 0,
                    "the guard must have row-scope-cleared only legacy-embedded before replay, \
                     so there must be zero duplicate-key failures: {status_v}"
                );
                break;
            }
            "failed" => panic!("rebuild job failed: {status_v}"),
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "rebuild job did not complete within 5s: {status_v}"
                );
            }
        }
    }

    let db_after = state
        .db
        .load_full()
        .expect("db must be present after clear+rebuild");
    let conn = db_after.connect().unwrap();
    let recreated = conn
        .get_entity_by_uuid("legacy-embedded")
        .unwrap()
        .expect("the WAL-replayed embedded entity must exist after the clean rebuild");
    assert_eq!(
        recreated.summary, "s",
        "legacy-embedded's fields must come from the WAL replay, not the stale pre-existing \
         row (stale summary was 'stale', the WAL line's is 's')"
    );
    assert!(
        conn.get_entity_by_uuid("independent-stream-row")
            .unwrap()
            .is_some(),
        "the row that lives only in apollo_program's own independent stream must survive a \
         force_clear rebuild of the unrelated liminis directory — this is the exact data loss \
         issue #462 exists to fix"
    );
}

/// SC-004: same split-stream setup as the test above, but the "liminis" directory's content
/// embeds a mutating non-`CREATE` (`SET`) line referencing `apollo_program`, not just `CREATE`
/// lines. This replay can't tell whether it's safe to clear only the rows it will recreate or
/// the whole group — FR-004 requires the rebuild to refuse outright, observably from the
/// response, rather than silently resolve the ambiguity in either direction. No data may be
/// touched.
#[tokio::test]
async fn test_rebuild_from_wal_split_stream_unsafe_mutation_refuses_rebuild() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'blocked-embedded', group_id: 'apollo_program', \
             name: 'stale', labels: ['Entity'], created_at: timestamp('2026-05-22 00:00:00'), \
             name_embedding: [0.0, 0.0, 0.0, 0.0], summary: 'stale', attributes: '{}'})",
        )
        .unwrap();
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'blocked-independent', group_id: 'apollo_program', \
             name: 'independent', labels: ['Entity'], \
             created_at: timestamp('2026-05-22 00:00:00'), \
             name_embedding: [0.0, 0.0, 0.0, 0.0], summary: 'independent', attributes: '{}'})",
        )
        .unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    let content = [
        bare_create_entity_wal_line(0, "blocked-embedded", "apollo_program"),
        unsafe_set_wal_line_with_group(1, "blocked-embedded", "apollo_program"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_ooo462_0000.jsonl"),
        content,
    )
    .unwrap();
    std::fs::create_dir_all(wal_dir.path().join("apollo_program")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("apollo_program")
            .join("20260701_000000_ppp462_0000.jsonl"),
        bare_create_entity_wal_line(0, "blocked-independent", "apollo_program") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db.clone(), wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        42,
        "knowledge_rebuild_from_wal",
        json!({"force_clear": true}),
        Arc::clone(&state),
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "expected an explicit refusal: apollo_program is split and its embedded content is not \
         safely clearable: {v}"
    );
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("apollo_program"),
        "error must name the blocked group: {v}"
    );

    // Fail-fast must occur before any clear.
    let conn = db.connect().unwrap();
    assert_eq!(
        conn.count_nodes("Entity").unwrap(),
        2,
        "no clear must have happened — both pre-existing apollo_program entities must remain \
         untouched: {v}"
    );
}

/// Row-scoped clearing must not silently sever topology for rows it doesn't touch. Here
/// `topology-embedded`'s `CREATE` is in the directory being replayed (so it's a row-scope-clear
/// target), but the `RelatesToNode_` connecting it to `topology-partner` (same group) has its
/// own `CREATE` only in apollo_program's own, independent stream — never replayed by this
/// request. `DETACH DELETE`ing `topology-embedded` would sever the live two-hop `RELATES_TO`
/// connection into that surviving edge, and `purge_group_rows`'s forced-rebind pass does not
/// repair it (it only handles cross-group pointers, not this ordinary same-group edge). FR-004
/// requires the rebuild to refuse rather than silently corrupt that edge's topology.
#[tokio::test]
async fn test_rebuild_from_wal_split_stream_topology_hazard_refuses_rebuild() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'topology-embedded', group_id: 'apollo_program', \
             name: 'stale', labels: ['Entity'], created_at: timestamp('2026-05-22 00:00:00'), \
             name_embedding: [0.0, 0.0, 0.0, 0.0], summary: 'stale', attributes: '{}'})",
        )
        .unwrap();
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'topology-partner', group_id: 'apollo_program', \
             name: 'partner', labels: ['Entity'], created_at: timestamp('2026-05-22 00:00:00'), \
             name_embedding: [0.0, 0.0, 0.0, 0.0], summary: 'partner', attributes: '{}'})",
        )
        .unwrap();
        // Simulates an edge whose own CREATE lives only in apollo_program's own independent
        // stream: present in the DB (as that stream's own prior replay would have produced),
        // but never referenced by the "liminis" directory's content below.
        let edge = lcg_core::types::RelatesToEdge {
            uuid: "topology-edge".to_string(),
            name: "KNOWS".to_string(),
            source_node_uuid: "topology-embedded".to_string(),
            target_node_uuid: "topology-partner".to_string(),
            group_id: "apollo_program".to_string(),
            fact: "embedded knows partner".to_string(),
            fact_embedding: vec![0.0, 0.0, 0.0, 0.0],
            created_at: "2026-05-22T00:00:00Z".to_string(),
            valid_at: None,
            invalid_at: None,
            attributes: "{}".to_string(),
            relation_type: None,
            episode_uuids: vec![],
            source_descriptions: vec![],
        };
        conn.insert_relates_to_edge(&edge).unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260701_000000_qqq462_0000.jsonl"),
        bare_create_entity_wal_line(0, "topology-embedded", "apollo_program") + "\n",
    )
    .unwrap();
    std::fs::create_dir_all(wal_dir.path().join("apollo_program")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("apollo_program")
            .join("20260701_000000_rrr462_0000.jsonl"),
        bare_create_entity_wal_line(0, "topology-partner", "apollo_program") + "\n",
    )
    .unwrap();

    let state = make_state_with_wal_and_path(db.clone(), wal_dir.path().to_path_buf(), db_path);

    let v = dispatch(
        43,
        "knowledge_rebuild_from_wal",
        json!({"force_clear": true}),
        Arc::clone(&state),
    )
    .await;

    assert!(
        v.get("error").is_some(),
        "expected an explicit refusal: row-scope-clearing topology-embedded would sever its \
         live connection to topology-edge, which apollo_program's own independent stream — not \
         this replay — owns: {v}"
    );
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("apollo_program"),
        "error must name the blocked group: {v}"
    );

    // Fail-fast must occur before any clear or delete.
    let conn = db.connect().unwrap();
    assert_eq!(
        conn.count_nodes("Entity").unwrap(),
        2,
        "no clear must have happened — both pre-existing apollo_program entities must remain \
         untouched: {v}"
    );
    assert_eq!(
        conn.get_relates_to_by_uuids(&["topology-edge".to_string()])
            .unwrap()
            .len(),
        1,
        "the relates_to edge must remain untouched: {v}"
    );
}

// ── Issue #283 / FR-004: knowledge_query_cypher proactively rebuilds the NameIndex ─────

/// A raw-Cypher `CREATE` issued through `knowledge_query_cypher` bypasses every
/// `NameIndex` insert/update hook. FR-004 requires the handler to notice the mutation and
/// rebuild the index so the entity is immediately resolvable, without needing the scan
/// fallback issue #283 also introduces for the endpoint-authority call sites.
#[tokio::test]
async fn test_raw_cypher_mutation_proactively_rebuilds_name_index() {
    let (db, _dir) = make_db(4);
    let state = make_state_no_wal(Arc::clone(&db));

    let create_query = "CREATE (:Entity {uuid: 'raw-1', name: 'RawEntity', group_id: 'g', \
         labels: ['Entity'], created_at: timestamp('2026-01-01 00:00:00'), \
         name_embedding: [1.0, 0.0, 0.0, 0.0], summary: 's', attributes: '{}'})";
    let v = dispatch(
        80,
        "knowledge_query_cypher",
        json!({"query": create_query}),
        Arc::clone(&state),
    )
    .await;
    assert!(
        v.get("error").is_none(),
        "raw Cypher CREATE via knowledge_query_cypher should succeed: {v}"
    );

    let conn = db.connect().unwrap();
    assert_eq!(
        conn.get_entity_by_name_ci("RawEntity", "g")
            .unwrap()
            .expect("plain index-only lookup must find it — proves the proactive rebuild ran")
            .uuid,
        "raw-1"
    );
    assert_eq!(
        conn.name_index_fallback_scan_count(),
        0,
        "the proactive rebuild must make the entity resolvable without a fallback scan"
    );
}

/// A read-only Cypher query through the same handler must not trigger a rebuild — FR-004's
/// mutation-keyword heuristic should leave the (still-empty, still-trusted) index alone.
#[tokio::test]
async fn test_read_only_raw_cypher_does_not_rebuild_name_index() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        // Bypass insert_entity so the index starts blind to this entity, isolating the
        // assertion to "did a MATCH-only query trigger a rebuild" rather than depending on
        // whether some earlier write already populated it.
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'raw-2', name: 'Untouched', group_id: 'g', \
             labels: ['Entity'], created_at: timestamp('2026-01-01 00:00:00'), \
             name_embedding: [1.0, 0.0, 0.0, 0.0], summary: 's', attributes: '{}'})",
        )
        .unwrap();
    }
    let state = make_state_no_wal(Arc::clone(&db));

    let v = dispatch(
        81,
        "knowledge_query_cypher",
        json!({"query": "MATCH (e:Entity) RETURN e.uuid"}),
        Arc::clone(&state),
    )
    .await;
    assert!(
        v.get("error").is_none(),
        "read-only query should succeed: {v}"
    );

    let conn = db.connect().unwrap();
    assert!(
        conn.get_entity_by_name_ci("Untouched", "g")
            .unwrap()
            .is_none(),
        "a read-only query must not trigger a rebuild that would incidentally pick up the \
         raw-Cypher-created entity"
    );
}

/// Index DDL (`CALL CREATE_VECTOR_INDEX(...)`) contains the `CREATE` keyword, which
/// `looks_like_mutation` alone would treat as an Entity mutation. `log_mutation` (wal.rs) has
/// always excluded index DDL from its own mutation check via the same `is_index_ddl` filter;
/// `handle_query_cypher` must apply it too, or a `CREATE_VECTOR_INDEX` call through the raw
/// Cypher `cypher` MCP scope would trigger a wasted full-Entity-table rebuild scan.
#[tokio::test]
async fn test_index_ddl_via_raw_cypher_does_not_rebuild_name_index() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        // Bypass insert_entity so the index starts blind to this entity, isolating the
        // assertion to "did the CREATE_VECTOR_INDEX call trigger a rebuild".
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'raw-4', name: 'StillUntouched', group_id: 'g', \
             labels: ['Entity'], created_at: timestamp('2026-01-01 00:00:00'), \
             name_embedding: [1.0, 0.0, 0.0, 0.0], summary: 's', attributes: '{}'})",
        )
        .unwrap();
    }
    let state = make_state_no_wal(Arc::clone(&db));

    let v = dispatch(
        82,
        "knowledge_query_cypher",
        json!({"query": "CALL CREATE_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', \
             'name_embedding', metric := 'cosine')"}),
        Arc::clone(&state),
    )
    .await;
    assert!(
        v.get("error").is_none(),
        "index-creation DDL should succeed: {v}"
    );

    let conn = db.connect().unwrap();
    assert!(
        conn.get_entity_by_name_ci("StillUntouched", "g")
            .unwrap()
            .is_none(),
        "index DDL must not trigger a NameIndex rebuild that would incidentally pick up the \
         raw-Cypher-created entity"
    );
}

// ── Issue #283 / SC-004: knowledge_status surfaces NameIndex trust + fallback-scan count ──

#[tokio::test]
async fn test_knowledge_status_surfaces_name_index_trust_and_fallback_scan_count() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher(
            "CREATE (:Entity {uuid: 'raw-3', name: 'Tracked', group_id: 'g', \
             labels: ['Entity'], created_at: timestamp('2026-01-01 00:00:00'), \
             name_embedding: [1.0, 0.0, 0.0, 0.0], summary: 's', attributes: '{}'})",
        )
        .unwrap();
        // Trigger one fallback scan, then force the untrusted flag directly — simulating the
        // FR-003 failure posture (a failed post-replay rebuild) independent of this test's
        // own raw_query call, which doesn't go through the handlers.rs rebuild wiring.
        assert!(conn
            .get_entity_by_name_ci_with_scan_fallback("Tracked", "g")
            .unwrap()
            .is_some());
        conn.mark_name_index_untrusted();
    }
    let state = make_state_no_wal(db);

    let v = dispatch(82, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert!(
        v.get("error").is_none(),
        "knowledge_status should succeed: {v}"
    );
    assert_eq!(
        v["result"]["name_index_trusted"], false,
        "status must reflect the untrusted mark: {v}"
    );
    assert_eq!(
        v["result"]["name_index_fallback_scans"], 1,
        "status must reflect the fallback-scan count: {v}"
    );
}

// ── Issue #352: re-derive WalWriter::global_seq after rebuild/clear ────────────

/// A standalone `Entity` WAL line built with a plain `CREATE` (rather than `entity_wal_line`'s
/// `MERGE ... ON CREATE SET`) — used to engineer a duplicate-primary-key replay failure against
/// a uuid that a prior line in the same WAL already created.
fn entity_create_conflict_wal_line(seq: u64, uuid: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"CREATE (:Entity {{uuid: '{uuid}', name: '{uuid}', group_id: 'g', labels: ['t'], created_at: timestamp('2026-05-22 00:00:00'), name_embedding: [1.0, 0.0, 0.0, 0.0], summary: 's', attributes: '{{}}'}})","params":{{}}}}"#
    )
}

/// Reads every `.jsonl` file in `dir` and returns all `seq` values found across every line (not
/// just the last line per file, unlike `WalWriter`'s internal `scan_max_seq`) — used to detect
/// duplicate `seq`s across the whole WAL directory (SC-003).
fn all_seqs_in_wal_dir(dir: &std::path::Path) -> Vec<u64> {
    let mut seqs = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line).unwrap();
            seqs.push(v["seq"].as_u64().unwrap());
        }
    }
    seqs
}

/// Dispatches `knowledge_rebuild_from_wal` (background-job path) and polls
/// `knowledge_rebuild_status` until it completes, returning the final status response. Panics if
/// the job fails or does not complete within 5 seconds — mirrors the polling pattern used by
/// `test_rebuild_from_wal_non_empty_db_force_clear_succeeds`.
async fn rebuild_and_wait(id: i64, params: Value, state: Arc<AppState>) -> Value {
    let v = dispatch(id, "knowledge_rebuild_from_wal", params, Arc::clone(&state)).await;
    assert_eq!(v["result"]["success"], true, "rebuild dispatch failed: {v}");
    let job_id = v["result"]["job_id"]
        .as_str()
        .expect("expected job_id (background-job path)")
        .to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch(
            id + 1,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        let status = status_v["result"]["status"].as_str().unwrap_or("?");
        match status {
            "completed" => return status_v,
            "failed" => panic!("rebuild job failed: {status_v}"),
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "rebuild job did not complete within 5s: {status_v}"
                );
            }
        }
    }
}

/// Dispatches `knowledge_process_chunk` with fixed content (`MockExtractor` is deterministic).
async fn process_a_chunk(id: i64, state: Arc<AppState>) -> Value {
    dispatch(
        id,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": format!("chunk-{id}"),
            "source_file": "test.txt",
            "reference_time": "2026-08-05T00:00:00Z",
        }),
        state,
    )
    .await
}

/// SC-001: a WAL directory populated after the service started (and the writer's `global_seq`
/// was derived), followed by `knowledge_rebuild_from_wal`, must not let the next
/// `knowledge_process_chunk` collide with a `seq` already present on disk.
#[tokio::test]
async fn test_global_seq_resync_after_rebuild_picks_up_externally_populated_wal() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    // Service starts against an EMPTY WAL dir — WalWriter::new derives global_seq = 0.
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    // WAL directory populated out-of-band after the service started; highest seq = 5.
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260805_000000_ext001_0000.jsonl"),
        format!(
            "{}\n{}\n",
            entity_wal_line(4, "external-entity-a"),
            entity_wal_line(5, "external-entity-b")
        ),
    )
    .unwrap();

    rebuild_and_wait(400, json!({}), Arc::clone(&state)).await;

    let chunk_v = process_a_chunk(410, Arc::clone(&state)).await;
    assert_eq!(chunk_v["result"]["success"], true, "{chunk_v}");

    let seqs = all_seqs_in_wal_dir(&wal_dir.path().join("liminis"));
    let max_seq_after = seqs.iter().copied().max().unwrap();
    assert!(
        max_seq_after > 5,
        "seq written after rebuild must exceed the pre-existing on-disk max of 5, got {max_seq_after} (all seqs: {seqs:?})"
    );

    // SC-003: no seq value repeats anywhere in the WAL directory.
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        seqs.len(),
        "no seq value may repeat across the WAL directory: {seqs:?}"
    );
}

/// SC-002: the same guarantee holds when the rebuild uses `force_clear: true`.
#[tokio::test]
async fn test_global_seq_resync_after_force_clear_rebuild() {
    let (db, _dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher("CREATE (:Entity {uuid: 'stale-entity-352'})")
            .unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    let state = make_state_with_wal_and_path(db, wal_dir.path().to_path_buf(), db_path);

    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260805_000000_ext002_0000.jsonl"),
        entity_wal_line(9, "force-clear-resync-entity") + "\n",
    )
    .unwrap();

    rebuild_and_wait(420, json!({"force_clear": true}), Arc::clone(&state)).await;

    let chunk_v = process_a_chunk(430, Arc::clone(&state)).await;
    assert_eq!(chunk_v["result"]["success"], true, "{chunk_v}");

    let seqs = all_seqs_in_wal_dir(&wal_dir.path().join("liminis"));
    let max_seq_after = seqs.iter().copied().max().unwrap();
    assert!(
        max_seq_after > 9,
        "expected a seq > 9 after the force_clear rebuild, got {max_seq_after} (all seqs: {seqs:?})"
    );

    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        seqs.len(),
        "no seq value may repeat across the WAL directory: {seqs:?}"
    );
}

/// SC-004 (no regression): a service that starts with an already-populated WAL directory (no
/// external population after start, no rebuild) must still emit exactly the `seq` that
/// `WalWriter::new`'s startup scan derived — this fix must not change that existing path.
#[tokio::test]
async fn test_global_seq_unaffected_without_rebuild_no_regression() {
    let (db, _dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260805_000000_ext003_0000.jsonl"),
        entity_wal_line(7, "preexisting-entity") + "\n",
    )
    .unwrap();

    // WalWriter::new scans the pre-existing WAL dir at construction: global_seq = 8.
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    let chunk_v = process_a_chunk(440, Arc::clone(&state)).await;
    assert_eq!(chunk_v["result"]["success"], true, "{chunk_v}");

    let seqs = all_seqs_in_wal_dir(&wal_dir.path().join("liminis"));
    let first_new_seq = seqs.iter().copied().filter(|&s| s != 7).min();
    assert_eq!(
        first_new_seq,
        Some(8),
        "unchanged behavior: the first seq emitted after a populated-at-startup WAL dir must \
         be exactly 8, got {seqs:?}"
    );
}

/// SC-005 (FR-003 edge case): a WAL line that fails to replay can still have a higher on-disk
/// `seq` than the last successfully committed line. Re-derivation must take the max of the
/// fresh on-disk scan and `last_committed_seq`, not trust `last_committed_seq` alone.
#[tokio::test]
async fn test_global_seq_resync_uses_on_disk_scan_when_highest_seq_failed_to_replay() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    // seq 10 (MERGE) replays and commits successfully, creating the entity. seq 20 is a plain
    // CREATE for the SAME primary key, which now collides and fails to replay — so
    // ReplayStats::last_committed_seq stops at 10, but the highest seq line on disk is 20
    // (scan_max_seq itself returns 21, the next seq to assign, not the raw max).
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260805_000000_ext005_0000.jsonl"),
        format!(
            "{}\n{}\n",
            entity_wal_line(10, "dup-entity-352"),
            entity_create_conflict_wal_line(20, "dup-entity-352")
        ),
    )
    .unwrap();

    let status_v = rebuild_and_wait(460, json!({}), Arc::clone(&state)).await;
    assert!(
        status_v["result"]["result"]["failed_lines"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "expected the conflicting CREATE to fail replay: {status_v}"
    );
    assert_eq!(
        status_v["result"]["result"]["last_committed_seq"], 10,
        "only the first (MERGE) line should have committed: {status_v}"
    );

    let chunk_v = process_a_chunk(470, Arc::clone(&state)).await;
    assert_eq!(chunk_v["result"]["success"], true, "{chunk_v}");

    let seqs = all_seqs_in_wal_dir(&wal_dir.path().join("liminis"));
    let max_seq_after = seqs.iter().copied().max().unwrap();
    assert!(
        max_seq_after > 20,
        "resync must use the on-disk scan (seq 20), not last_committed_seq (10) alone: \
         got max {max_seq_after} (all seqs: {seqs:?})"
    );
}

/// SC-006: a `dry_run: true` rebuild MUST NOT re-derive `global_seq` — a dry run is documented
/// as having no observable side effects, including on in-process writer state (FR-006).
#[tokio::test]
async fn test_dry_run_rebuild_does_not_resync_global_seq() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    // Empty WAL dir at startup — WalWriter::new derives global_seq = 0.
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    // Populated after start with a much higher seq.
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260805_000000_ext006_0000.jsonl"),
        entity_wal_line(99, "dry-run-entity") + "\n",
    )
    .unwrap();

    // Non-streaming dry run returns synchronously — no job to poll.
    let v = dispatch(
        480,
        "knowledge_rebuild_from_wal",
        json!({"dry_run": true}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(v["result"]["dry_run"], true, "{v}");

    let chunk_v = process_a_chunk(490, Arc::clone(&state)).await;
    assert_eq!(chunk_v["result"]["success"], true, "{chunk_v}");

    let seqs = all_seqs_in_wal_dir(&wal_dir.path().join("liminis"));
    let first_new_seq = seqs.iter().copied().filter(|&s| s != 99).min();
    assert_eq!(
        first_new_seq,
        Some(0),
        "a dry run must not have re-derived global_seq — the writer must still start from its \
         pre-dry-run value of 0: {seqs:?}"
    );
}

// ── to_seq bounded rebuild (#362) ────────────────────────────────────────────

/// User Story 1 / SC-003: a WAL with a "bad" mutation at seq N, bounded via `to_seq: N-1`,
/// excludes it from the rebuilt graph. Exercises the streaming (progress-token) call shape.
#[tokio::test]
async fn test_rebuild_from_wal_to_seq_excludes_bad_mutation_streaming() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    let content = [
        entity_wal_line(0, "good-entity-a"),
        entity_wal_line(1, "good-entity-b"),
        entity_wal_line(2, "bad-entity-362"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260808_000000_stream362.jsonl"),
        &content,
    )
    .unwrap();

    let state = make_state_with_wal(db.clone(), wal_dir.path().to_path_buf());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(500),
        method: "knowledge_rebuild_from_wal".to_string(),
        params: json!({"to_seq": 1}),
    };
    let resp = handlers::dispatch(req, Arc::clone(&state), Some(tx)).await;
    while rx.try_recv().is_ok() {}
    let v = serde_json::to_value(resp).unwrap();

    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["mutations_replayed"], 2, "{v}");
    assert_eq!(
        v["result"]["to_seq"], 1,
        "the applied to_seq bound must be echoed back in the result: {v}"
    );
    assert_eq!(
        v["result"]["from_seq"], 0,
        "the applied from_seq bound must be echoed back in the result: {v}"
    );

    let conn = db.connect().unwrap();
    assert_eq!(
        conn.count_nodes("Entity").unwrap(),
        2,
        "only seq<=1 entities replayed"
    );
    assert!(
        conn.get_entity_by_uuid("bad-entity-362").unwrap().is_none(),
        "the bad mutation at seq=2 must not have been applied"
    );
}

/// User Story 2 / SC: `dry_run: true` combined with `to_seq` bounds the returned statistics and
/// leaves the live database untouched. Exercises the non-streaming dry-run call shape.
#[tokio::test]
async fn test_rebuild_from_wal_to_seq_dry_run_bounded_and_untouched() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();

    let content = [
        entity_wal_line(0, "dry-good-a"),
        entity_wal_line(1, "dry-good-b"),
        entity_wal_line(2, "dry-excluded"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260808_000000_dry362.jsonl"),
        &content,
    )
    .unwrap();

    let state = make_state_with_wal(db.clone(), wal_dir.path().to_path_buf());
    let count_before = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };

    let v = dispatch(
        501,
        "knowledge_rebuild_from_wal",
        json!({"dry_run": true, "to_seq": 1}),
        Arc::clone(&state),
    )
    .await;

    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["dry_run"], true, "{v}");
    assert_eq!(
        v["result"]["mutations_replayed"], 2,
        "dry run stats must be bounded by to_seq: {v}"
    );
    assert_eq!(
        v["result"]["to_seq"], 1,
        "the applied to_seq bound must be echoed back even for a dry run: {v}"
    );

    let count_after = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };
    assert_eq!(count_before, count_after, "dry_run must not modify the DB");
}

/// User Story 1 / FR-005, FR-006, SC-004, SC-005: after a bounded, non-dry-run rebuild via the
/// non-streaming background-job call shape, `knowledge_status`'s `wal.applied_seq` reflects the
/// bounded landing point (<= to_seq), and a subsequent write is assigned a seq strictly greater
/// than the WAL's true on-disk maximum — never colliding with the excluded, unapplied tail.
#[tokio::test]
async fn test_rebuild_from_wal_to_seq_background_job_bounds_applied_seq_and_avoids_collision() {
    let (db, _db_dir, db_path) = make_db_with_path(4);
    {
        let conn = db.connect().unwrap();
        conn.run_cypher("CREATE (:Entity {uuid: 'pre-existing-362-bg'})")
            .unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(wal_dir.path().join("liminis")).unwrap();
    let state = make_state_with_wal_and_path(db, wal_dir.path().to_path_buf(), db_path);

    let content = [
        entity_wal_line(0, "bg-good-a"),
        entity_wal_line(1, "bg-good-b"),
        entity_wal_line(2, "bg-excluded"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        wal_dir
            .path()
            .join("liminis")
            .join("20260808_000000_bg362.jsonl"),
        &content,
    )
    .unwrap();

    // from_seq: 0, force_clear: true — the corrupted-mutation recovery scenario (Edge Cases).
    let job_status_v = rebuild_and_wait(
        502,
        json!({"from_seq": 0, "to_seq": 1, "force_clear": true}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(
        job_status_v["result"]["result"]["to_seq"], 1,
        "the applied to_seq bound must be echoed in the background job's result: {job_status_v}"
    );

    let status_v = dispatch(504, "knowledge_status", json!({}), Arc::clone(&state)).await;
    let applied_seq = status_v["result"]["wal"]["applied_seq"]
        .as_u64()
        .expect("applied_seq must be present and numeric");
    assert!(
        applied_seq <= 1,
        "applied_seq must be <= to_seq (1), got {applied_seq}: {status_v}"
    );

    // The bad mutation must not have been applied.
    let db_guard = state.db.load();
    let conn = db_guard.as_ref().unwrap().connect().unwrap();
    assert!(
        conn.get_entity_by_uuid("bg-excluded").unwrap().is_none(),
        "the excluded mutation at seq=2 must not have been applied"
    );
    drop(conn);
    drop(db_guard);

    // FR-006/SC-005: a subsequent write must not collide with the excluded, unapplied seq=2
    // WAL entry still on disk.
    let chunk_v = process_a_chunk(505, Arc::clone(&state)).await;
    assert_eq!(chunk_v["result"]["success"], true, "{chunk_v}");

    let seqs = all_seqs_in_wal_dir(&wal_dir.path().join("liminis"));
    let max_seq_after = seqs.iter().copied().max().unwrap();
    assert!(
        max_seq_after > 2,
        "seq written after a bounded rebuild must exceed the WAL's true on-disk max of 2, \
         got {max_seq_after} (all seqs: {seqs:?})"
    );
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        seqs.len(),
        "no seq value may repeat across the WAL directory after a bounded rebuild: {seqs:?}"
    );
}
