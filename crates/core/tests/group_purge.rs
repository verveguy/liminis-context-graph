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
    WalWriter, DEFAULT_GROUP_ID,
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
        wal_root: wal_dir,
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

/// Polls `knowledge_rebuild_status` for `job_id` until it reports `completed`, panicking on
/// `failed` or a 10s timeout. Shared by every test that drives a background rebuild job via
/// `knowledge_rebuild_from_wal`.
async fn wait_for_rebuild(id: i64, job_id: &str, state: &Arc<AppState>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch_val(
            id,
            "knowledge_rebuild_status",
            json!({"job_id": job_id}),
            Arc::clone(state),
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

/// Reads every WAL line's `cypher` field under `wal_root/group_id`, in file-name (and therefore
/// chronological) order — for issue #385's assertions, which need to know not just *how many*
/// mutations landed in a group's stream but *which* ones (a purge's `DETACH DELETE`s vs. a
/// forced rebind's `SET rn.attributes` / hop `MERGE`/`DELETE`). Every `group_id` used by this
/// module's tests is already a safe, unencoded directory name (see
/// `wal_group::encode_group_dir_name`), so the group_id is used as the subdirectory name
/// directly. Returns an empty vec (not an error) when the group's directory doesn't exist at
/// all — the FR-004 "no directory" case is itself the thing several tests assert on.
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
        .fts_search_entities("Alice Probe", Some(&[GROUP_A]), 10)
        .unwrap();
    assert!(
        fts_before.iter().any(|(uuid, _)| uuid == &alice.uuid),
        "FTS should find the entity before delete: {fts_before:?}"
    );
    let vec_before = conn
        .vector_search_entities(&[1.0, 0.0, 0.0, 0.0], Some(&[GROUP_A]), 10)
        .unwrap();
    assert!(
        vec_before.iter().any(|(uuid, _)| uuid == &alice.uuid),
        "vector search should find the entity before delete: {vec_before:?}"
    );

    conn.delete_entities_by_group_ids(&[GROUP_A]).unwrap();

    let fts_after = conn
        .fts_search_entities("Alice Probe", Some(&[GROUP_A]), 10)
        .unwrap();
    assert!(
        !fts_after.iter().any(|(uuid, _)| uuid == &alice.uuid),
        "FTS index must not return a deleted entity (self-maintains on DETACH DELETE): \
         {fts_after:?}"
    );
    let vec_after = conn
        .vector_search_entities(&[1.0, 0.0, 0.0, 0.0], Some(&[GROUP_A]), 10)
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
            .fts_search_entities("Searchable", Some(&[GROUP_A, GROUP_B]), 10)
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
        .fts_search_entities("Searchable", Some(&[GROUP_A, GROUP_B]), 10)
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
    let (rebind_counts, _) = cross_group::rebind_pointers_forced(&conn, GROUP_A, TS).unwrap();
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

/// A malformed element (non-string, e.g. from an upstream template bug) must reject the whole
/// request rather than silently dropping it and purging only the well-formed elements — for a
/// destructive, confirm-gated admin op, a partially-understood argument list must fail loudly.
#[tokio::test]
async fn purge_rejects_group_ids_array_with_non_string_element() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity("Real", GROUP_A, TS))
            .unwrap();
    }
    let state = make_state(Arc::clone(&db), None);

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A, 123], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_err(&v, 1);

    let (a_ent, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(
        a_ent, 1,
        "a malformed group_ids element must reject the whole call, not purge a subset"
    );
}

