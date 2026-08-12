//! Integration tests for group-scoped complete purge (issue #361):
//! `knowledge_delete_by_group` removes an entire group's `Entity`/`Episodic`/`RelatesToNode_`
//! data, without orphaning another group's cross-group pointers.
//!
//! Covers the spec's acceptance scenarios and SC-001–SC-009.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use tokio_util::sync::CancellationToken;

use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    cross_group::{self, CreateCrossGroupEdgeParams, EndpointSpec},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    extractor::MockExtractor,
    group_purge, handlers,
    ipc::IpcRequest,
    pointer::{self, BindingState, EndpointSide},
    telemetry::{NoopSink, TelemetrySink},
    types::EntityRow,
    WalWriter,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use uuid::Uuid;

const DIM: usize = 4;
const TS: &str = "2026-01-01T00:00:00Z";
const GROUP_A: &str = "group-a";
const GROUP_B: &str = "group-b";
const GROUP_LAYER: &str = "layer";

// ── helpers ───────────────────────────────────────────────────────────────────

fn open_db(dir: &TempDir) -> Db {
    let db = Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    db
}

fn make_entity(name: &str, group_id: &str, created_at: &str) -> EntityRow {
    EntityRow {
        uuid: Uuid::new_v4().to_string(),
        name: name.to_string(),
        group_id: group_id.to_string(),
        labels: vec!["Entity".to_string()],
        created_at: created_at.to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: format!("summary of {name}"),
        attributes: "{}".to_string(),
        ..Default::default()
    }
}

fn make_state(db: Arc<Db>, wal_dir: Option<std::path::PathBuf>) -> Arc<AppState> {
    make_state_with_path(db, "test.db".to_string(), wal_dir)
}

/// Like [`make_state`], but with an explicit `db_path` — required for any test that exercises
/// a force-clear-and-reopen path (`knowledge_clear_all`, `knowledge_rebuild_from_wal` with
/// `force_clear: true`), since those reopen `Db::open(&state.db_path)` and swap the result into
/// `state.db` — a mismatched `db_path` would silently reopen/mutate an unrelated file instead
/// of the real test database.
fn make_state_with_path(
    db: Arc<Db>,
    db_path: String,
    wal_dir: Option<std::path::PathBuf>,
) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = wal_dir
        .as_ref()
        .and_then(|d| WalWriter::new(d, 10_000, 0).ok());
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path,
        wal_dir,
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

async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

fn assert_ok(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("result").is_some(), "expected result, got: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
}

fn assert_err(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("error").is_some(), "expected error field: {v}");
}

/// Direct-count entities/episodes/edges for a single group_id, bypassing the IPC layer, for
/// assertions that need per-group (not DB-wide) figures.
fn group_counts(db: &Db, group_id: &str) -> (u64, u64, u64) {
    let conn = db.connect().unwrap();
    let single = [group_id];
    (
        conn.count_entities_by_group_ids(&single).unwrap(),
        conn.count_episodics_by_group_ids(&single).unwrap(),
        conn.count_relates_to_by_group_ids(&single).unwrap(),
    )
}

// ── HNSW/FTS self-maintenance probe (Plan task 1) ───────────────────────────────
//
// Empirically verifies whether lbug's HNSW vector index and FTS index self-maintain on
// DETACH DELETE of an Entity node — never exercised before this issue since nothing in this
// codebase deleted Entity/RelatesToNode_ nodes previously. Finding recorded in
// docs/adr/0361-group-scoped-purge.md.

