//! FR-level integration tests for the direct assertion API (issue #379):
//! `knowledge_assert_entity` / `knowledge_assert_relationship`.
//!
//! `crates/core/tests/ipc_parity.rs` already covers the two tools' basic wire-shape
//! correctness (create, idempotent re-assert, entity_uuid lookup, Merged-tombstone-by-name
//! forwarding, cross-group name refusal). This file covers the FRs that need dedicated
//! fixtures: by-uuid Merged forwarding, a dead-end Merged chain, the group-scoped edge upsert
//! (FR-016), the embedder-unavailable fallback, `summary`'s full-replace-on-update semantics,
//! `valid_at` format handling, WAL-replay survival (SC-006), and SC-008's structural-
//! determinism fixture.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{CountingEmbedder, MockEmbedder, OaiEmbedder},
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    pointer,
    telemetry::{NoopSink, TelemetrySink},
    Db, EntityRow, RelatesToEdge, WalReplayer, WalWriter,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const DIM: usize = 4;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("assert.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

/// A working embedder (`MockEmbedder`) — the default for tests not specifically exercising the
/// embedder-unavailable fallback.
fn make_state(db: Arc<Db>) -> Arc<AppState> {
    make_state_with(
        db,
        Arc::new(MockEmbedder::new(DIM)),
        None,
        "test.db".to_string(),
    )
}

/// A deliberately-unreachable embedder (an explicit `OaiEmbedder::new_http` endpoint with no
/// server listening in the test environment) — for exercising FR-012/FR-021's embedder-unavailable
/// fallback (a zero-vector embedding + a populated `embedding_warning`). Configured with
/// `dim: DIM` (matching the test DB's schema dimension) so the fallback's zero vector is the
/// right length for `make_db`'s `FLOAT[DIM]` columns — `from_env()`'s default dim (768) would
/// mismatch this test schema's dim (4) and fail to bind for an unrelated reason.
fn make_state_failing_embedder(db: Arc<Db>) -> Arc<AppState> {
    make_state_with(
        db,
        Arc::new(OaiEmbedder::new_http(
            "http://127.0.0.1:1/v1/embeddings",
            "unused-model",
            DIM,
        )),
        None,
        "test.db".to_string(),
    )
}

/// A state with a real, live `WalWriter` so dispatch calls actually append WAL lines — needed
/// for the WAL-replay-survival test (SC-006).
fn make_state_with_live_wal(db: Arc<Db>, wal_dir: PathBuf, db_path: String) -> Arc<AppState> {
    let wal_writer = WalWriter::new(&wal_dir, 10_000, 5 * 1024 * 1024).ok();
    make_state_with(
        db,
        Arc::new(MockEmbedder::new(DIM)),
        Some((wal_dir, wal_writer)),
        db_path,
    )
}

/// A `MockEmbedder` wrapped in `CountingEmbedder` — for asserting the embedder is (or isn't)
/// called at all, not just what it returns (issue #444).
fn make_state_counting(db: Arc<Db>) -> (Arc<AppState>, Arc<CountingEmbedder>) {
    let inner: Arc<dyn lcg_core::Embedder> = Arc::new(MockEmbedder::new(DIM));
    let counting = Arc::new(CountingEmbedder::new(inner));
    let embedder: Arc<dyn lcg_core::Embedder> = counting.clone();
    let state = make_state_with(db, embedder, None, "test.db".to_string());
    (state, counting)
}

fn make_state_with(
    db: Arc<Db>,
    embedder: Arc<dyn lcg_core::Embedder>,
    wal: Option<(PathBuf, Option<WalWriter>)>,
    db_path: String,
) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let (wal_root, wal_writer) = match wal {
        Some((dir, writer)) => (Some(dir), writer),
        None => (None, None),
    };
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder,
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(tokio::sync::RwLock::new(())),
        sink,
        db_path,
        wal_root,
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
        embedding_cache: std::sync::Arc::new(lcg_core::EmbeddingCache::new()),
    })
}

fn make_entity(
    uuid: &str,
    name: &str,
    group_id: &str,
    labels: Vec<String>,
    attrs: &str,
) -> EntityRow {
    EntityRow {
        uuid: uuid.to_string(),
        name: name.to_string(),
        group_id: group_id.to_string(),
        labels,
        created_at: "2026-01-01 00:00:00".to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: "seed".to_string(),
        attributes: attrs.to_string(),
        ..Default::default()
    }
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

fn assert_ok_resp(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc field wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("result").is_some(), "expected result, got: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
}

fn assert_err_resp(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc field wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("error").is_some(), "expected error field: {v}");
}

// ── FR-008/FR-009: Merged-tombstone forward resolution ─────────────────────────