/// A repeated `group_id` in the request must be deduped before use: it must not produce a
/// duplicate `GroupPurgeCounts` entry in the response's `groups` array (which would misreport
/// the purge as having touched the group twice), and each group's counts must reflect that
/// group's actual data exactly once.
#[tokio::test]
async fn purge_dedupes_repeated_group_ids() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity("A", GROUP_A, TS)).unwrap();
    }
    let state = make_state(Arc::clone(&db), None);

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A, GROUP_A], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 1);
    let groups = v["result"]["groups"].as_array().unwrap();
    assert_eq!(
        groups.len(),
        1,
        "a repeated group_id must not produce a duplicate entry: {v}"
    );
    assert_eq!(groups[0]["group_id"], GROUP_A, "{v}");
    assert_eq!(groups[0]["entities"], 1, "{v}");

    let (a_ent, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(a_ent, 0);
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

/// FR-002/FR-008/FR-012 interaction: when a multi-group purge call includes both the layer
/// group that owns a `RelatesToNode_` *and* the foreign group its pointer targets, that
/// `RelatesToNode_` is `DETACH DELETE`d outright (it's owned by one of the purged groups) — it
/// never survives to reach the `unbound` state. `unbound_impacts` must not report it, since
/// "left unbound" only describes a `RelatesToNode_` owned by a group *outside* the call.
#[tokio::test]
async fn purge_excludes_unbound_impact_when_owning_group_is_itself_purged() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    let edge_uuid = {
        let conn = db.connect().unwrap();
        let layer_entity = make_entity("LayerEntity", GROUP_A, TS);
        let bob = make_entity("Bob", GROUP_B, TS);
        conn.insert_entity(&layer_entity).unwrap();
        conn.insert_entity(&bob).unwrap();

        // Layer group_id = GROUP_A (about to be purged), pointing at a foreign entity in
        // GROUP_B (also about to be purged in the same call).
        let edge = cross_group::create_cross_group_edge(
            &conn,
            CreateCrossGroupEdgeParams {
                name: "KNOWS".to_string(),
                source: EndpointSpec::Uuid(layer_entity.uuid.clone()),
                target: EndpointSpec::Foreign {
                    source_group_id: GROUP_B.to_string(),
                    endpoint_name: "Bob".to_string(),
                },
                group_id: GROUP_A.to_string(),
                fact: "LayerEntity knows Bob".to_string(),
                fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
                valid_at: None,
                relation_type: None,
            },
            TS,
        )
        .unwrap();
        edge.uuid
    };

    let state = make_state(Arc::clone(&db), None);

    let dry = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A, GROUP_B], "dry_run": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&dry, 1);
    assert_eq!(
        dry["result"]["unbound_impacts"].as_array().unwrap().len(),
        0,
        "dry_run must not report an unbound impact for a RelatesToNode_ owned by a group \
         that is itself being purged (it's deleted, not left unbound): {dry}"
    );

    let real = dispatch_val(
        2,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A, GROUP_B], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&real, 2);
    assert_eq!(
        real["result"]["unbound_impacts"].as_array().unwrap().len(),
        0,
        "{real}"
    );

    let conn = db.connect().unwrap();
    let survived = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge_uuid))
        .unwrap();
    assert!(
        survived.is_empty(),
        "the RelatesToNode_ owned by a purged group must be deleted outright, not survive as unbound"
    );
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

    let (counts, _) = group_purge::purge_groups(&conn, &[GROUP_A], TS, false).unwrap();
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
    let wal_root = TempDir::new().unwrap();

    // knowledge_rebuild_from_wal is group-scoped (issue #378 FR-006): only group A's own WAL
    // directory needs A's content — group B is never purged nor rebuilt here, so it has no WAL
    // presence at all, which also proves the rebuild can't be borrowing anything from it.
    let group_a_dir = wal_root.path().join(GROUP_A);
    std::fs::create_dir_all(&group_a_dir).unwrap();
    let content =
        entity_wal_line(0, "11111111-1111-1111-1111-111111111111", "A-One", GROUP_A) + "\n";
    std::fs::write(
        group_a_dir.join("20260101_000000_aaa111_0000.jsonl"),
        &content,
    )
    .unwrap();

    // No live wal_writer attached: the purge below runs against the DB directly and its own
    // deletes are never appended to the WAL root, keeping the hand-written WAL a clean
    // pre-purge snapshot to replay from.
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

    // Now replay the untouched pre-purge WAL with force_clear, using a state that has wal_root
    // configured (rebuild_from_wal only needs wal_root, not a live writer). Must share db_path
    // with state_no_writer above: the group-scoped force_clear path operates against
    // state.db_path's already-open Db (no reopen/swap, unlike knowledge_clear_all), so a
    // mismatched path would silently operate on a different file.
    let state_with_wal_dir = make_state_with_path(
        Arc::clone(&db),
        db_path.clone(),
        Some(wal_root.path().to_path_buf()),
    );
    let rebuild_v = dispatch_val(
        2,
        "knowledge_rebuild_from_wal",
        json!({"group_id": GROUP_A, "force_clear": true}),
        Arc::clone(&state_with_wal_dir),
    )
    .await;
    assert_ok(&rebuild_v, 2);
    let job_id = rebuild_v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    wait_for_rebuild(3, &job_id, &state_with_wal_dir).await;

    // The group-scoped force_clear path never swaps state.db (unlike knowledge_clear_all) —
    // the original `db` Arc is still the live, correct handle after rebuild.
    let (a_ent_restored, _, _) = group_counts(&db, GROUP_A);
    let (b_ent_restored, _, _) = group_counts(&db, GROUP_B);
    assert_eq!(
        a_ent_restored, 1,
        "group A must be restored by replaying its pre-purge WAL"
    );
    assert_eq!(
        b_ent_restored, 1,
        "group B was never purged, unaffected by replay"
    );
}