#[test]
fn hnsw_and_fts_self_maintain_on_entity_delete() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice Probe", GROUP_A, TS);
    conn.insert_entity(&alice).unwrap();

    let fts_before = conn
        .fts_search_entities("Alice Probe", &[GROUP_A], 10)
        .unwrap();
    assert!(
        fts_before.iter().any(|(uuid, _)| uuid == &alice.uuid),
        "FTS should find the entity before delete: {fts_before:?}"
    );
    let vec_before = conn
        .vector_search_entities(&[1.0, 0.0, 0.0, 0.0], &[GROUP_A], 10)
        .unwrap();
    assert!(
        vec_before.iter().any(|(uuid, _)| uuid == &alice.uuid),
        "vector search should find the entity before delete: {vec_before:?}"
    );

    conn.delete_entities_by_group_ids(&[GROUP_A]).unwrap();

    let fts_after = conn
        .fts_search_entities("Alice Probe", &[GROUP_A], 10)
        .unwrap();
    assert!(
        !fts_after.iter().any(|(uuid, _)| uuid == &alice.uuid),
        "FTS index must not return a deleted entity (self-maintains on DETACH DELETE): \
         {fts_after:?}"
    );
    let vec_after = conn
        .vector_search_entities(&[1.0, 0.0, 0.0, 0.0], &[GROUP_A], 10)
        .unwrap();
    assert!(
        !vec_after.iter().any(|(uuid, _)| uuid == &alice.uuid),
        "HNSW index must not return a deleted entity (self-maintains on DETACH DELETE): \
         {vec_after:?}"
    );
}

// ── SC-001 / SC-002 / AC1: purge removes group A, leaves group B untouched ──────────────────

#[tokio::test]
async fn purge_removes_target_group_and_leaves_other_group_untouched() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));

    {
        let conn = db.connect().unwrap();
        let a1 = make_entity("A-One", GROUP_A, TS);
        let a2 = make_entity("A-Two", GROUP_A, TS);
        let b1 = make_entity("B-One", GROUP_B, TS);
        conn.insert_entity(&a1).unwrap();
        conn.insert_entity(&a2).unwrap();
        conn.insert_entity(&b1).unwrap();

        let edge_a = lcg_core::types::RelatesToEdge {
            uuid: Uuid::new_v4().to_string(),
            name: "KNOWS".to_string(),
            source_node_uuid: a1.uuid.clone(),
            target_node_uuid: a2.uuid.clone(),
            group_id: GROUP_A.to_string(),
            fact: "A-One knows A-Two".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            created_at: TS.to_string(),
            valid_at: None,
            invalid_at: None,
            attributes: "{}".to_string(),
            relation_type: None,
            episode_uuids: vec![],
            source_descriptions: vec![],
        };
        conn.insert_relates_to_edge(&edge_a).unwrap();
    }

    let (a_ent_before, _, a_edges_before) = group_counts(&db, GROUP_A);
    let (b_ent_before, b_ep_before, b_edges_before) = group_counts(&db, GROUP_B);
    assert_eq!(a_ent_before, 2);
    assert_eq!(a_edges_before, 1);
    assert_eq!(b_ent_before, 1);

    let state = make_state(Arc::clone(&db), None);
    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 1);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["dry_run"], false, "{v}");
    let groups = v["result"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{v}");
    assert_eq!(groups[0]["group_id"], GROUP_A, "{v}");
    assert_eq!(groups[0]["entities"], 2, "{v}");
    assert_eq!(groups[0]["edges"], 1, "{v}");

    let (a_ent_after, a_ep_after, a_edges_after) = group_counts(&db, GROUP_A);
    assert_eq!(a_ent_after, 0, "group A entities must be gone");
    assert_eq!(a_ep_after, 0, "group A episodes must be gone");
    assert_eq!(a_edges_after, 0, "group A edges must be gone");

    let (b_ent_after, b_ep_after, b_edges_after) = group_counts(&db, GROUP_B);
    assert_eq!(
        b_ent_after, b_ent_before,
        "group B entities must be untouched"
    );
    assert_eq!(
        b_ep_after, b_ep_before,
        "group B episodes must be untouched"
    );
    assert_eq!(
        b_edges_after, b_edges_before,
        "group B edges must be untouched"
    );
}

// ── SC-003: search returns nothing for a purged group, still returns for another ────────────

