// User Story 4 / SC-006 (issue #378): a cross-group pointer from a "layer" group L into a
// content group B must resolve correctly after B is *incrementally replayed* (via
// `knowledge_rebuild_from_wal`, not a purge-and-rehydrate) — and the staleness gate that drives
// re-binding must use B's own applied position, not any other group's (FR-011).
//
// Unlike `cross_group_pointers.rs` (which drives `cross_group::rebind_pointers` directly against
// a `Conn` to test the resolution/self-loop/duplicate logic in isolation), this file goes through
// real IPC dispatch for the WAL replay step (`knowledge_rebuild_from_wal`) and the rebind step
// (`knowledge_rebind_pointers`), so it exercises the same per-group WAL directory resolution and
// `AppState` plumbing issue #378 introduces end to end.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    cross_group,
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    pointer::{self, BindingState, EndpointSide},
    telemetry::{NoopSink, TelemetrySink},
    EntityRow,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const GROUP_LAYER: &str = "layer-l";
const GROUP_B: &str = "group-b";

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("replay_test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }
    (db, dir)
}

fn make_state_with_wal(db: Arc<Db>, wal_root: std::path::PathBuf) -> Arc<AppState> {
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
        wal_root: Some(wal_root),
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
    })
}

/// A standalone `Entity` WAL line for group B's "widget" content, param-bound the same way
/// `handlers_wal_admin.rs`'s fixtures are.
fn widget_wal_line(seq: u64, uuid: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {{uuid: '{uuid}'}}) ON CREATE SET n.name = 'widget', n.group_id = '{GROUP_B}', n.labels = ['t'], n.created_at = timestamp('2026-05-22 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{{}}'","params":{{}}}}"#
    )
}

async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    };
    let resp = handlers::dispatch(req, state, None).await;
    serde_json::to_value(resp).unwrap()
}

/// `knowledge_rebuild_from_wal` without a progress channel returns a `job_id` and replays in the
/// background (fire-and-forget), which this test can't await deterministically. Passing a
/// progress sender (mirroring a real caller that set `_progress_token`) takes the streaming code
/// path instead, which replays synchronously and returns the final stats directly — see
/// `handlers::dispatch`'s `progress_tx` doc comment.
async fn dispatch_rebuild_sync(id: i64, params: Value, state: Arc<AppState>) -> Value {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: "knowledge_rebuild_from_wal".to_string(),
        params,
    };
    let resp = handlers::dispatch(req, state, Some(tx)).await;
    serde_json::to_value(resp).unwrap()
}

fn assert_ok_resp(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc field wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
}