// ── Issue #385: delete_by_group / rebind_pointers WAL attribution ───────────────────────────
//
// The scenario reproduced in the issue: groups A, B, C where C (the layer group) holds a
// cross-group edge into A. Before #385, `knowledge_delete_by_group(["A"])` routed *both* A's
// own deletions and C's forced-rebind writes through the default group's ("liminis") WAL
// stream — a group that was never otherwise written to. After #385, each mutation lands in the
// stream of the group whose data it actually modifies.

/// User Story 1 / SC-001 / AC1: A's deletions land in `A/`, C's (the layer group's)
/// forced-rebind mutations land in `C/`, B is completely untouched, and no `liminis/`
/// directory is created — matching the issue's reproduction exactly (A/B/C, C holding a
/// cross-group edge into A).
#[tokio::test]
async fn delete_by_group_attributes_deletions_and_forced_rebind_to_their_own_groups() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        let alice = make_entity("Alice", GROUP_LAYER, TS); // C — owns the cross-group edge
        let bob = make_entity("Bob", GROUP_A, TS); // A — about to be purged
        let carol = make_entity("Carol", GROUP_B, TS); // B — must be left untouched
        conn.insert_entity(&alice).unwrap();
        conn.insert_entity(&bob).unwrap();
        conn.insert_entity(&carol).unwrap();
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

    let wal_root = TempDir::new().unwrap();
    let state = make_state(Arc::clone(&db), Some(wal_root.path().to_path_buf()));

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 1);

    // No liminis/ directory: nothing was ever attributed to the default group (FR-004).
    assert!(
        !wal_root.path().join(DEFAULT_GROUP_ID).exists(),
        "delete_by_group must never create the default group's WAL directory"
    );

    // A/ contains exactly A's own purge deletions.
    let a_cyphers = read_wal_cyphers(wal_root.path(), GROUP_A);
    assert!(
        !a_cyphers.is_empty(),
        "group A's own WAL stream must contain its purge's deletions"
    );
    assert!(
        a_cyphers
            .iter()
            .all(|c| c.contains("DETACH DELETE") || c.contains("MATCH")),
        "A's stream must contain only its own purge's DETACH DELETE mutations: {a_cyphers:?}"
    );
    assert!(
        a_cyphers.iter().any(|c| c.contains("(e:Entity)")),
        "A's stream must contain its own entity deletion: {a_cyphers:?}"
    );
    assert!(
        !a_cyphers.iter().any(|c| c.contains("rn.attributes")),
        "A's stream must not contain C's forced-rebind attribute write: {a_cyphers:?}"
    );

    // C/ (the layer group) contains the forced-rebind pointer mutations, not A's deletions.
    let layer_cyphers = read_wal_cyphers(wal_root.path(), GROUP_LAYER);
    assert!(
        !layer_cyphers.is_empty(),
        "the owning group's WAL stream must contain the forced-rebind mutations"
    );
    assert!(
        layer_cyphers.iter().any(|c| c.contains("rn.attributes")),
        "layer group's stream must contain the pointer attribute rewrite: {layer_cyphers:?}"
    );
    assert!(
        !layer_cyphers
            .iter()
            .any(|c| c.contains("(e:Entity)") && c.contains("DETACH DELETE")),
        "layer group's stream must not contain A's entity deletion: {layer_cyphers:?}"
    );

    // B/ was never touched by this call — it must have no WAL directory at all.
    assert!(
        !wal_root.path().join(GROUP_B).exists(),
        "group B must gain no WAL directory as a side effect of purging A (FR-004)"
    );
}