#[tokio::test]
async fn purge_removes_group_from_search_leaves_other_group_searchable() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    let (a_uuid, b_uuid) = {
        let conn = db.connect().unwrap();
        let a = make_entity("Searchable Alpha", GROUP_A, TS);
        let b = make_entity("Searchable Beta", GROUP_B, TS);
        conn.insert_entity(&a).unwrap();
        conn.insert_entity(&b).unwrap();
        (a.uuid, b.uuid)
    };

    {
        let conn = db.connect().unwrap();
        let before = conn
            .fts_search_entities("Searchable", &[GROUP_A, GROUP_B], 10)
            .unwrap();
        assert_eq!(before.len(), 2, "{before:?}");
    }

    let state = make_state(Arc::clone(&db), None);
    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "confirm": true}),
        state,
    )
    .await;
    assert_ok(&v, 1);

    let conn = db.connect().unwrap();
    let after = conn
        .fts_search_entities("Searchable", &[GROUP_A, GROUP_B], 10)
        .unwrap();
    assert!(
        !after.iter().any(|(uuid, _)| uuid == &a_uuid),
        "group A entity must not appear in search after purge: {after:?}"
    );
    assert!(
        after.iter().any(|(uuid, _)| uuid == &b_uuid),
        "group B entity must still appear in search: {after:?}"
    );
}

// ── AC2 / FR-008 / FR-009 / SC-007 / SC-008: foreign RelatesToNode_ survives, pointer unbound ─

#[tokio::test]
async fn purge_preserves_foreign_relates_to_node_and_leaves_pointer_unbound() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));

    let (alice_uuid, edge_uuid) = {
        let conn = db.connect().unwrap();
        let alice = make_entity("Alice", GROUP_LAYER, TS);
        let bob = make_entity("Bob", GROUP_A, TS);
        conn.insert_entity(&alice).unwrap();
        conn.insert_entity(&bob).unwrap();

        let edge = cross_group::create_cross_group_edge(
            &conn,
            CreateCrossGroupEdgeParams {
                name: "KNOWS".to_string(),
                source: EndpointSpec::Uuid(alice.uuid.clone()),
                target: EndpointSpec::Foreign {
                    source_group_id: GROUP_A.to_string(),
                    endpoint_name: "Bob".to_string(),
                },
                group_id: GROUP_LAYER.to_string(),
                fact: "Alice knows Bob".to_string(),
                fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
                valid_at: None,
                relation_type: None,
            },
            TS,
        )
        .unwrap();
        assert_eq!(
            pointer::read_pointers(&edge.attributes)
                .get(EndpointSide::Dst)
                .unwrap()
                .binding_state,
            BindingState::Bound
        );
        (alice.uuid, edge.uuid)
    };

    let state = make_state(Arc::clone(&db), None);

    // Sanity: knowledge_status reports the pointer as bound before the purge (FR-010 baseline).
    let status_before = dispatch_val(1, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_eq!(
        status_before["result"]["cross_group_pointers"]["bound"], 1,
        "{status_before}"
    );
    assert_eq!(
        status_before["result"]["cross_group_pointers"]["unbound"], 0,
        "{status_before}"
    );

    let v = dispatch_val(
        2,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 2);
    let impacts = v["result"]["unbound_impacts"].as_array().unwrap();
    assert_eq!(impacts.len(), 1, "{v}");
    assert_eq!(impacts[0]["group_id"], GROUP_LAYER, "{v}");
    assert_eq!(impacts[0]["pointer_count"], 1, "{v}");

    // The RelatesToNode_ itself (owned by GROUP_LAYER, not GROUP_A) must survive (FR-008).
    let conn = db.connect().unwrap();
    let survived = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge_uuid))
        .unwrap();
    assert_eq!(
        survived.len(),
        1,
        "RelatesToNode_ owned by the layer group must survive the purge of group A"
    );
    let mid = &survived[0];
    assert_eq!(
        mid.source_node_uuid, alice_uuid,
        "src hop (own group) survives"
    );
    assert_eq!(
        mid.target_node_uuid, "",
        "dst hop into purged group A is gone"
    );

    // The pointer's binding_state must now be Unbound, not merely absent a hop (FR-009).
    let ptr = pointer::read_pointers(&mid.attributes);
    assert_eq!(
        ptr.get(EndpointSide::Dst).unwrap().binding_state,
        BindingState::Unbound
    );

    // knowledge_status reflects the unbound pointer immediately (FR-010/SC-008).
    let status_after = dispatch_val(3, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_eq!(
        status_after["result"]["cross_group_pointers"]["bound"], 0,
        "{status_after}"
    );
    assert_eq!(
        status_after["result"]["cross_group_pointers"]["unbound"], 1,
        "{status_after}"
    );

    // Rehydrating group A (re-inserting "Bob" under a new UUID) and re-binding must restore it
    // to zero unbound (SC-008's "returns to zero once the purged group is rehydrated").
    let bob_v2 = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&bob_v2).unwrap();
    let rebind_counts = cross_group::rebind_pointers_forced(&conn, GROUP_A, TS).unwrap();
    assert_eq!(rebind_counts.bound, 1);

    let status_rehydrated =
        dispatch_val(4, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_eq!(
        status_rehydrated["result"]["cross_group_pointers"]["unbound"], 0,
        "{status_rehydrated}"
    );
    assert_eq!(
        status_rehydrated["result"]["cross_group_pointers"]["bound"], 1,
        "{status_rehydrated}"
    );
}