/// `entity_uuid` naming a `Merged` tombstone forwards to the canonical and updates it, not the
/// tombstone (FR-008), mirroring `ipc_parity.rs`'s by-name coverage of the same rule.
#[tokio::test]
async fn assert_entity_by_uuid_forwards_through_merged_tombstone_to_canonical() {
    let (db, _dir) = make_db(DIM);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity(
            "canonical-uuid-1",
            "Acme Corp",
            "liminis",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
        conn.insert_entity(&make_entity(
            "tombstone-uuid-1",
            "Acme",
            "liminis",
            vec!["Entity".to_string(), "Merged".to_string()],
            &pointer::write_merged_into("{}", "canonical-uuid-1"),
        ))
        .unwrap();
    }
    let state = make_state(db);
    let v = dispatch_val(
        1,
        "knowledge_assert_entity",
        json!({
            "name": "Acme Corp",
            "entity_uuid": "tombstone-uuid-1",
            "group_id": "liminis",
            "summary": "reasserted via uuid",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 1);
    assert_eq!(
        v["result"]["entity_uuid"], "canonical-uuid-1",
        "must forward through the Merged tombstone to the canonical: {v}"
    );

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    let canonical = conn
        .get_entity_by_uuid("canonical-uuid-1")
        .unwrap()
        .unwrap();
    assert_eq!(canonical.summary, "reasserted via uuid");
    let tombstone = conn
        .get_entity_by_uuid("tombstone-uuid-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        tombstone.summary, "seed",
        "the tombstone itself must be left untouched"
    );
}

/// FR-009: a `Merged` tombstone with no `merged_into` recorded (a dead end) fails the call
/// rather than creating a new entity under the stale name or guessing a target.
#[tokio::test]
async fn assert_entity_dead_end_merged_chain_errors_without_writing() {
    let (db, _dir) = make_db(DIM);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity(
            "dangling-tombstone",
            "Ghost Co",
            "liminis",
            vec!["Entity".to_string(), "Merged".to_string()],
            "{}", // no merged_into recorded
        ))
        .unwrap();
    }
    let state = make_state(db);
    let v = dispatch_val(
        2,
        "knowledge_assert_entity",
        json!({"name": "Ghost Co", "group_id": "liminis"}),
        Arc::clone(&state),
    )
    .await;
    assert_err_resp(&v, 2);

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    assert_eq!(
        conn.count_entities_by_name_ci("Ghost Co", "liminis")
            .unwrap(),
        1,
        "must not create a duplicate or new entity on a dead-end Merged chain"
    );
}

// ── FR-011: entity_uuid rename must not collide with another entity's name ─────