/// SC-003 / User Story 1 AC2: replaying A's own WAL stream in isolation reproduces A's purge —
/// its deletions are recorded on A's own stream, not merely applied to the live DB, so a fresh
/// database built only from `A/`'s replay ends up purged too, never resurrecting what was
/// deleted.
#[tokio::test]
async fn purge_deletions_are_present_in_own_groups_wal_and_survive_isolated_replay() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let db = Arc::new(open_db(&dir));
    let wal_root = TempDir::new().unwrap();
    let state = make_state_with_path(
        Arc::clone(&db),
        db_path,
        Some(wal_root.path().to_path_buf()),
    );

    // Create A's entity through the real IPC write path so its creation is recorded on A's own
    // WAL stream exactly as it would be in production.
    let create_v = dispatch_val(
        1,
        "knowledge_assert_entity",
        json!({"name": "A-One", "group_id": GROUP_A}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&create_v, 1);
    let (a_ent_before, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(a_ent_before, 1, "A-One must exist before the purge");

    // Purge A — with #385's fix, the deletion is recorded on A's own WAL stream (previously it
    // would have gone to liminis/, leaving A's stream containing only the creation).
    let purge_v = dispatch_val(
        2,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&purge_v, 2);
    let (a_ent_after_purge, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(a_ent_after_purge, 0, "A-One must be gone after the purge");

    let a_cyphers = read_wal_cyphers(wal_root.path(), GROUP_A);
    assert!(
        a_cyphers
            .iter()
            .any(|c| c.contains("CREATE") || c.contains("MERGE")),
        "A's stream must contain its own creation: {a_cyphers:?}"
    );
    assert!(
        a_cyphers.iter().any(|c| c.contains("DETACH DELETE")),
        "A's stream must also contain its own purge's deletion: {a_cyphers:?}"
    );

    // Replay A's own stream from scratch (from_seq: 0, via force_clear) — if the deletion above
    // had landed anywhere other than A's own stream, this replay would resurrect A-One.
    let rebuild_v = dispatch_val(
        3,
        "knowledge_rebuild_from_wal",
        json!({"group_id": GROUP_A, "force_clear": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&rebuild_v, 3);
    let job_id = rebuild_v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();
    wait_for_rebuild(4, &job_id, &state).await;

    let (a_ent_replayed, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(
        a_ent_replayed, 0,
        "replaying A's own WAL stream in isolation must not resurrect a purged entity (SC-003)"
    );
}

/// Edge Cases / FR-003: purging `["A", "B"]` where A owns an edge referencing B (a fellow purge
/// target) via a cross-group pointer. The edge is `DETACH DELETE`d outright as part of A's own
/// purge (it's owned by A) — the mutation is attributed to A's stream, not B's, and it never
/// reaches the `unbound`/forced-rebind path (B's own purge doesn't need to rebind anything for
/// an edge that no longer exists).
#[tokio::test]
async fn purge_multi_group_attributes_owner_purged_edge_to_owning_groups_stream() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        let owner = make_entity("Owner", GROUP_A, TS);
        let bob = make_entity("Bob", GROUP_B, TS);
        conn.insert_entity(&owner).unwrap();
        conn.insert_entity(&bob).unwrap();
        // Edge owned by A (about to be purged), pointing at a foreign entity in B (also about
        // to be purged in the same call).
        cross_group::create_cross_group_edge(
            &conn,
            CreateCrossGroupEdgeParams {
                name: "KNOWS".to_string(),
                source: EndpointSpec::Uuid(owner.uuid.clone()),
                target: EndpointSpec::Foreign {
                    source_group_id: GROUP_B.to_string(),
                    endpoint_name: "Bob".to_string(),
                },
                group_id: GROUP_A.to_string(),
                fact: "Owner knows Bob".to_string(),
                fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
                valid_at: None,
                relation_type: None,
            },
            TS,
        )
        .unwrap();
    }

    let wal_root = TempDir::new().unwrap();
    let state = make_state(Arc::clone(&db), Some(wal_root.path().to_path_buf()));

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A, GROUP_B], "confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 1);
    assert_eq!(
        v["result"]["unbound_impacts"].as_array().unwrap().len(),
        0,
        "the edge is deleted outright (owned by a purge target), never left unbound: {v}"
    );

    assert!(
        !wal_root.path().join(DEFAULT_GROUP_ID).exists(),
        "a multi-group purge must never create the default group's WAL directory"
    );

    let a_cyphers = read_wal_cyphers(wal_root.path(), GROUP_A);
    assert!(
        a_cyphers
            .iter()
            .any(|c| c.contains("RelatesToNode_") && c.contains("DETACH DELETE")),
        "the edge's deletion must be attributed to A, the group that owns it: {a_cyphers:?}"
    );

    // B's own per-group purge pass still issues its own (zero-matching-row) RelatesToNode_
    // delete call — that's B's own deletion attempt, not a leak of A's edge. What must never
    // appear in B's stream is a forced-rebind attribute write for an edge B doesn't own.
    let b_cyphers = read_wal_cyphers(wal_root.path(), GROUP_B);
    assert!(
        !b_cyphers.iter().any(|c| c.contains("rn.attributes")),
        "B's stream must not receive a forced-rebind mutation for an edge it doesn't own: \
         {b_cyphers:?}"
    );
}

/// Edge Cases: `dry_run: true` produces no WAL mutations of any kind — no group, including the
/// default group, gains a WAL directory as a side effect of a dry-run preview.
#[tokio::test]
async fn dry_run_creates_no_wal_directory_for_any_group() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity("Bob", GROUP_A, TS))
            .unwrap();
    }

    let wal_root = TempDir::new().unwrap();
    let state = make_state(Arc::clone(&db), Some(wal_root.path().to_path_buf()));

    let v = dispatch_val(
        1,
        "knowledge_delete_by_group",
        json!({"group_ids": [GROUP_A], "dry_run": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 1);
    assert_eq!(v["result"]["dry_run"], true, "{v}");

    assert!(
        !wal_root.path().join(GROUP_A).exists(),
        "dry_run must not create even the purged group's own WAL directory"
    );
    assert!(
        !wal_root.path().join(DEFAULT_GROUP_ID).exists(),
        "dry_run must not create the default group's WAL directory"
    );
}

/// `clear_group_for_rebuild` (used by `knowledge_rebuild_from_wal`'s `from_seq: 0` path) shares
/// `group_purge::purge_groups` with `handle_delete_by_group` and has the identical bug before
/// #385: a forced rebind of a foreign owning group's pointers must land in that owning group's
/// own WAL stream, not the default group's.
#[tokio::test]
async fn clear_group_for_rebuild_routes_forced_rebind_to_owning_group_not_default() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let db = Arc::new(open_db(&dir));
    let wal_root = TempDir::new().unwrap();

    // A minimal but valid pre-existing WAL stream for group A, so knowledge_rebuild_from_wal
    // has something to replay after clear_group_for_rebuild's own purge runs. Its content is
    // incidental to this test, which is about where the *forced rebind* mutation lands.
    let seed_uuid = "33333333-3333-3333-3333-333333333333";
    let group_a_dir = wal_root.path().join(GROUP_A);
    std::fs::create_dir_all(&group_a_dir).unwrap();
    std::fs::write(
        group_a_dir.join("20260101_000000_seed_0000.jsonl"),
        entity_wal_line(0, seed_uuid, "A-Seed", GROUP_A) + "\n",
    )
    .unwrap();

    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: seed_uuid.to_string(),
            ..make_entity("A-Seed", GROUP_A, TS)
        })
        .unwrap();
        let alice = make_entity("Alice", GROUP_LAYER, TS);
        conn.insert_entity(&alice).unwrap();
        // Layer-owned cross-group edge pointing into A — the forced-rebind target once A is
        // cleared for rebuild.
        cross_group::create_cross_group_edge(
            &conn,
            CreateCrossGroupEdgeParams {
                name: "KNOWS".to_string(),
                source: EndpointSpec::Uuid(alice.uuid.clone()),
                target: EndpointSpec::Foreign {
                    source_group_id: GROUP_A.to_string(),
                    endpoint_name: "A-Seed".to_string(),
                },
                group_id: GROUP_LAYER.to_string(),
                fact: "Alice knows A-Seed".to_string(),
                fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
                valid_at: None,
                relation_type: None,
            },
            TS,
        )
        .unwrap();
    }

    let state = make_state_with_path(
        Arc::clone(&db),
        db_path,
        Some(wal_root.path().to_path_buf()),
    );

    let rebuild_v = dispatch_val(
        1,
        "knowledge_rebuild_from_wal",
        json!({"group_id": GROUP_A, "force_clear": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&rebuild_v, 1);
    let job_id = rebuild_v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();
    wait_for_rebuild(2, &job_id, &state).await;

    assert!(
        !wal_root.path().join(DEFAULT_GROUP_ID).exists(),
        "clear_group_for_rebuild's forced rebind must never create the default group's WAL \
         directory"
    );
    let layer_cyphers = read_wal_cyphers(wal_root.path(), GROUP_LAYER);
    assert!(
        layer_cyphers.iter().any(|c| c.contains("rn.attributes")),
        "the forced rebind of the layer group's pointer must be attributed to the layer \
         group's own stream: {layer_cyphers:?}"
    );
}

// ── Issue #462: group_purge::purge_group_rows — row-scoped purge for the split-stream case ──
//
// Unlike `purge_groups` (whole-group), `purge_group_rows` deletes only the exact uuids named,
// leaving every other row in the same group untouched — the primitive `clear_groups_for_rebuild`
// uses when a `force_clear` rebuild's WAL content references a foreign group that also has an
// independent, un-replayed stream elsewhere (FR-001), while still clearing the rows that
// specific replay owns (FR-002).

#[tokio::test]
async fn purge_group_rows_deletes_only_named_uuids() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));

    let (target, sibling) = {
        let conn = db.connect().unwrap();
        let target = make_entity("Target", GROUP_A, TS);
        let sibling = make_entity("Sibling", GROUP_A, TS);
        conn.insert_entity(&target).unwrap();
        conn.insert_entity(&sibling).unwrap();
        (target, sibling)
    };

    {
        let conn = db.connect().unwrap();
        group_purge::purge_group_rows(&conn, GROUP_A, std::slice::from_ref(&target.uuid), TS)
            .unwrap();
    }

    let conn = db.connect().unwrap();
    assert!(
        conn.get_entity_by_uuid(&target.uuid).unwrap().is_none(),
        "the named uuid must be deleted"
    );
    assert!(
        conn.get_entity_by_uuid(&sibling.uuid).unwrap().is_some(),
        "a sibling row in the same group not named in uuids must survive — this is the whole \
         point of row-scoped over whole-group purge"
    );
    let (ent_count, _, _) = group_counts(&db, GROUP_A);
    assert_eq!(ent_count, 1, "only the sibling should remain in the group");
}