// ── AC4 / FR-006: purging a nonexistent group_id is a no-op success ─────────────────────────

#[tokio::test]
async fn purge_nonexistent_group_is_noop_success() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity("Untouched", GROUP_A, TS))
            .unwrap();
    }
    let state = make_state(Arc::clone(&db), None);

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": ["does-not-exist"], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 1);
    assert_eq!(v["result"]["success"], true, "{v}");
    let groups = v["result"]["groups"].as_array().unwrap();
    assert_eq!(groups[0]["entities"], 0, "{v}");
    assert_eq!(groups[0]["episodes"], 0, "{v}");
    assert_eq!(groups[0]["edges"], 0, "{v}");

    let (a_ent, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(
        a_ent, 1,
        "group A must be untouched by an absent-group purge"
    );
}

// ── US2 / FR-012 / FR-013 / SC-009: dry_run preview matches the real purge exactly ───────────

#[tokio::test]
async fn dry_run_mutates_nothing_and_matches_a_following_real_purge() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        let alice = make_entity("Alice", GROUP_LAYER, TS);
        let bob = make_entity("Bob", GROUP_A, TS);
        conn.insert_entity(&alice).unwrap();
        conn.insert_entity(&bob).unwrap();
        cross_group::create_cross_group_edge(
            &conn,
            CreateCrossGroupEdgeParams {
                name: "KNOWS".to_string(),
                source: EndpointSpec::Uuid(alice.uuid.clone()),
                target: EndpointSpec::Foreign {
                    source_group_id: GROUP_A.to_string(),
                    endpoint_name: "Bob".to_string(),
                },
                group_id: GROUP_LAYER.to_string(),
                fact: "Alice knows Bob".to_string(),
                fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
                valid_at: None,
                relation_type: None,
            },
            TS,
        )
        .unwrap();
    }

    let state = make_state(Arc::clone(&db), None);

    let (a_ent_before, a_ep_before, a_edges_before) = group_counts(&db, GROUP_A);

    let dry = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "dry_run": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&dry, 1);
    assert_eq!(dry["result"]["dry_run"], true, "{dry}");

    // Nothing mutated.
    let (a_ent_mid, a_ep_mid, a_edges_mid) = group_counts(&db, GROUP_A);
    assert_eq!(a_ent_mid, a_ent_before);
    assert_eq!(a_ep_mid, a_ep_before);
    assert_eq!(a_edges_mid, a_edges_before);

    let real = dispatch_val(
        2,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&real, 2);
    assert_eq!(real["result"]["dry_run"], false, "{real}");

    // The counts (and unbound impacts) predicted by dry_run must exactly match the real purge.
    assert_eq!(
        dry["result"]["groups"], real["result"]["groups"],
        "{dry} vs {real}"
    );
    assert_eq!(
        dry["result"]["unbound_impacts"], real["result"]["unbound_impacts"],
        "{dry} vs {real}"
    );
}