/// SC-006 end to end: a layer edge points (unresolved) into group B by name. B is incrementally
/// replayed from its own WAL directory via `knowledge_rebuild_from_wal` (not purged, not
/// rehydrated from scratch), the rebind pass runs, and the pointer resolves against B's
/// post-replay state — using B's own applied position, not group L's (FR-011, User Story 4 AC2).
#[tokio::test]
async fn cross_group_pointer_resolves_after_target_groups_incremental_replay() {
    let (db, _db_dir) = make_db(4);
    let hub_uuid = Uuid::new_v4().to_string();
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: hub_uuid.clone(),
            name: "layer-hub".to_string(),
            group_id: GROUP_LAYER.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-05-22 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "s".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        // Group L's own applied position is set to a deliberately different, larger value than
        // anything group B will reach — proves the rebind below keys off B's position, not L's
        // (a bug that compared against the wrong group's position would still "work" here only
        // by accident if it happened to use a smaller number).
        conn.set_applied_seq(GROUP_LAYER, 999).unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();
    let state = make_state_with_wal(db, wal_root.clone());

    // The layer edge is created before group B has any content at all — the target endpoint is
    // Foreign-by-name and must come back Unbound (SC-006's starting point).
    let add_v = dispatch_val(
        1,
        "knowledge_add_cross_group_edge",
        json!({
            "name": "REFERENCES",
            "group_id": GROUP_LAYER,
            "source": {"uuid": hub_uuid},
            "target": {"source_group_id": GROUP_B, "endpoint_name": "widget"},
            "fact": "layer-hub references widget",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&add_v, 1);
    let edge_uuid = add_v["result"]["uuid"].as_str().unwrap().to_string();
    assert_eq!(
        add_v["result"]["cross_group_pointers"]["dst"]["binding_state"], "unbound",
        "target must be unresolved before group B has any content: {add_v}"
    );
    assert_eq!(add_v["result"]["target_node_uuid"], "");

    // Group B now receives its first WAL content — a genuinely incremental replay (from_seq: 0
    // against a group that is currently empty in the DB, not a purge-and-rehydrate cycle).
    let widget_uuid = Uuid::new_v4().to_string();
    let group_b_dir = wal_root.join(GROUP_B);
    std::fs::create_dir_all(&group_b_dir).unwrap();
    std::fs::write(
        group_b_dir.join("20260522_000000_aaaaaa_0000.jsonl"),
        widget_wal_line(0, &widget_uuid) + "\n",
    )
    .unwrap();

    let rebuild_v = dispatch_rebuild_sync(
        2,
        json!({"group_id": GROUP_B, "from_seq": 0}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rebuild_v, 2);
    assert_eq!(
        rebuild_v["result"]["success"], true,
        "incremental replay of group B must succeed: {rebuild_v}"
    );

    // Group L's own position is untouched by B's replay (SC-002, re-confirmed here as this
    // test's own setup, not just Task 11's dedicated coverage).
    let (layer_applied, group_b_applied) = {
        let db2 = state.db.load_full().unwrap();
        let conn = db2.connect().unwrap();
        (
            conn.get_applied_seq(GROUP_LAYER).unwrap(),
            conn.get_applied_seq(GROUP_B).unwrap(),
        )
    };
    assert_eq!(
        layer_applied,
        Some(999),
        "group L's position must be untouched by group B's replay"
    );
    assert_eq!(group_b_applied, Some(0));

    // Re-bind pointers sourced from group B. Must resolve against B's now-populated state.
    let rebind_v = dispatch_val(
        3,
        "knowledge_rebind_pointers",
        json!({"source_group_id": GROUP_B}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rebind_v, 3);
    assert_eq!(rebind_v["result"]["checked"], 1, "{rebind_v}");
    assert_eq!(rebind_v["result"]["bound"], 1, "{rebind_v}");

    let (final_hop, final_attrs) = {
        let db3 = state.db.load_full().unwrap();
        let conn = db3.connect().unwrap();
        let edge = conn
            .get_relates_to_by_uuids(std::slice::from_ref(&edge_uuid))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        (edge.target_node_uuid.clone(), edge.attributes.clone())
    };
    assert_eq!(
        final_hop, widget_uuid,
        "the pointer's hop must resolve to the entity created by B's incremental replay"
    );
    let dst_ptr = pointer::read_pointers(&final_attrs)
        .get(EndpointSide::Dst)
        .cloned()
        .unwrap();
    assert_eq!(dst_ptr.binding_state, BindingState::Bound);
    assert_eq!(dst_ptr.resolved_uuid, Some(widget_uuid));
    // The staleness signal recorded on the pointer is group B's own applied position (0), not
    // group L's (999) — the core of FR-011 / User Story 4 AC2.
    assert_eq!(
        dst_ptr.bound_at_seq,
        Some(0),
        "bound_at_seq must reflect group B's own applied position, not any other group's"
    );
}

/// A second rebind pass with no further WAL activity on group B is a true no-op — confirms the
/// staleness gate correctly recognizes it has already caught up to B's current position, using
/// the direct `cross_group::rebind_pointers` entry point (mirrors `cross_group_pointers.rs`'s own
/// idempotency test, here exercised after a real WAL-driven replay rather than a hand-built
/// fixture).
#[tokio::test]
async fn rebind_after_incremental_replay_is_idempotent_on_second_pass() {
    let (db, _db_dir) = make_db(4);
    let hub_uuid = Uuid::new_v4().to_string();
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: hub_uuid.clone(),
            name: "layer-hub".to_string(),
            group_id: GROUP_LAYER.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-05-22 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "s".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();
    let state = make_state_with_wal(db, wal_root.clone());

    let add_v = dispatch_val(
        1,
        "knowledge_add_cross_group_edge",
        json!({
            "name": "REFERENCES",
            "group_id": GROUP_LAYER,
            "source": {"uuid": hub_uuid},
            "target": {"source_group_id": GROUP_B, "endpoint_name": "widget"},
            "fact": "layer-hub references widget",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&add_v, 1);

    let widget_uuid = Uuid::new_v4().to_string();
    let group_b_dir = wal_root.join(GROUP_B);
    std::fs::create_dir_all(&group_b_dir).unwrap();
    std::fs::write(
        group_b_dir.join("20260522_000000_aaaaaa_0000.jsonl"),
        widget_wal_line(0, &widget_uuid) + "\n",
    )
    .unwrap();
    let rebuild_v = dispatch_rebuild_sync(
        2,
        json!({"group_id": GROUP_B, "from_seq": 0}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rebuild_v, 2);

    {
        let db2 = state.db.load_full().unwrap();
        let conn = db2.connect().unwrap();
        let ts = "2026-05-22T00:00:00Z";
        let (first, _) = cross_group::rebind_pointers(&conn, GROUP_B, ts).unwrap();
        assert_eq!(first.checked, 1);
        assert_eq!(first.bound, 1);

        let (second, _) = cross_group::rebind_pointers(&conn, GROUP_B, ts).unwrap();
        assert_eq!(
            second.checked, 0,
            "a second pass with no intervening WAL activity on group B must skip the \
             already-current pointer (FR-009 no-op guarantee): {second:?}"
        );
    }
}
