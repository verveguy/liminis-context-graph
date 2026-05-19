// Telemetry integration tests (T012).
//
// Injects a CaptureSink into dispatch() via AppState and verifies that IpcCall
// events are emitted with the correct shape for each IPC call, covering acceptance
// scenario 1: timing events emitted per IPC call with field shapes matching
// docs/telemetry.md.

use std::sync::Arc;

use liminis_graph_core::{
    app_state::AppState,
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::HttpEmbedder,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    telemetry::{CaptureSink, TelemetryEvent, TelemetrySink},
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::RwLock;

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(
        Db::open(dir.path().join("telemetry_test.db").to_str().unwrap()).unwrap(),
    );
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }
    (db, dir)
}

fn make_state_with_sink(db: Arc<Db>, sink: Arc<dyn TelemetrySink>) -> Arc<AppState> {
    Arc::new(AppState {
        db,
        embedder: Arc::new(HttpEmbedder::from_env()),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
    })
}

fn req(id: i64, method: &str, params: serde_json::Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    }
}

#[tokio::test]
async fn ipc_call_event_emitted_on_successful_dispatch() {
    let (db, _dir) = make_db(8);
    let sink = Arc::new(CaptureSink::new());
    let state = make_state_with_sink(db, Arc::clone(&sink) as Arc<dyn TelemetrySink>);

    let _resp = handlers::dispatch(
        req(1, "knowledge_build_indices", json!({})),
        state,
    )
    .await;

    let events = sink.events();
    assert_eq!(events.len(), 1, "expected exactly one IpcCall event, got: {events:?}");

    match &events[0] {
        TelemetryEvent::IpcCall { method, success, .. } => {
            assert_eq!(method, "knowledge_build_indices");
            assert!(success, "expected success=true");
        }
        other => panic!("expected IpcCall event, got: {other:?}"),
    }
}

#[tokio::test]
async fn ipc_call_event_emitted_on_error_dispatch() {
    let (db, _dir) = make_db(8);
    let sink = Arc::new(CaptureSink::new());
    let state = make_state_with_sink(db, Arc::clone(&sink) as Arc<dyn TelemetrySink>);

    // Use an unknown method to get success=false
    let _resp = handlers::dispatch(
        req(2, "no_such_method", json!({})),
        state,
    )
    .await;

    let events = sink.events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        TelemetryEvent::IpcCall { success, .. } => {
            assert!(!success, "expected success=false for unknown method");
        }
        other => panic!("expected IpcCall event, got: {other:?}"),
    }
}