/// FR-013: `dry_run: true` combined with `confirm: true` still performs no mutation —
/// `dry_run` takes precedence.
#[tokio::test]
async fn dry_run_true_with_confirm_true_still_does_not_mutate() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity("Still Here", GROUP_A, TS))
            .unwrap();
    }
    let state = make_state(Arc::clone(&db), None);

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "dry_run": true, "confirm": true}),
        state,
    )
    .await;
    assert_ok(&v, 1);
    assert_eq!(v["result"]["dry_run"], true, "{v}");

    let (a_ent, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(
        a_ent, 1,
        "dry_run must take precedence over confirm (FR-013)"
    );
}

// ── confirm gate / param validation ──────────────────────────────────────────────────────────

#[tokio::test]
async fn purge_rejected_without_confirm_or_dry_run() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity("Guarded", GROUP_A, TS))
            .unwrap();
    }
    let state = make_state(Arc::clone(&db), None);

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A]}),
        Arc::clone(&state),
    )
    .await;
    assert_err(&v, 1);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("confirm"), "error should mention confirm: {v}");

    let (a_ent, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(a_ent, 1, "DB must be unchanged after a rejected purge");
}

#[tokio::test]
async fn purge_rejects_missing_group_ids() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    let state = make_state(Arc::clone(&db), None);

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_err(&v, 1);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("group_ids"),
        "error should mention group_ids: {v}"
    );
}

#[tokio::test]
async fn purge_rejects_empty_group_ids_array() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    let state = make_state(Arc::clone(&db), None);

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_err(&v, 1);
}

// ── FR-002: multiple group_ids are purged atomically in one call ────────────────────────────

#[tokio::test]
async fn purge_multiple_groups_in_one_call() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity("A", GROUP_A, TS)).unwrap();
        conn.insert_entity(&make_entity("B", GROUP_B, TS)).unwrap();
    }
    let state = make_state(Arc::clone(&db), None);

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A, GROUP_B], "confirm": true}),
        state,
    )
    .await;
    assert_ok(&v, 1);
    let groups = v["result"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{v}");

    let (a_ent, _, _) = group_counts(&db, GROUP_A);
    let (b_ent, _, _) = group_counts(&db, GROUP_B);
    assert_eq!(a_ent, 0);
    assert_eq!(b_ent, 0);
}

// ── NameIndex staleness: a purged entity's name must no longer resolve ──────────────────────

#[test]
fn purged_entity_name_no_longer_resolves_via_name_index() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("NameIndexed Alice", GROUP_A, TS);
    conn.insert_entity(&alice).unwrap();
    conn.rebuild_name_index().unwrap();
    assert!(
        conn.get_entity_by_name_ci("NameIndexed Alice", GROUP_A)
            .unwrap()
            .is_some(),
        "name index should resolve the entity before purge"
    );

    let counts = group_purge::purge_groups(&conn, &[GROUP_A], TS, false).unwrap();
    assert_eq!(counts.groups[0].entities, 1);

    assert!(
        conn.get_entity_by_name_ci("NameIndexed Alice", GROUP_A)
            .unwrap()
            .is_none(),
        "name index must not resolve a purged entity's name"
    );
}

// ── SC-004 proxy: purge-then-replay restores the pre-purge state ────────────────────────────
//
// #378 (per-group WAL directories) is unimplemented, so there is no "group A's own WAL
// directory" to replay in isolation yet (see the Plan stage's documented decision). This test
// exercises the closest available proxy: hand-write a WAL representing groups A and B's
// pre-purge state, purge group A with *no* WAL writer attached (so the purge's own deletes are
// never recorded), then force_clear + replay that WAL — restoring both groups to their
// pre-purge counts, since the replayed WAL never saw the purge at all.