#[tokio::test]
async fn purge_group_rows_leaves_untouched_episodic_and_relates_to_in_same_group() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));

    let (entity_uuid, a2_uuid, episodic_uuid, edge_uuid) = {
        let conn = db.connect().unwrap();
        let a1 = make_entity("A-One", GROUP_A, TS);
        let a2 = make_entity("A-Two", GROUP_A, TS);
        conn.insert_entity(&a1).unwrap();
        conn.insert_entity(&a2).unwrap();

        let edge = lcg_core::types::RelatesToEdge {
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
        conn.insert_relates_to_edge(&edge).unwrap();

        let episodic = lcg_core::types::EpisodicRow {
            uuid: Uuid::new_v4().to_string(),
            name: "chunk-1".to_string(),
            group_id: GROUP_A.to_string(),
            created_at: TS.to_string(),
            source: "text".to_string(),
            source_description: "test".to_string(),
            content: "hello".to_string(),
            content_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: TS.to_string(),
            entity_edges: vec![],
        };
        conn.insert_episodic(&episodic).unwrap();

        (a1.uuid, a2.uuid, episodic.uuid, edge.uuid)
    };

    // Only the entity's uuid is targeted — the episodic and the relates_to edge, both in the
    // same group, are not named and must survive.
    {
        let conn = db.connect().unwrap();
        group_purge::purge_group_rows(&conn, GROUP_A, std::slice::from_ref(&entity_uuid), TS)
            .unwrap();
    }

    let conn = db.connect().unwrap();
    assert!(
        conn.get_entity_by_uuid(&entity_uuid).unwrap().is_none(),
        "the named entity must be deleted"
    );
    let episodics = conn.retrieve_episodes(GROUP_A, 10).unwrap();
    assert!(
        episodics.iter().any(|e| e.uuid == episodic_uuid),
        "the episodic in the same group must survive a row-scoped purge that didn't name it"
    );
    let edges = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge_uuid))
        .unwrap();
    assert_eq!(
        edges.len(),
        1,
        "the relates_to edge in the same group must survive a row-scoped purge that didn't \
         name it"
    );
    // purge_group_rows performs no topology safety check of its own (see its own doc comment's
    // "Precondition" paragraph) — DETACH DELETEing entity_uuid severed this edge's hop to it,
    // leaving the surviving RelatesToNode_ row dangling on that side. This assertion documents
    // why handlers.rs's classification (both the unlocked pass and the locked freshness
    // re-check) must call db::Conn::find_relates_to_dangling_after_uuid_purge and refuse the
    // rebuild before ever reaching this function in a case like this one — not that calling
    // purge_group_rows directly, as this test does, was itself safe.
    assert_eq!(
        edges[0].source_node_uuid, "",
        "the edge's hop to the deleted entity must be severed (dangling), confirming why the \
         caller-side topology guard exists"
    );
    assert_eq!(
        edges[0].target_node_uuid, a2_uuid,
        "the edge's hop to the untouched sibling entity must remain intact"
    );
}

