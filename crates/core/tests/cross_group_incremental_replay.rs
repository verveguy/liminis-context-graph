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
    EntityRow, DEFAULT_GROUP_ID,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const GROUP_LAYER: &str = "layer-l";
const GROUP_LAYER_2: &str = "layer-l2";
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

/// A standalone `Entity` WAL line for group B's content, param-bound the same way
/// `handlers_wal_admin.rs`'s fixtures are. `name` lets a test seed more than one distinct
/// entity for B to resolve foreign pointers against (issue #385's multi-owning-group coverage).
fn named_wal_line(seq: u64, uuid: &str, name: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {{uuid: '{uuid}'}}) ON CREATE SET n.name = '{name}', n.group_id = '{GROUP_B}', n.labels = ['t'], n.created_at = timestamp('2026-05-22 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{{}}'","params":{{}}}}"#
    )
}

fn widget_wal_line(seq: u64, uuid: &str) -> String {
    named_wal_line(seq, uuid, "widget")
}

/// Reads every WAL line's `cypher` field under `wal_root/group_id`, in file-name order — issue
/// #385's assertions need to know *which* mutations landed in which group's stream, not just
/// how many. Returns an empty vec (not an error) when the group's directory doesn't exist —
/// several assertions rely on that "no directory" case directly.
fn read_wal_cyphers(wal_root: &std::path::Path, group_id: &str) -> Vec<String> {
    let dir = wal_root.join(group_id);
    if !dir.exists() {
        return Vec::new();
    }
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        for line in std::fs::read_to_string(&f).unwrap().lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line).unwrap();
            out.push(v["cypher"].as_str().unwrap_or_default().to_string());
        }
    }
    out
}