fn entity_wal_line(seq: u64, uuid: &str, name: &str, group_id: &str) -> String {
    let line = json!({
        "seq": seq,
        "ts": "2026-01-01T00:00:00.000000+00:00",
        "db": "",
        "cypher": "MERGE (n:Entity {uuid: $uuid}) ON CREATE SET n.name = $name, \
             n.group_id = $group_id, n.labels = $labels, n.created_at = $created_at, \
             n.name_embedding = $name_embedding, n.summary = $summary, n.attributes = $attributes",
        "params": {
            "uuid": uuid,
            "name": name,
            "group_id": group_id,
            "labels": ["Entity"],
            "created_at": "2026-01-01T00:00:00.000000+00:00",
            "name_embedding": [1.0, 0.0, 0.0, 0.0],
            "summary": "s",
            "attributes": "{}",
        },
    });
    line.to_string()
}

#[tokio::test]
async fn purge_then_rebuild_from_wal_restores_purged_group() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let db = Arc::new(open_db(&dir));
    let wal_dir = TempDir::new().unwrap();

    let content = [
        entity_wal_line(0, "11111111-1111-1111-1111-111111111111", "A-One", GROUP_A),
        entity_wal_line(1, "22222222-2222-2222-2222-222222222222", "B-One", GROUP_B),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        wal_dir.path().join("20260101_000000_aaa111_0000.jsonl"),
        &content,
    )
    .unwrap();

    // No live wal_writer attached: the purge below runs against the DB directly and its own
    // deletes are never appended to wal_dir, keeping the hand-written WAL a clean pre-purge
    // snapshot to replay from.
    let state_no_writer = make_state_with_path(Arc::clone(&db), db_path.clone(), None);

    // Populate the DB directly (mirrors what replaying the WAL above would produce) so the
    // purge has something to remove.
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            ..make_entity("A-One", GROUP_A, TS)
        })
        .unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "22222222-2222-2222-2222-222222222222".to_string(),
            ..make_entity("B-One", GROUP_B, TS)
        })
        .unwrap();
    }

    let purge_v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "confirm": true}),
        Arc::clone(&state_no_writer),
    )
    .await;
    assert_ok(&purge_v, 1);
    let (a_ent_after_purge, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(a_ent_after_purge, 0, "group A purged");

    // Now replay the untouched pre-purge WAL with force_clear, using a state that has wal_dir
    // configured (rebuild_from_wal only needs wal_dir, not a live writer). Must share db_path
    // with state_no_writer above: force_clear reopens `Db::open(&state.db_path)` and swaps the
    // result into `state.db`, so a mismatched path would silently operate on a different file.
    let state_with_wal_dir = make_state_with_path(
        Arc::clone(&db),
        db_path.clone(),
        Some(wal_dir.path().to_path_buf()),
    );
    let rebuild_v = dispatch_val(
        2,
        "knowledge_rebuild_from_wal",
        json!({"force_clear": true}),
        Arc::clone(&state_with_wal_dir),
    )
    .await;
    assert_ok(&rebuild_v, 2);
    let job_id = rebuild_v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch_val(
            3,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state_with_wal_dir),
        )
        .await;
        match status_v["result"]["status"].as_str().unwrap_or("?") {
            "completed" => break,
            "failed" => panic!("rebuild job failed: {status_v}"),
            "running" => {
                if std::time::Instant::now() > deadline {
                    panic!("rebuild did not complete within 10s: {status_v}");
                }
            }
            other => panic!("unexpected status: {other}: {status_v}"),
        }
    }

    // force_clear reopened the DB and swapped a new Db instance into state.db — the original
    // `db` Arc from before the rebuild is now a stale handle to a deleted file, so post-rebuild
    // assertions must go through the live, swapped instance.
    let db_after_rebuild = state_with_wal_dir
        .db
        .load_full()
        .expect("db must be loaded");
    let (a_ent_restored, _, _) = group_counts(&db_after_rebuild, GROUP_A);
    let (b_ent_restored, _, _) = group_counts(&db_after_rebuild, GROUP_B);
    assert_eq!(
        a_ent_restored, 1,
        "group A must be restored by replaying its pre-purge WAL"
    );
    assert_eq!(
        b_ent_restored, 1,
        "group B was never purged, unaffected by replay"
    );
}