#[tokio::test]
async fn purge_group_rows_forced_rebind_correctness_under_partial_emptying() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir));

    let (bob_uuid, carol_uuid) = {
        let conn = db.connect().unwrap();
        let alice = make_entity("Alice", GROUP_LAYER, TS);
        let bob = make_entity("Bob", GROUP_A, TS);
        let carol = make_entity("Carol", GROUP_A, TS);
        conn.insert_entity(&alice).unwrap();
        conn.insert_entity(&bob).unwrap();
        conn.insert_entity(&carol).unwrap();

        // Two layer-owned cross-group pointers into group A: one at Bob (will be deleted below),
        // one at Carol (will survive) — partial emptying, not a whole-group purge.
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
        cross_group::create_cross_group_edge(
            &conn,
            CreateCrossGroupEdgeParams {
                name: "KNOWS".to_string(),
                source: EndpointSpec::Uuid(alice.uuid.clone()),
                target: EndpointSpec::Foreign {
                    source_group_id: GROUP_A.to_string(),
                    endpoint_name: "Carol".to_string(),
                },
                group_id: GROUP_LAYER.to_string(),
                fact: "Alice knows Carol".to_string(),
                fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
                valid_at: None,
                relation_type: None,
            },
            TS,
        )
        .unwrap();

        (bob.uuid, carol.uuid)
    };

    let state = make_state(Arc::clone(&db), None);
    let status_before = dispatch_val(1, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_eq!(
        status_before["result"]["cross_group_pointers"]["bound"], 2,
        "{status_before}"
    );

    // Row-scoped purge of only Bob — Carol (same group, not named) must remain resolvable.
    {
        let conn = db.connect().unwrap();
        group_purge::purge_group_rows(&conn, GROUP_A, std::slice::from_ref(&bob_uuid), TS).unwrap();
    }

    let conn = db.connect().unwrap();
    assert!(
        conn.get_entity_by_uuid(&bob_uuid).unwrap().is_none(),
        "Bob must be deleted"
    );
    assert!(
        conn.get_entity_by_uuid(&carol_uuid).unwrap().is_some(),
        "Carol (same group, not named) must survive the row-scoped purge"
    );

    let status_after = dispatch_val(2, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_eq!(
        status_after["result"]["cross_group_pointers"]["unbound"], 1,
        "only Bob's pointer should have transitioned to unbound: {status_after}"
    );
    assert_eq!(
        status_after["result"]["cross_group_pointers"]["bound"], 1,
        "Carol's pointer must still resolve Bound — `rebind_pointers_forced`'s real \
         per-pointer re-resolution must not treat a partial (row-scoped) purge as though the \
         whole source group were emptied: {status_after}"
    );
}
