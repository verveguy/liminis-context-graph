// Integration tests for autonomous WAL-corruption self-recovery (issue #151).
//
// FR-012: (a) torn WAL → startup self-recovery; (b) knowledge_recover_full IPC;
// (b) idempotency; (f) fallback to full rebuild when drop_lbug_wal fails.
//
// The unit tests for derive_episode_cursor (FR-012 c,d,e) live inside recovery.rs.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use liminis_graph_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    recovery,
    replay::WalReplayer,
    telemetry::{CaptureSink, NoopSink, TelemetrySink},
    WalWriter,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

// ── Helpers ───────────────────────────────────────────────────────────────────

const DIM: usize = 4;

fn make_db(dir: &TempDir) -> (Db, String) {
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let db = Db::open(&db_path).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
    }
    (db, db_path)
}

fn make_degraded_state(
    db_path: &str,
    wal_dir: std::path::PathBuf,
    sink: Arc<dyn TelemetrySink>,
) -> Arc<AppState> {
    let wal_writer = WalWriter::new(&wal_dir, 10_000, 0).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(None),
        degraded_reason: Arc::new(Mutex::new(Some("lbug_wal_corrupt".to_string()))),
        embedder: Arc::new(MockEmbedder::new(DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_path.to_string(),
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

fn make_healthy_state(db: Arc<Db>, wal_dir: std::path::PathBuf) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = WalWriter::new(&wal_dir, 10_000, 0).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(DIM)),
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

/// Writes a JSONL WAL line for an Episodic CREATE with all required fields.
fn write_episode_wal_line(wal_dir: &std::path::Path, filename: &str, seq: u64, uuid: &str) {
    let line = format!(
        "{}\n",
        json!({
            "seq": seq,
            "ts": "2026-06-18T00:00:00.000000+00:00",
            "db": "",
            "cypher": "CREATE (:Episodic {uuid: $uuid, name: $name, group_id: $group_id, \
                       created_at: $created_at, source: $source, \
                       source_description: $source_description, content: $content, \
                       content_embedding: $content_embedding, valid_at: $valid_at, \
                       entity_edges: $entity_edges})",
            "params": {
                "uuid": uuid,
                "name": format!("Episode {uuid}"),
                "group_id": "test-group",
                "created_at": "2026-06-18 00:00:00",
                "source": "text",
                "source_description": "test",
                "content": "test content",
                "content_embedding": [0.1_f32, 0.2_f32, 0.3_f32, 0.4_f32],
                "valid_at": "2026-06-18 00:00:00",
                "entity_edges": [],
            }
        })
    );
    let path = wal_dir.join(filename);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(line.as_bytes()).unwrap();
}

/// Writes garbage bytes to `{db_path}.wal` to simulate a torn WAL tail.
fn corrupt_lbug_wal(db_path: &str) {
    let wal_path = format!("{}.wal", db_path);
    std::fs::write(&wal_path, b"CORRUPT_WAL_TAIL_NOT_VALID_LBUG_WAL").unwrap();
}

// ── FR-012 (a): run_full_recovery_sequence directly ──────────────────────────

/// FR-012 (a): torn lbug WAL → run_full_recovery_sequence → healthy DB.
///
/// Setup: DB opened + schema init'd (lbug creates .wal); WAL JSONL files with
/// 1 episode; episode replayed into DB; lbug WAL corrupted; recovery runs.
/// Expect: Ok((db, report)) with episodes_after=1, indexes_rebuilt=true.
#[tokio::test]
async fn test_run_full_recovery_sequence_torn_wal() {
    let dir = TempDir::new().unwrap();
    let wal_dir = dir.path().join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();

    let ep_uuid = "ep-recovery-test-001";
    let (db, db_path) = make_db(&dir);

    // Write JSONL WAL file with the episode
    write_episode_wal_line(&wal_dir, "0001.jsonl", 1, ep_uuid);

    // Replay WAL into DB so the episode exists at the checkpoint
    {
        let conn = db.connect().unwrap();
        WalReplayer::new(&wal_dir).replay(&conn).unwrap();
    }
    drop(db);

    // Corrupt the lbug WAL to simulate a torn write
    corrupt_lbug_wal(&db_path);

    // Run the full recovery sequence directly (as startup would)
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let result = tokio::task::spawn_blocking({
        let db_path = db_path.clone();
        let wal_dir = wal_dir.clone();
        move || recovery::run_full_recovery_sequence(&db_path, &wal_dir, DIM, sink)
    })
    .await
    .unwrap();

    let (recovered_db, report) = result.expect("run_full_recovery_sequence should succeed");

    // Episode should be present after recovery
    let conn = recovered_db.connect().unwrap();
    let episode_count = conn.count_nodes("Episodic").unwrap();
    assert_eq!(
        episode_count, 1,
        "Should have 1 episode after recovery, got {episode_count}"
    );
    assert!(report.indexes_rebuilt, "indexes_rebuilt should be true");
    assert_eq!(
        report.episodes_after, 1,
        "report.episodes_after should be 1"
    );
    // from_seq derived from the episode in the DB
    assert_eq!(
        report.cursor_reason,
        recovery::CursorReason::UuidMatch,
        "Should have derived cursor via uuid match"
    );

    // Confirm lbug WAL was renamed aside
    assert!(
        !std::path::Path::new(&format!("{}.wal", db_path)).exists(),
        "Corrupt lbug WAL should be renamed after recovery"
    );
}

// ── FR-012 (b): knowledge_recover_full IPC on degraded engine ────────────────

/// FR-012 (b): knowledge_recover_full on a degraded engine (DB=None) performs
/// full recovery and returns structured phase counts.
#[tokio::test]
async fn test_knowledge_recover_full_on_degraded_engine() {
    let dir = TempDir::new().unwrap();
    let wal_dir = dir.path().join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();

    let ep_uuid = "ep-ipc-recovery-001";
    let (db, db_path) = make_db(&dir);

    // Write WAL JSONL and replay into DB
    write_episode_wal_line(&wal_dir, "0001.jsonl", 1, ep_uuid);
    {
        let conn = db.connect().unwrap();
        WalReplayer::new(&wal_dir).replay(&conn).unwrap();
    }
    drop(db);

    // Corrupt lbug WAL
    corrupt_lbug_wal(&db_path);

    // Create degraded AppState (db=None)
    let capture_sink: Arc<CaptureSink> = Arc::new(CaptureSink::new());
    let state = make_degraded_state(
        &db_path,
        wal_dir.clone(),
        Arc::clone(&capture_sink) as Arc<dyn TelemetrySink>,
    );

    // Issue knowledge_recover_full
    let resp = dispatch(1, "knowledge_recover_full", json!({}), Arc::clone(&state)).await;

    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(resp.get("result").is_some(), "Expected result: {resp}");
    let result = &resp["result"];
    assert_eq!(result["success"], true, "Recovery should succeed: {result}");
    assert_eq!(result["recovery_needed"], true);
    assert_eq!(result["episodes_after"], 1);
    assert_eq!(result["indexes_rebuilt"], true);

    // Engine should now be healthy
    assert!(
        state.db.load_full().is_some(),
        "DB should be Some after recovery"
    );
    assert!(
        state.degraded_reason.lock().unwrap().is_none(),
        "degraded_reason should be cleared"
    );

    // Recovery telemetry should have been emitted
    let events = capture_sink.events();
    let has_healthy = events.iter().any(|e| {
        matches!(
            e,
            liminis_graph_core::TelemetryEvent::ServiceState { state, .. } if state == "healthy"
        )
    });
    assert!(
        has_healthy,
        "Should have emitted ServiceState{{state: healthy}}. Events: {events:?}"
    );
}

// ── FR-012 (b) idempotency ───────────────────────────────────────────────────

/// FR-012 (b) idempotency: knowledge_recover_full on a healthy engine returns
/// recovery_needed=false and mutations_replayed=0 without changing the DB.
#[tokio::test]
async fn test_knowledge_recover_full_idempotent_on_healthy_engine() {
    let dir = TempDir::new().unwrap();
    let wal_dir = dir.path().join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();

    let ep_uuid = "ep-idempotent-001";
    let (db, _db_path) = make_db(&dir);

    // Insert episode by replaying a WAL line (avoids raw_query visibility issue)
    write_episode_wal_line(&wal_dir, "0001.jsonl", 1, ep_uuid);
    {
        let conn = db.connect().unwrap();
        WalReplayer::new(&wal_dir).replay(&conn).unwrap();
    }

    let state = make_healthy_state(Arc::new(db), wal_dir);

    // Episodes before
    let db_snap = state.db.load_full().unwrap();
    let conn = db_snap.connect().unwrap();
    let episodes_before = conn.count_nodes("Episodic").unwrap();
    drop(conn);
    drop(db_snap);

    // Call knowledge_recover_full on healthy engine
    let resp = dispatch(1, "knowledge_recover_full", json!({}), Arc::clone(&state)).await;

    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(resp.get("result").is_some(), "Expected result: {resp}");
    let result = &resp["result"];
    assert_eq!(result["success"], true);
    assert_eq!(
        result["recovery_needed"], false,
        "Should be no-op: {result}"
    );
    assert_eq!(result["mutations_replayed"], 0);
    assert_eq!(result["indexes_rebuilt"], false);
    assert_eq!(result["episodes_before"], episodes_before as i64);
    assert_eq!(result["episodes_after"], episodes_before as i64);
}

// ── FR-012 (f): fallback to full rebuild when drop_lbug_wal fails ────────────

/// FR-012 (f): when drop_lbug_wal fails (main DB corrupt/missing), recovery
/// falls back to full rebuild from WAL and still produces a healthy engine.
#[tokio::test]
async fn test_knowledge_recover_full_fallback_on_corrupt_db() {
    let dir = TempDir::new().unwrap();
    let wal_dir = dir.path().join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();

    // Write WAL JSONL with 1 episode — this is the source of truth
    let ep_uuid = "ep-fallback-001";
    write_episode_wal_line(&wal_dir, "0001.jsonl", 1, ep_uuid);

    // Use a db_path whose parent exists but whose DB file does NOT exist.
    // Db::open on a non-existent path would normally create a new DB — but if
    // we put a directory at that path, Db::open must fail, triggering the fallback.
    let db_path = dir.path().join("corrupt.db");
    std::fs::create_dir_all(&db_path).unwrap(); // make it a directory, not a file
    let db_path_str = db_path.to_str().unwrap().to_string();

    // Create a "corrupt WAL" alongside it
    let wal_path = format!("{}.wal", db_path_str);
    std::fs::write(&wal_path, b"CORRUPT").unwrap();

    // Create degraded AppState with no DB
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let state = make_degraded_state(&db_path_str, wal_dir.clone(), Arc::clone(&sink));

    // Remove the directory so Db::open can create a file there (fallback needs to rebuild)
    // Actually, keep the directory to force Db::open to fail → fallback triggers
    // The fallback full_rebuild will call remove_dir_all(&db_path) first, then create fresh DB

    let resp = dispatch(1, "knowledge_recover_full", json!({}), Arc::clone(&state)).await;

    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(resp.get("result").is_some(), "Expected result: {resp}");
    let result = &resp["result"];
    assert_eq!(
        result["success"], true,
        "Fallback recovery should succeed: {result}"
    );
    assert_eq!(result["recovery_needed"], true);
    // After full rebuild from WAL, the episode should be present
    assert_eq!(
        result["episodes_after"], 1,
        "Should have 1 episode after fallback rebuild: {result}"
    );
    assert_eq!(result["indexes_rebuilt"], true);

    // Engine should be healthy
    assert!(
        state.db.load_full().is_some(),
        "DB should be Some after fallback recovery"
    );
}
