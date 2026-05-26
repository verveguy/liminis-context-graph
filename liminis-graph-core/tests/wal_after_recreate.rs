// Regression test for issue #100: WAL not written after Recreate.
//
// After knowledge_clear_all (preserve_wal: false), the WalWriter must be
// re-initialized so that post-Recreate writes are captured in the WAL.
// Prior to the fix, handle_clear_all took the writer to None and never
// restored it, silently disabling WAL writes for the rest of the session.

use std::collections::HashMap;
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
    telemetry::{NoopSink, TelemetrySink},
    WalWriter,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const EMB_DIM: usize = 4;

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

fn has_wal_files(dir: &std::path::Path) -> bool {
    if !dir.exists() {
        return false;
    }
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        })
        .unwrap_or(false)
}

fn wal_byte_total(dir: &std::path::Path) -> u64 {
    if !dir.exists() {
        return 0;
    }
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

// FR-001, FR-002, FR-003, SC-001, SC-002, SC-004
//
// Sequence: start service with WAL → Recreate (preserve_wal: false) → write →
// assert WAL re-populated.
#[tokio::test]
async fn wal_repopulated_after_recreate() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let wal_path = tmp.path().join("wal");

    // Open once, init schema, drop the Conn (not the Db). handle_clear_all deletes
    // the file and creates a fresh one; the old Arc<Db> becomes stale but valid on
    // macOS (unlinked file), and state.db is atomically replaced by the handler.
    let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(EMB_DIM).unwrap();
    }

    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = WalWriter::new(&wal_path, 10_000, 5 * 1024 * 1024).ok();

    let state = Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(EMB_DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_path.to_str().unwrap().to_string(),
        wal_dir: Some(wal_path.clone()),
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
    });

    // Step 1: Recreate (preserve_wal: false) — clears DB and WAL directory.
    let clear_resp = dispatch(
        1,
        "knowledge_clear_all",
        json!({"confirm": true, "preserve_wal": false}),
        Arc::clone(&state),
    )
    .await;
    assert!(
        clear_resp.get("result").is_some(),
        "knowledge_clear_all must succeed: {clear_resp}"
    );
    assert_eq!(
        clear_resp["result"]["success"], true,
        "clear_all success must be true: {clear_resp}"
    );

    // FR-001: WalWriter must be re-initialized (not None) after Recreate.
    assert!(
        state.wal_writer.lock().unwrap().is_some(),
        "WalWriter must be re-initialized after Recreate (FR-001)"
    );

    // WAL directory must exist (created by WalWriter::new during re-init).
    assert!(
        wal_path.exists(),
        "WAL directory must exist after Recreate re-init"
    );

    // Step 2: Ingest an episode — goes through wal_flush_chunk.
    // Uses MockExtractor so no real LLM is needed.
    let write_resp = dispatch(
        2,
        "knowledge_add_episode",
        json!({
            "name": "postclear-regression-chunk",
            "episode_body": "Alice works at Acme Corp.",
            "source": "test",
            "source_description": "test/wal_after_recreate",
            "reference_time": "2026-01-01 00:00:00",
            "group_id": "wal_after_recreate_test"
        }),
        Arc::clone(&state),
    )
    .await;
    assert!(
        write_resp.get("result").is_some(),
        "knowledge_add_episode must succeed after Recreate: {write_resp}"
    );

    // SC-001 / FR-002: WAL directory must contain JSONL file(s) with content.
    assert!(
        has_wal_files(&wal_path),
        "WAL directory must contain at least one JSONL file after post-Recreate write (SC-001)"
    );
    let total_bytes = wal_byte_total(&wal_path);
    assert!(
        total_bytes > 0,
        "WAL files must be non-empty after post-Recreate write (FR-002)"
    );

    // SC-002: knowledge_status must report wal.exists=true and wal.byte_size>0.
    let status_resp = dispatch(
        3,
        "knowledge_status",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert!(
        status_resp.get("result").is_some(),
        "knowledge_status must succeed: {status_resp}"
    );
    assert_eq!(
        status_resp["result"]["wal"]["exists"], true,
        "knowledge_status.wal.exists must be true after post-Recreate write (SC-002): {status_resp}"
    );
    let wal_byte_size = status_resp["result"]["wal"]["byte_size"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        wal_byte_size > 0,
        "knowledge_status.wal.byte_size must be >0 after post-Recreate write (SC-002): {status_resp}"
    );
}
