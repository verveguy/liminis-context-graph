// FR-009: binary enters degraded mode when lbug WAL is corrupt
// FR-010: recovery from degraded mode via drop_lbug_wal strategy

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
    telemetry::{CaptureSink, NoopSink, TelemetryEvent, TelemetrySink},
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;

// ── helpers ───────────────────────────────────────────────────────────────────

fn req(id: i64, method: &str, params: Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    }
}

fn make_degraded_state_with_capture(
    reason: &str,
    db_path: String,
    sink: Arc<CaptureSink>,
) -> Arc<AppState> {
    Arc::new(AppState {
        db: ArcSwapOption::from(None),
        degraded_reason: Arc::new(Mutex::new(Some(reason.to_string()))),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path,
        wal_dir: None,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
    })
}

async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

// ── FR-009: Degraded mode detection ──────────────────────────────────────────

/// FR-009: With a corrupt WAL, Db::open returns an error containing "Corrupted wal file".
/// The binary classifies this as recoverable and enters degraded mode.
/// health_check returns {ok: false, healthy: false, state: "degraded"}.
#[tokio::test]
async fn test_degraded_mode_from_corrupt_wal() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let wal_path = format!("{}.wal", db_path);

    // Write garbage bytes to the WAL file to simulate corruption
    std::fs::write(&wal_path, b"CORRUPT_DATA_NOT_A_WAL_FILE").unwrap();

    // Attempt to open the DB — lbug should reject the corrupt WAL
    let open_result = Db::open(&db_path);
    assert!(
        open_result.is_err(),
        "Expected Db::open to fail with corrupt WAL"
    );
    let err_msg = match open_result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("Expected error but got Ok"),
    };
    // The error should indicate WAL corruption
    assert!(
        err_msg.contains("Corrupted wal file")
            || err_msg.contains("corrupt")
            || err_msg.contains("WAL")
            || err_msg.contains("wal"),
        "Expected WAL corruption error, got: {err_msg}"
    );

    // Simulate what main.rs does: classify as recoverable, create degraded AppState
    let sink: Arc<CaptureSink> = Arc::new(CaptureSink::new());
    let reason = "lbug_wal_corrupt".to_string();
    let state = make_degraded_state_with_capture(&reason, db_path.clone(), Arc::clone(&sink));

    // Verify the state is actually degraded
    assert!(
        state.db.load_full().is_none(),
        "DB should be None in degraded state"
    );
    assert_eq!(
        state.degraded_reason.lock().unwrap().as_deref(),
        Some("lbug_wal_corrupt")
    );

    // health_check should return {ok: false, state: "degraded"} — NOT a JSON-RPC error
    let resp = dispatch_val(1, "health_check", json!({}), Arc::clone(&state)).await;
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(
        resp.get("result").is_some(),
        "health_check should return a result (not error) in degraded mode"
    );
    assert_eq!(resp["result"]["ok"], false);
    assert_eq!(resp["result"]["healthy"], false);
    assert_eq!(resp["result"]["state"], "degraded");
    assert_eq!(resp["result"]["reason"], "lbug_wal_corrupt");

    // knowledge_status should also work and include recovery options
    let status_resp = dispatch_val(2, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_eq!(status_resp["jsonrpc"], "2.0");
    assert!(status_resp.get("result").is_some());
    assert_eq!(status_resp["result"]["degraded"], true);
    assert_eq!(status_resp["result"]["running"], true);
    assert_eq!(status_resp["result"]["context_graph_initialized"], false);

    // Any other method should return -32001 (DB unavailable)
    let entity_resp = dispatch_val(
        3,
        "knowledge_find_entities",
        json!({"query": "test"}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(entity_resp["jsonrpc"], "2.0");
    assert!(
        entity_resp.get("error").is_some(),
        "Should return error when degraded"
    );
    assert_eq!(entity_resp["error"]["code"], -32001);
    assert_eq!(entity_resp["error"]["data"]["reason"], "lbug_wal_corrupt");
}

// ── FR-010: Recovery from degraded mode ──────────────────────────────────────

/// FR-010: Starting from degraded state with a corrupt WAL:
/// (a) calling knowledge_recover with drop_lbug_wal renames the corrupt file
/// (b) state.db becomes Some after recovery
/// (c) telemetry sink captures ServiceState{state: "healthy"}
/// (d) subsequent health_check returns {ok: true}
#[tokio::test]
async fn test_recovery_drop_lbug_wal() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("recovery.db").to_str().unwrap().to_string();
    let wal_path = format!("{}.wal", db_path);

    // Write garbage bytes to simulate a corrupt WAL
    std::fs::write(&wal_path, b"CORRUPT_WAL_DATA").unwrap();

    // Verify the WAL file exists before recovery
    assert!(
        std::path::Path::new(&wal_path).exists(),
        "WAL file should exist before recovery"
    );

    // Create degraded state with CaptureSink for telemetry verification
    let sink: Arc<CaptureSink> = Arc::new(CaptureSink::new());
    let state =
        make_degraded_state_with_capture("lbug_wal_corrupt", db_path.clone(), Arc::clone(&sink));

    // Call knowledge_recover with drop_lbug_wal strategy
    let recover_resp = dispatch_val(
        10,
        "knowledge_recover",
        json!({"strategy": "drop_lbug_wal"}),
        Arc::clone(&state),
    )
    .await;

    // The response should be a success result
    assert_eq!(recover_resp["jsonrpc"], "2.0");
    assert!(
        recover_resp.get("result").is_some(),
        "knowledge_recover should return a result: {recover_resp}"
    );
    let result = &recover_resp["result"];
    assert_eq!(result["strategy"], "drop_lbug_wal");
    assert_eq!(result["success"], true, "Recovery should succeed: {result}");

    // (a) The original .wal file should be renamed (not exist at original path)
    assert!(
        !std::path::Path::new(&wal_path).exists(),
        "Original WAL file should be renamed after drop_lbug_wal"
    );

    // A .wal.corrupt-* file should exist in the same directory
    let corrupt_files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".wal.corrupt-"))
        .collect();
    assert!(
        !corrupt_files.is_empty(),
        "A .wal.corrupt-* file should exist after recovery"
    );

    // (b) state.db should now be Some after successful recovery
    assert!(
        state.db.load_full().is_some(),
        "DB should be Some after successful recovery"
    );

    // (c) Telemetry sink should have captured ServiceState{state: "healthy"}
    let events = sink.events();
    let has_healthy_event = events.iter().any(|e| {
        matches!(
            e,
            TelemetryEvent::ServiceState {
                state,
                ..
            } if state == "healthy"
        )
    });
    assert!(
        has_healthy_event,
        "Should have emitted ServiceState{{state: healthy}} event. Events: {events:?}"
    );

    // (d) Subsequent health_check should return {ok: true}
    let health_resp = dispatch_val(11, "health_check", json!({}), Arc::clone(&state)).await;
    assert_eq!(health_resp["jsonrpc"], "2.0");
    assert!(
        health_resp.get("result").is_some(),
        "health_check should return result"
    );
    assert_eq!(
        health_resp["result"]["ok"], true,
        "health_check should return ok: true after recovery"
    );
    assert_eq!(health_resp["result"]["state"], "healthy");

    // degraded_reason should be cleared
    assert!(
        state.degraded_reason.lock().unwrap().is_none(),
        "degraded_reason should be cleared after recovery"
    );
}

/// Tests that knowledge_recover with an unknown strategy returns an error.
#[tokio::test]
async fn test_recovery_unknown_strategy() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        db: ArcSwapOption::from(None),
        degraded_reason: Arc::new(Mutex::new(Some("lbug_wal_corrupt".to_string()))),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path,
        wal_dir: None,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
    });

    let resp = dispatch_val(
        1,
        "knowledge_recover",
        json!({"strategy": "not_a_valid_strategy"}),
        Arc::clone(&state),
    )
    .await;

    // Should return a JSON-RPC error (unknown strategy)
    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(
        resp.get("error").is_some(),
        "Unknown strategy should return error"
    );
}

/// Tests that knowledge_recover without a strategy returns an error.
#[tokio::test]
async fn test_recovery_missing_strategy() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        db: ArcSwapOption::from(None),
        degraded_reason: Arc::new(Mutex::new(Some("lbug_wal_corrupt".to_string()))),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path,
        wal_dir: None,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
    });

    let resp = dispatch_val(1, "knowledge_recover", json!({}), Arc::clone(&state)).await;

    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(
        resp.get("error").is_some(),
        "Missing strategy should return error"
    );
}