/// Snapshots `wal_root/group_id`'s `.jsonl` files as `(file_name, exact_bytes)` pairs, for a
/// byte-identical before/after comparison (SC-002: the source group's own stream must be
/// untouched by a `rebind_pointers` call that only names it as `source_group_id`).
fn snapshot_group_wal_dir(
    wal_root: &std::path::Path,
    group_id: &str,
) -> Option<Vec<(String, Vec<u8>)>> {
    let dir = wal_root.join(group_id);
    if !dir.exists() {
        return None;
    }
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    Some(
        files
            .into_iter()
            .map(|f| {
                let name = f.file_name().unwrap().to_string_lossy().to_string();
                let bytes = std::fs::read(&f).unwrap();
                (name, bytes)
            })
            .collect(),
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
        conn.set_wal_position(GROUP_LAYER, 999, None).unwrap();
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

    // Group L's own applied position is set to a deliberately different, larger value than
    // anything group B will reach — proves the rebind below keys off B's position, not L's (a
    // bug that compared against the wrong group's position would still "work" here only by
    // accident if it happened to use a smaller number). Set *after* the dispatch above rather
    // than before it (issue #383): that call now legitimately advances GROUP_LAYER's own
    // applied_seq via `wal_flush_ungrouped`, so seeding the sentinel any earlier would just be
    // overwritten by the real value the fix now produces, rather than surviving as a no-op the
    // way it did while the bug this issue fixes was still present.
    {
        let db1 = state.db.load_full().unwrap();
        let conn = db1.connect().unwrap();
        conn.set_wal_position(GROUP_LAYER, 999, None).unwrap();
    }

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
            conn.get_wal_position(GROUP_LAYER).unwrap().applied_seq,
            conn.get_wal_position(GROUP_B).unwrap().applied_seq,
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

/// Issue #383 User Story 2 / SC-003: a target group built *entirely* through the assertion API
/// (`knowledge_assert_entity`, no episode ingest, no raw WAL replay at all) must still make
/// FR-011's re-bind staleness gate fire when it receives further assert-API content — this is
/// the motivating correctness case for the issue, not just `knowledge_status`'s cosmetic
/// `applied_seq` reporting (that's `assert.rs`'s `knowledge_status_reflects_assert_only_writes`).
/// Before this fix, `wal_flush_ungrouped` never advanced `applied_seq`, so a target group's
/// position would stay permanently `null` no matter how much assert-API content it received —
/// the staleness gate would never observe an advance and would never fire.
#[tokio::test]
async fn rebind_staleness_gate_fires_for_target_group_built_entirely_via_assert_api() {
    const GROUP_LAYER2: &str = "layer-l2";
    const GROUP_TARGET: &str = "target-g";

    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();
    let state = make_state_with_wal(db, wal_root);

    // Source entity, itself created purely via the assert API.
    let hub_v = dispatch_val(
        1,
        "knowledge_assert_entity",
        json!({"name": "layer-hub", "group_id": GROUP_LAYER2}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&hub_v, 1);
    let hub_uuid = hub_v["result"]["entity_uuid"].as_str().unwrap().to_string();

    // Cross-group pointer into GROUP_TARGET, created before that group has any content at all —
    // Foreign-by-name, starts Unbound.
    let add_v = dispatch_val(
        2,
        "knowledge_add_cross_group_edge",
        json!({
            "name": "REFERENCES",
            "group_id": GROUP_LAYER2,
            "source": {"uuid": hub_uuid},
            "target": {"source_group_id": GROUP_TARGET, "endpoint_name": "widget"},
            "fact": "layer-hub references widget",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&add_v, 2);
    let edge_uuid = add_v["result"]["uuid"].as_str().unwrap().to_string();
    assert_eq!(
        add_v["result"]["cross_group_pointers"]["dst"]["binding_state"], "unbound",
        "target must be unresolved before group target has any content: {add_v}"
    );

    // GROUP_TARGET has received no writes of its own yet — its applied_seq must be null (SC-001's
    // reproduction baseline), confirming the staleness gate has nothing to observe yet.
    let status_before = dispatch_val(3, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&status_before, 3);
    assert!(
        status_before["result"]["wal_groups"]
            .get(GROUP_TARGET)
            .is_none()
            || status_before["result"]["wal_groups"][GROUP_TARGET]["applied_seq"].is_null(),
        "group target must report no applied_seq before it has received any writes: \
         {status_before}"
    );

    // Further content asserted into the target group — entirely through the assert API, the
    // pattern issue #379 exists to serve.
    let widget_v = dispatch_val(
        4,
        "knowledge_assert_entity",
        json!({"name": "widget", "group_id": GROUP_TARGET}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&widget_v, 4);
    let widget_uuid = widget_v["result"]["entity_uuid"]
        .as_str()
        .unwrap()
        .to_string();

    // SC-001: group target's applied_seq must now be non-null and consistent with its content —
    // this is the fix itself, exercised on the group the FR-011 gate is about to read from.
    let target_applied_seq = {
        let db2 = state.db.load_full().unwrap();
        let conn = db2.connect().unwrap();
        conn.get_wal_position(GROUP_TARGET).unwrap().applied_seq
    };
    assert!(
        target_applied_seq.is_some(),
        "group target's applied_seq must have advanced after an assert-API write, not stay null"
    );

    // SC-003: the FR-011 staleness gate must observe group target's advance and fire, resolving
    // the previously-unbound pointer.
    let rebind_v = dispatch_val(
        5,
        "knowledge_rebind_pointers",
        json!({"source_group_id": GROUP_TARGET}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rebind_v, 5);
    assert_eq!(
        rebind_v["result"]["checked"], 1,
        "the staleness gate must have observed group target's advanced applied_seq and \
         checked the pointer: {rebind_v}"
    );
    assert_eq!(rebind_v["result"]["bound"], 1, "{rebind_v}");

    let (final_target, final_attrs) = {
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
        final_target, widget_uuid,
        "the pointer must resolve to the entity created via the assert API"
    );
    let dst_ptr = pointer::read_pointers(&final_attrs)
        .get(EndpointSide::Dst)
        .cloned()
        .unwrap();
    assert_eq!(dst_ptr.binding_state, BindingState::Bound);
    assert_eq!(dst_ptr.bound_at_seq, target_applied_seq);
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

// ── Issue #385: knowledge_rebind_pointers WAL attribution (User Story 2) ────────────────────
//
// A standalone `knowledge_rebind_pointers(source_group_id: ...)` call resolves pointers whose
// *source* is `source_group_id`, but the mutations it issues land on the edges owned by other
// ("owning") groups. Before #385 all of this was routed through the default group's ("liminis")
// WAL stream regardless of who owns what; after #385 each mutation lands in the owning group's
// own stream.

/// US2 AC1/AC2, SC-002: a standalone rebind call routes its mutations to the edge's owning
/// group (the layer group L), never to `source_group_id` (B) merely because it was named in the
/// call, and never to the default group.
#[tokio::test]
async fn standalone_rebind_pointers_routes_to_owning_group_not_source_or_default() {
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

    // L's edge starts Unbound: B has no content yet.
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

    // B receives its content via incremental replay — its own stream now has exactly this
    // creation line, captured below as the "before" snapshot for the byte-identical check.
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
    let b_snapshot_before_rebind = snapshot_group_wal_dir(&wal_root, GROUP_B);

    // The standalone rebind call — real IPC dispatch, exercising handle_rebind_pointers's own
    // WAL-flush loop, not the direct cross_group::rebind_pointers entry point.
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

    assert!(
        !wal_root.join(DEFAULT_GROUP_ID).exists(),
        "a standalone rebind_pointers call must never create the default group's WAL directory"
    );

    let layer_cyphers = read_wal_cyphers(&wal_root, GROUP_LAYER);
    assert!(
        layer_cyphers.iter().any(|c| c.contains("rn.attributes")),
        "the owning group L's stream must contain the pointer's attribute rewrite: \
         {layer_cyphers:?}"
    );
    assert!(
        layer_cyphers
            .iter()
            .any(|c| c.contains("RelatesToNode_") && c.contains("MERGE")),
        "the owning group L's stream must contain the newly-bound hop's creation: \
         {layer_cyphers:?}"
    );

    let b_snapshot_after_rebind = snapshot_group_wal_dir(&wal_root, GROUP_B);
    assert_eq!(
        b_snapshot_before_rebind, b_snapshot_after_rebind,
        "source_group_id's own stream (B) must be byte-identical before and after a rebind \
         call that only names it as the source, never as an owning group (SC-002)"
    );
}

/// US2 AC3: pointers owned by several distinct groups, all resolving against the same
/// `source_group_id`, are each attributed only to their own owning group's stream — no
/// cross-contamination between owning groups, and none reaching the default group.
#[tokio::test]
async fn standalone_rebind_pointers_attributes_each_owning_group_to_its_own_stream() {
    let (db, _db_dir) = make_db(4);
    let (hub1_uuid, hub2_uuid) = {
        let conn = db.connect().unwrap();
        let hub1 = EntityRow {
            uuid: Uuid::new_v4().to_string(),
            name: "layer1-hub".to_string(),
            group_id: GROUP_LAYER.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-05-22 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "s".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        };
        let hub2 = EntityRow {
            uuid: Uuid::new_v4().to_string(),
            group_id: GROUP_LAYER_2.to_string(),
            name: "layer2-hub".to_string(),
            ..hub1.clone()
        };
        conn.insert_entity(&hub1).unwrap();
        conn.insert_entity(&hub2).unwrap();
        (hub1.uuid, hub2.uuid)
    };

    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();
    let state = make_state_with_wal(db, wal_root.clone());

    // Two distinct owning groups (L1, L2), each with an edge pointing (Unbound) at a different
    // name in the same source group B.
    let add1_v = dispatch_val(
        1,
        "knowledge_add_cross_group_edge",
        json!({
            "name": "REFERENCES",
            "group_id": GROUP_LAYER,
            "source": {"uuid": hub1_uuid},
            "target": {"source_group_id": GROUP_B, "endpoint_name": "widget"},
            "fact": "layer1-hub references widget",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&add1_v, 1);
    let add2_v = dispatch_val(
        2,
        "knowledge_add_cross_group_edge",
        json!({
            "name": "REFERENCES",
            "group_id": GROUP_LAYER_2,
            "source": {"uuid": hub2_uuid},
            "target": {"source_group_id": GROUP_B, "endpoint_name": "gadget"},
            "fact": "layer2-hub references gadget",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&add2_v, 2);

    // B receives both names in one incremental replay.
    let widget_uuid = Uuid::new_v4().to_string();
    let gadget_uuid = Uuid::new_v4().to_string();
    let group_b_dir = wal_root.join(GROUP_B);
    std::fs::create_dir_all(&group_b_dir).unwrap();
    std::fs::write(
        group_b_dir.join("20260522_000000_aaaaaa_0000.jsonl"),
        named_wal_line(0, &widget_uuid, "widget")
            + "\n"
            + &named_wal_line(1, &gadget_uuid, "gadget")
            + "\n",
    )
    .unwrap();
    let rebuild_v = dispatch_rebuild_sync(
        3,
        json!({"group_id": GROUP_B, "from_seq": 0}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rebuild_v, 3);

    let rebind_v = dispatch_val(
        4,
        "knowledge_rebind_pointers",
        json!({"source_group_id": GROUP_B}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rebind_v, 4);
    assert_eq!(rebind_v["result"]["checked"], 2, "{rebind_v}");
    assert_eq!(rebind_v["result"]["bound"], 2, "{rebind_v}");

    assert!(
        !wal_root.join(DEFAULT_GROUP_ID).exists(),
        "a multi-owning-group rebind call must never create the default group's WAL directory"
    );

    let layer1_cyphers = read_wal_cyphers(&wal_root, GROUP_LAYER);
    assert!(
        layer1_cyphers.iter().any(|c| c.contains("rn.attributes")),
        "L1's own stream must contain its edge's attribute rewrite: {layer1_cyphers:?}"
    );
    let layer2_cyphers = read_wal_cyphers(&wal_root, GROUP_LAYER_2);
    assert!(
        layer2_cyphers.iter().any(|c| c.contains("rn.attributes")),
        "L2's own stream must contain its edge's attribute rewrite: {layer2_cyphers:?}"
    );
}

/// Edge Cases: `rebind_pointers` resolving zero unbound pointers produces no mutations, so no
/// group's WAL stream — including the default group's — gains a new directory as a side effect.
#[tokio::test]
async fn standalone_rebind_pointers_with_nothing_to_resolve_creates_no_wal_directory() {
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();
    let state = make_state_with_wal(db, wal_root.clone());

    let rebind_v = dispatch_val(
        1,
        "knowledge_rebind_pointers",
        json!({"source_group_id": "no-such-group"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rebind_v, 1);
    assert_eq!(rebind_v["result"]["checked"], 0, "{rebind_v}");

    assert!(
        !wal_root.exists() || std::fs::read_dir(&wal_root).unwrap().next().is_none(),
        "a rebind call that resolves nothing must not create any group's WAL directory, \
         including the default group's"
    );
}