/// Regression guard: the rename-collision check must not mistake reusing a `Merged` tombstone's
/// own name for a collision. Asserting by the tombstone's name forwards `existing` to the
/// canonical (FR-008), whose current name differs from the caller-supplied name — an implicit
/// rename onto a name that *is* already in the group, but only via an inactive tombstone, which
/// must not block the assertion.
#[tokio::test]
async fn assert_entity_by_name_merged_forward_rename_is_not_a_self_collision() {
    let (db, _dir) = make_db(DIM);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity(
            "canonical-selfcollision",
            "Acme Corp",
            "liminis",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
        conn.insert_entity(&make_entity(
            "tombstone-selfcollision",
            "Acme",
            "liminis",
            vec!["Entity".to_string(), "Merged".to_string()],
            &pointer::write_merged_into("{}", "canonical-selfcollision"),
        ))
        .unwrap();
    }
    let state = make_state(db);
    // Assert by the tombstone's own name "Acme" — forwards to the canonical
    // ("Acme Corp"), an implicit rename onto the tombstone's inactive name.
    let v = dispatch_val(
        19,
        "knowledge_assert_entity",
        json!({"name": "Acme", "group_id": "liminis"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 19);
    assert_eq!(v["result"]["entity_uuid"], "canonical-selfcollision");

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    let canonical = conn
        .get_entity_by_uuid("canonical-selfcollision")
        .unwrap()
        .unwrap();
    assert_eq!(
        canonical.name, "Acme",
        "the assertion's name must be applied to the canonical entity"
    );
}

/// The `entity_uuid` path can be asked to rename the resolved entity (the caller-supplied
/// `name` need not match the entity's current name). Renaming it onto a name already held by a
/// *different* active entity in the same group must be rejected, not silently applied — an
/// unguarded rename would leave two active entities sharing one normalized `(name, group_id)`
/// key, breaking FR-011's idempotency invariant (one of the two becomes unreachable by future
/// by-name resolution, including `knowledge_assert_relationship`'s endpoint resolution).
#[tokio::test]
async fn assert_entity_by_uuid_rejects_rename_that_collides_with_another_entity() {
    let (db, _dir) = make_db(DIM);
    let state = make_state(db);

    let alice = dispatch_val(
        14,
        "knowledge_assert_entity",
        json!({"name": "Alice", "group_id": "liminis"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&alice, 14);
    let alice_uuid = alice["result"]["entity_uuid"].as_str().unwrap().to_string();

    let bob = dispatch_val(
        15,
        "knowledge_assert_entity",
        json!({"name": "Bob", "group_id": "liminis"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&bob, 15);

    // Attempt to rename Alice (by entity_uuid) onto Bob's name.
    let renamed = dispatch_val(
        16,
        "knowledge_assert_entity",
        json!({"name": "Bob", "entity_uuid": alice_uuid, "group_id": "liminis"}),
        Arc::clone(&state),
    )
    .await;
    assert_err_resp(&renamed, 16);

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    assert_eq!(
        conn.get_entity_by_uuid(&alice_uuid).unwrap().unwrap().name,
        "Alice",
        "the rejected rename must leave Alice's name untouched"
    );
    assert_eq!(
        conn.count_entities_by_name_ci("Bob", "liminis").unwrap(),
        1,
        "must not end up with two active entities named Bob"
    );
}

/// A rename via `entity_uuid` onto a genuinely unused name in the group is not a collision and
/// must succeed, updating the entity's name in place.
#[tokio::test]
async fn assert_entity_by_uuid_rename_to_unused_name_succeeds() {
    let (db, _dir) = make_db(DIM);
    let state = make_state(db);

    let created = dispatch_val(
        17,
        "knowledge_assert_entity",
        json!({"name": "Old Name", "group_id": "liminis"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&created, 17);
    let uuid = created["result"]["entity_uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let renamed = dispatch_val(
        18,
        "knowledge_assert_entity",
        json!({"name": "New Name", "entity_uuid": uuid, "group_id": "liminis"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&renamed, 18);
    assert_eq!(renamed["result"]["entity_uuid"], uuid);

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    assert_eq!(
        conn.get_entity_by_uuid(&uuid).unwrap().unwrap().name,
        "New Name"
    );
}

// ── FR-016/FR-017: group-scoped edge upsert (ADR-0368/ADR-0371) ────────────────

/// Asserting an edge in one `group_id` must never match, update, or overwrite an edge with the
/// same predicate and endpoint UUIDs that exists under a *different* `group_id` — the exact
/// unsafe shape ADR-0368 identified. This can happen even though the endpoints themselves live
/// in one group (`liminis`), because an edge's own `group_id` (the "layer" it belongs to) can
/// differ from its endpoints' group — e.g. a `knowledge_add_cross_group_edge`-style layer edge.
#[tokio::test]
async fn assert_relationship_edge_upsert_never_crosses_groups() {
    let (db, _dir) = make_db(DIM);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity(
            "alice-fr016",
            "Alice",
            "liminis",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
        conn.insert_entity(&make_entity(
            "bob-fr016",
            "Bob",
            "liminis",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
        // Pre-seed a "layer-x"-scoped edge between the same two UUIDs, same predicate — this
        // must survive untouched by an assert into a different group_id.
        conn.insert_relates_to_edge(&RelatesToEdge {
            uuid: "layer-edge-fr016".to_string(),
            name: "KNOWS".to_string(),
            source_node_uuid: "alice-fr016".to_string(),
            target_node_uuid: "bob-fr016".to_string(),
            group_id: "layer-x".to_string(),
            fact: "layer-x's own fact about Alice and Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            created_at: "2026-01-01 00:00:00".to_string(),
            valid_at: None,
            invalid_at: None,
            attributes: "{}".to_string(),
            relation_type: None,
            episode_uuids: vec![],
            source_descriptions: vec![],
        })
        .unwrap();
    }
    let state = make_state(db);

    let v = dispatch_val(
        3,
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice",
            "target_name": "Bob",
            "predicate": "KNOWS",
            "group_id": "liminis",
            "fact": "liminis's own fact about Alice and Bob",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 3);
    let new_edge_uuid = v["result"]["edge_uuid"].as_str().unwrap().to_string();
    assert_ne!(
        new_edge_uuid, "layer-edge-fr016",
        "must create a distinct edge, not match the layer-x edge: {v}"
    );
    assert_eq!(v["result"]["created"], true);

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    assert_eq!(
        conn.count_relates_to_edges().unwrap(),
        2,
        "the pre-seeded layer-x edge and the new liminis edge must both exist"
    );
    let layer_edge = conn.get_edge_by_uuid("layer-edge-fr016").unwrap().unwrap();
    assert_eq!(
        layer_edge.fact, "layer-x's own fact about Alice and Bob",
        "the layer-x edge must be left completely untouched (FR-016/SC-005)"
    );
    assert!(
        conn.find_active_relates_to_uuid("alice-fr016", "bob-fr016", "KNOWS", "liminis")
            .unwrap()
            .is_some(),
        "expected a KNOWS edge scoped to liminis"
    );
}

// ── FR-012/FR-021: embedder-unavailable fallback ────────────────────────────────

/// If the configured embedder is unavailable, `knowledge_assert_entity` still succeeds, storing
/// a zero-vector `name_embedding` and surfacing a non-null `embedding_warning`.
#[tokio::test]
async fn assert_entity_embedder_unavailable_still_succeeds_with_warning() {
    let (db, _dir) = make_db(DIM);
    let state = make_state_failing_embedder(db);
    let v = dispatch_val(
        4,
        "knowledge_assert_entity",
        json!({"name": "Offline Corp", "group_id": "liminis"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 4);
    assert!(
        v["result"]["embedding_warning"].is_string(),
        "expected a populated embedding_warning: {v}"
    );
    let uuid = v["result"]["entity_uuid"].as_str().unwrap();

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    let row = conn.get_entity_by_uuid(uuid).unwrap().unwrap();
    assert_eq!(
        row.name_embedding,
        vec![0.0f32; DIM],
        "expected a zero-vector stored name_embedding (Entity.name_embedding is a fixed-size \
         FLOAT[N] column — a literal empty vector cannot bind)"
    );
}

/// Same fallback for `knowledge_assert_entity`'s `summary_embedding` (issue #470): a non-empty
/// `summary` that the embedder fails on still succeeds, stores a zero-vector `summary_embedding`,
/// and surfaces its own distinct `embedding_warning` text (appended alongside `name_embedding`'s,
/// per `handlers.rs`'s `warnings.join("; ")`).
#[tokio::test]
async fn assert_entity_summary_embedder_unavailable_still_succeeds_with_warning() {
    let (db, _dir) = make_db(DIM);
    let state = make_state_failing_embedder(db);
    let v = dispatch_val(
        41,
        "knowledge_assert_entity",
        json!({
            "name": "Offline Summary Corp",
            "summary": "a non-empty summary the embedder will fail on",
            "group_id": "liminis",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 41);
    let warning = v["result"]["embedding_warning"]
        .as_str()
        .expect("expected a populated embedding_warning");
    assert!(
        warning.contains("summary_embedding"),
        "expected the summary_embedding failure to be named in embedding_warning: {warning}"
    );
    let uuid = v["result"]["entity_uuid"].as_str().unwrap();

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    let rows = conn
        .cypher_query(&format!(
            "MATCH (n:Entity {{uuid: '{uuid}'}}) RETURN n.summary_embedding"
        ))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0], "[0,0,0,0]",
        "expected a zero-vector stored summary_embedding on embedder failure, matching \
         name_embedding's same fallback convention: {:?}",
        rows[0][0]
    );
}

/// Same fallback for `knowledge_assert_relationship`'s `fact_embedding` (FR-021).
#[tokio::test]
async fn assert_relationship_embedder_unavailable_still_succeeds_with_warning() {
    let (db, _dir) = make_db(DIM);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity(
            "alice-fr021",
            "Alice",
            "liminis",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
        conn.insert_entity(&make_entity(
            "bob-fr021",
            "Bob",
            "liminis",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
    }
    let state = make_state_failing_embedder(db);
    let v = dispatch_val(
        5,
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice",
            "target_name": "Bob",
            "predicate": "KNOWS",
            "group_id": "liminis",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 5);
    assert!(
        v["result"]["embedding_warning"].is_string(),
        "expected a populated embedding_warning: {v}"
    );
    let edge_uuid = v["result"]["edge_uuid"].as_str().unwrap();

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    // get_edge_by_uuid's query doesn't select fact_embedding at all (it's not needed by that
    // call site); get_full_edges_for_entity does, so use that to actually observe the stored
    // vector.
    let edge = conn
        .get_full_edges_for_entity("alice-fr021")
        .unwrap()
        .into_iter()
        .find(|e| e.uuid == edge_uuid)
        .expect("expected the asserted edge to be found via get_full_edges_for_entity");
    assert_eq!(
        edge.fact_embedding,
        vec![0.0f32; DIM],
        "expected a zero-vector stored fact_embedding (RelatesToNode_.fact_embedding is a \
         fixed-size FLOAT[N] column — a literal empty vector cannot bind)"
    );
}

// ── summary full-replace-on-update semantics ────────────────────────────────────

/// `summary` round-trips on create, and a re-assert that omits it fully clears the previously
/// set value (full-replace-on-update, matching `attributes`/`labels`) rather than leaving the
/// prior value untouched.
#[tokio::test]
async fn assert_entity_summary_round_trips_and_clears_on_omit() {
    let (db, _dir) = make_db(DIM);
    let state = make_state(db);
    let created = dispatch_val(
        6,
        "knowledge_assert_entity",
        json!({"name": "Carol", "group_id": "liminis", "summary": "runs engineering"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&created, 6);
    let uuid = created["result"]["entity_uuid"]
        .as_str()
        .unwrap()
        .to_string();

    {
        let db2 = state.db.load_full().unwrap();
        let conn = db2.connect().unwrap();
        let row = conn.get_entity_by_uuid(&uuid).unwrap().unwrap();
        assert_eq!(row.summary, "runs engineering");
    }

    // Re-assert with no `summary` key at all.
    let updated = dispatch_val(
        7,
        "knowledge_assert_entity",
        json!({"name": "Carol", "group_id": "liminis"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&updated, 7);
    assert_eq!(updated["result"]["entity_uuid"], uuid);

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    let row = conn.get_entity_by_uuid(&uuid).unwrap().unwrap();
    assert_eq!(
        row.summary, "",
        "omitting summary on re-assert must clear the prior value, not preserve it"
    );
}

// ── FR-022: valid_at format handling ────────────────────────────────────────────

/// `knowledge_assert_relationship`'s `valid_at` accepts both RFC-3339 and lbug's space-delimited
/// read-back format, and rejects an unparseable value cleanly (no lbug Binder exception).
#[tokio::test]
async fn assert_relationship_valid_at_accepts_both_formats_rejects_bad() {
    let (db, _dir) = make_db(DIM);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity(
            "alice-fr022",
            "Alice",
            "liminis",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
        conn.insert_entity(&make_entity(
            "bob-fr022",
            "Bob",
            "liminis",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
    }
    let state = make_state(db);

    let rfc3339 = dispatch_val(
        8,
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice", "target_name": "Bob", "predicate": "KNOWS",
            "group_id": "liminis", "valid_at": "2026-06-24T10:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rfc3339, 8);

    let space_format = dispatch_val(
        9,
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice", "target_name": "Bob", "predicate": "KNOWS",
            "group_id": "liminis", "valid_at": "2026-06-24 10:00:00",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&space_format, 9);

    let bad = dispatch_val(
        10,
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice", "target_name": "Bob", "predicate": "KNOWS",
            "group_id": "liminis", "valid_at": "not-a-timestamp",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_err_resp(&bad, 10);
}

// ── SC-006: WAL-replay survival ─────────────────────────────────────────────────

/// Entities and relationships created via either assert tool are flushed through the same WAL
/// path as every other write handler and survive a fresh replay (SC-006/FR-023).
#[tokio::test]
async fn assert_entity_and_relationship_survive_wal_replay() {
    let (db, dir) = make_db(DIM);
    let wal_dir = TempDir::new().unwrap();
    let db_path = dir.path().join("assert.db").to_str().unwrap().to_string();
    let state = make_state_with_live_wal(db, wal_dir.path().to_path_buf(), db_path);

    let alice = dispatch_val(
        11,
        "knowledge_assert_entity",
        json!({"name": "Alice", "group_id": "liminis", "summary": "engineer"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&alice, 11);
    dispatch_val(
        12,
        "knowledge_assert_entity",
        json!({"name": "Bob", "group_id": "liminis", "summary": "designer"}),
        Arc::clone(&state),
    )
    .await;
    let rel = dispatch_val(
        13,
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice", "target_name": "Bob", "predicate": "KNOWS",
            "group_id": "liminis",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rel, 13);

    // Replay the recorded WAL onto a fresh DB.
    let db2_dir = TempDir::new().unwrap();
    let db2 = Db::open(db2_dir.path().join("replay.db").to_str().unwrap()).unwrap();
    {
        let conn2 = db2.connect().unwrap();
        conn2.init_schema(DIM).unwrap();
        let stats = WalReplayer::new(wal_dir.path()).replay(&conn2).unwrap();
        assert!(
            stats.lines_replayed > 0,
            "WAL replay must process some lines"
        );
    }

    let conn2 = db2.connect().unwrap();
    assert_eq!(
        conn2.count_entities_by_name_ci("Alice", "liminis").unwrap(),
        1
    );
    assert_eq!(
        conn2.count_entities_by_name_ci("Bob", "liminis").unwrap(),
        1
    );
    assert_eq!(conn2.count_relates_to_edges().unwrap(), 1);
}

// ── SC-008/Story 3: structural-determinism fixture ──────────────────────────────

/// Asserting the same sequence of entities and relationships into two independently-created
/// fresh graph instances produces identical names, attributes, and graph topology (which entity
/// connects to which via which predicate) — never identical UUIDs, which are `Uuid::new_v4()`
/// per instance and are not comparable across runs (see the corrected User Story 3, AC1).
#[tokio::test]
async fn structural_determinism_across_independent_instances() {
    async fn build_fixture(state: Arc<AppState>) -> (String, Value, Value) {
        let alice = dispatch_val(
            20,
            "knowledge_assert_entity",
            json!({
                "name": "Alice", "group_id": "liminis",
                "attributes": {"role": "engineer"},
            }),
            Arc::clone(&state),
        )
        .await;
        let bob = dispatch_val(
            21,
            "knowledge_assert_entity",
            json!({
                "name": "Bob", "group_id": "liminis",
                "attributes": {"role": "designer"},
            }),
            Arc::clone(&state),
        )
        .await;
        let rel = dispatch_val(
            22,
            "knowledge_assert_relationship",
            json!({
                "source_name": "Alice", "target_name": "Bob", "predicate": "KNOWS",
                "group_id": "liminis", "fact": "Alice KNOWS Bob",
            }),
            Arc::clone(&state),
        )
        .await;
        let db = state.db.load_full().unwrap();
        let conn = db.connect().unwrap();
        let alice_uuid = alice["result"]["entity_uuid"].as_str().unwrap().to_string();
        let bob_uuid = bob["result"]["entity_uuid"].as_str().unwrap().to_string();
        let alice_row = conn.get_entity_by_uuid(&alice_uuid).unwrap().unwrap();
        let bob_row = conn.get_entity_by_uuid(&bob_uuid).unwrap().unwrap();
        let edge_uuid = rel["result"]["edge_uuid"].as_str().unwrap().to_string();
        let edge_row = conn.get_edge_by_uuid(&edge_uuid).unwrap().unwrap();
        // Topology captured as "source_name -> predicate -> target_name" — deliberately
        // UUID-free, since UUIDs are per-instance and never compared (SC-008).
        let topology = format!(
            "{} -> {} -> {}",
            alice_row.name, edge_row.name, bob_row.name
        );
        (
            topology,
            json!({"name": alice_row.name, "attributes": alice_row.attributes}),
            json!({"name": bob_row.name, "attributes": bob_row.attributes}),
        )
    }

    let (db1, _dir1) = make_db(DIM);
    let (db2, _dir2) = make_db(DIM);
    let state1 = make_state(db1);
    let state2 = make_state(db2);

    let (topology1, alice1, bob1) = build_fixture(state1).await;
    let (topology2, alice2, bob2) = build_fixture(state2).await;

    assert_eq!(
        topology1, topology2,
        "graph topology must be identical across instances"
    );
    assert_eq!(
        alice1, alice2,
        "Alice's name/attributes must be identical across instances"
    );
    assert_eq!(
        bob1, bob2,
        "Bob's name/attributes must be identical across instances"
    );
}

// ── Issue #383: applied_seq advances for assert-API-only writes ─────────────────

/// User Story 1 / SC-001: writing entities and relationships purely through the assert API (no
/// episode ingest at all) must make `knowledge_status`'s `wal_groups` entry for that group
/// report a real, non-null `applied_seq` consistent with `max_seq` — the exact reproduction in
/// issue #383's Background (`applied_seq: null` against a real, non-zero `max_seq`).
#[tokio::test]
async fn knowledge_status_reflects_assert_only_writes() {
    const GROUP: &str = "assert-only-a";
    let (db, dir) = make_db(DIM);
    let wal_dir = TempDir::new().unwrap();
    let db_path = dir.path().join("assert.db").to_str().unwrap().to_string();
    let state = make_state_with_live_wal(db, wal_dir.path().to_path_buf(), db_path);

    // Fresh group with no prior writes: must not appear in wal_groups at all yet.
    let status0 = dispatch_val(1, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&status0, 1);
    assert!(
        status0["result"]["wal_groups"].get(GROUP).is_none(),
        "group must not appear in wal_groups before it has received any writes: {status0}"
    );

    let alice = dispatch_val(
        2,
        "knowledge_assert_entity",
        json!({"name": "Alice", "group_id": GROUP, "summary": "engineer"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&alice, 2);
    let bob = dispatch_val(
        3,
        "knowledge_assert_entity",
        json!({"name": "Bob", "group_id": GROUP, "summary": "designer"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&bob, 3);
    let rel = dispatch_val(
        4,
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice", "target_name": "Bob", "predicate": "KNOWS",
            "group_id": GROUP,
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rel, 4);

    let status1 = dispatch_val(5, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&status1, 5);
    let applied1 = status1["result"]["wal_groups"][GROUP]["applied_seq"].as_u64();
    let max1 = status1["result"]["wal_groups"][GROUP]["max_seq"].as_u64();
    assert!(
        applied1.is_some(),
        "SC-001: applied_seq must be non-null after assert-API-only writes, not null \
         regardless of max_seq: {status1}"
    );
    assert!(
        max1.is_some(),
        "max_seq must reflect the real WAL content: {status1}"
    );
    assert_eq!(
        applied1, max1,
        "applied_seq must be consistent with max_seq (every mutation here succeeded, \
         nothing filtered)"
    );

    // Further assert-API writes must advance applied_seq further still.
    let carol = dispatch_val(
        6,
        "knowledge_assert_entity",
        json!({"name": "Carol", "group_id": GROUP, "summary": "manager"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&carol, 6);

    let status2 = dispatch_val(7, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&status2, 7);
    let applied2 = status2["result"]["wal_groups"][GROUP]["applied_seq"].as_u64();
    assert!(
        applied2.unwrap() > applied1.unwrap(),
        "additional assert-API writes must advance applied_seq further: {status1} -> {status2}"
    );
}

/// User Story 3 / SC-002: a write to one group must never move another group's reported
/// `applied_seq` — the per-group isolation issue #378 established, which this fix must preserve
/// rather than regress back to a single shared counter.
#[tokio::test]
async fn assert_api_write_to_one_group_leaves_another_groups_applied_seq_untouched() {
    const GROUP_A: &str = "iso-a";
    const GROUP_B: &str = "iso-b";
    let (db, dir) = make_db(DIM);
    let wal_dir = TempDir::new().unwrap();
    let db_path = dir.path().join("assert.db").to_str().unwrap().to_string();
    let state = make_state_with_live_wal(db, wal_dir.path().to_path_buf(), db_path);

    // Establish group B's baseline position first.
    let b_seed = dispatch_val(
        1,
        "knowledge_assert_entity",
        json!({"name": "B-Seed", "group_id": GROUP_B}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&b_seed, 1);

    let status_before = dispatch_val(2, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&status_before, 2);
    let b_before = status_before["result"]["wal_groups"][GROUP_B].clone();
    assert!(
        b_before["applied_seq"].as_u64().is_some(),
        "group B's baseline write must have set a real applied_seq: {status_before}"
    );

    // Write only to group A.
    let a1 = dispatch_val(
        3,
        "knowledge_assert_entity",
        json!({"name": "A-One", "group_id": GROUP_A}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&a1, 3);
    let a2 = dispatch_val(
        4,
        "knowledge_assert_entity",
        json!({"name": "A-Two", "group_id": GROUP_A}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&a2, 4);

    let status_after = dispatch_val(5, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&status_after, 5);
    let b_after = status_after["result"]["wal_groups"][GROUP_B].clone();
    assert_eq!(
        b_before, b_after,
        "group B's wal_groups entry must be byte-identical after a write to group A only \
         (SC-002): before={b_before} after={b_after}"
    );

    // Sanity: group A's own position did actually advance, so this isn't vacuously true.
    let a_applied = status_after["result"]["wal_groups"][GROUP_A]["applied_seq"]
        .as_u64()
        .unwrap();
    assert!(
        a_applied > 0,
        "group A must show real progress from its 2 writes: {status_after}"
    );
}

// ── issue #444: existence is resolved before the embedder is ever called ───────

/// User Story 1 / SC-001: re-asserting an already-known entity by name updates it via
/// `update_entity_core` without ever calling the embedder for `name` or `summary`, since
/// neither value is persisted on the update path.
#[tokio::test]
async fn assert_entity_reassert_by_name_skips_embedder() {
    let (db, _dir) = make_db(DIM);
    let (state, counting) = make_state_counting(db);

    let created = dispatch_val(
        1,
        "knowledge_assert_entity",
        json!({"name": "Alice", "group_id": "g1", "summary": "first summary"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&created, 1);
    assert_eq!(created["result"]["created"], true);
    // One call for `name`, one for the non-empty `summary`.
    assert_eq!(
        counting.call_count(),
        2,
        "create must embed both name and summary exactly once each"
    );

    let reasserted = dispatch_val(
        2,
        "knowledge_assert_entity",
        json!({"name": "Alice", "group_id": "g1", "summary": "updated summary"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&reasserted, 2);
    assert_eq!(reasserted["result"]["created"], false);
    assert_eq!(
        counting.call_count(),
        2,
        "re-asserting a known entity by name must not call the embedder at all"
    );

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    let uuid = reasserted["result"]["entity_uuid"].as_str().unwrap();
    let row = conn.get_entity_by_uuid(uuid).unwrap().unwrap();
    assert_eq!(
        row.summary, "updated summary",
        "the update must still apply via update_entity_core"
    );
}

/// User Story 1, Acceptance Scenario 3 / SC-001: resolving an existing entity via `entity_uuid`
/// (rather than by name) also skips the embedder — matching the by-name update case.
#[tokio::test]
async fn assert_entity_reassert_by_uuid_skips_embedder() {
    let (db, _dir) = make_db(DIM);
    let (state, counting) = make_state_counting(db);

    let created = dispatch_val(
        1,
        "knowledge_assert_entity",
        json!({"name": "Bob", "group_id": "g1", "summary": "seed"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&created, 1);
    let uuid = created["result"]["entity_uuid"]
        .as_str()
        .unwrap()
        .to_string();
    let count_after_create = counting.call_count();
    assert!(count_after_create > 0);

    let reasserted = dispatch_val(
        2,
        "knowledge_assert_entity",
        json!({
            "name": "Bob",
            "entity_uuid": uuid,
            "group_id": "g1",
            "summary": "changed via uuid",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&reasserted, 2);
    assert_eq!(reasserted["result"]["created"], false);
    assert_eq!(
        counting.call_count(),
        count_after_create,
        "re-asserting a known entity by entity_uuid must not call the embedder"
    );
}

/// User Story 2 / SC-002: re-asserting an already-active relationship updates it via
/// `update_relates_to_core` without ever calling the embedder for `fact`.
#[tokio::test]
async fn assert_relationship_reassert_skips_embedder() {
    let (db, _dir) = make_db(DIM);
    {
        // Seed the two endpoints directly (bypassing dispatch) so only the assert_relationship
        // calls themselves are visible in the embedder's call count.
        let conn = db.connect().unwrap();
        conn.insert_entity(&make_entity(
            "alice-444",
            "Alice",
            "g1",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
        conn.insert_entity(&make_entity(
            "bob-444",
            "Bob",
            "g1",
            vec!["Entity".to_string()],
            "{}",
        ))
        .unwrap();
    }
    let (state, counting) = make_state_counting(db);

    let created = dispatch_val(
        1,
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice",
            "target_name": "Bob",
            "predicate": "KNOWS",
            "group_id": "g1",
            "fact": "Alice knows Bob",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&created, 1);
    assert_eq!(created["result"]["created"], true);
    assert_eq!(
        counting.call_count(),
        1,
        "create must embed fact exactly once"
    );

    let reasserted = dispatch_val(
        2,
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice",
            "target_name": "Bob",
            "predicate": "KNOWS",
            "group_id": "g1",
            "fact": "Alice still knows Bob",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&reasserted, 2);
    assert_eq!(reasserted["result"]["created"], false);
    assert_eq!(
        counting.call_count(),
        1,
        "re-asserting a known relationship must not call the embedder"
    );

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    let uuid = reasserted["result"]["edge_uuid"].as_str().unwrap();
    let edge = conn.get_edge_by_uuid(uuid).unwrap().unwrap();
    assert_eq!(
        edge.fact, "Alice still knows Bob",
        "the update must still apply via update_relates_to_core"
    );
}

/// Edge Case: a resolved-existing-row path that ends in an error (the rename-collision check)
/// must not have called the embedder either — FR-006's general "resolved existing → no embed"
/// rule applies even when the resolved branch subsequently errors out.
#[tokio::test]
async fn assert_entity_rename_collision_rejected_skips_embedder() {
    let (db, _dir) = make_db(DIM);
    let (state, counting) = make_state_counting(db);

    let alice = dispatch_val(
        1,
        "knowledge_assert_entity",
        json!({"name": "Alice", "group_id": "g1"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&alice, 1);
    let alice_uuid = alice["result"]["entity_uuid"].as_str().unwrap().to_string();

    let bob = dispatch_val(
        2,
        "knowledge_assert_entity",
        json!({"name": "Bob", "group_id": "g1"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&bob, 2);

    let count_before_collision = counting.call_count();

    // Attempt to rename Alice (by entity_uuid) onto Bob's already-active name.
    let rejected = dispatch_val(
        3,
        "knowledge_assert_entity",
        json!({"name": "Bob", "entity_uuid": alice_uuid, "group_id": "g1"}),
        Arc::clone(&state),
    )
    .await;
    assert_err_resp(&rejected, 3);
    assert_eq!(
        counting.call_count(),
        count_before_collision,
        "a rejected rename-collision must not have called the embedder"
    );
}
