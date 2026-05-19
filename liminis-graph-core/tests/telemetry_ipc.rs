// Telemetry integration tests (T012).
//
// Injects a CaptureSink into dispatch() and verifies that IpcCall events
// are emitted with the correct shape for each IPC call, covering acceptance
// scenario 1: timing events emitted per IPC call with field shapes matching
// docs/telemetry.md.

use std::sync::Arc;

use liminis_graph_core::{
    db::Db,
    embedder::Embedder,
    extractor::Extractor,
    handlers,
    ipc::IpcRequest,
    telemetry::{CaptureSink, NoopSink, TelemetryEvent},
};
use serde_json::json;
use tempfile::TempDir;

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
    let embedder = Arc::new(Embedder::from_env());
    let extractor = Arc::new(Extractor::from_env(Arc::new(NoopSink)));
    let sink = Arc::new(CaptureSink::new());

    let _resp = handlers::dispatch(
        req(1, "knowledge_build_indices", json!({})),
        db,
        embedder,
        extractor,
        Arc::clone(&sink) as Arc<dyn liminis_graph_core::TelemetrySink>,
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
    let embedder = Arc::new(Embedder::from_env());
    let extractor = Arc::new(Extractor::from_env(Arc::new(NoopSink)));
    let sink = Arc::new(CaptureSink::new());

    // knowledge_close always succeeds; use an unknown method to get success=false
    let _resp = handlers::dispatch(
        req(2, "no_such_method", json!({})),
        db,
        embedder,
        extractor,
        Arc::clone(&sink) as Arc<dyn liminis_graph_core::TelemetrySink>,
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
