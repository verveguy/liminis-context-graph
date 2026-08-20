/// Integration tests for knowledge_backfill_relation_types — FR-012, FR-013, SC-005.
///
/// These tests require a real WAL writer (unlike parity tests which set wal_writer: None)
/// to verify that backfill mutations are WAL-durable and survive a rebuild_from_wal replay.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    telemetry::{NoopSink, TelemetrySink},
    EntityRow, RelatesToEdge, WalReplayer, WalWriter,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const DIM: usize = 4;
const TS: &str = "2026-01-01T00:00:00Z";
const GRP: &str = "liminis";

// ── Helpers ───────────────────────────────────────────────────────────────────

fn open_db(dir: &TempDir) -> Arc<Db> {
    let db = Arc::new(Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
    }
    db
}

fn make_state_with_wal(db: Arc<Db>, wal_dir: &std::path::Path) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = WalWriter::new(wal_dir, 10_000, 0).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_root: Some(wal_dir.to_path_buf()),
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
    })
}

fn make_entity(name: &str) -> EntityRow {
    make_entity_in_group(name, GRP)
}

fn make_entity_in_group(name: &str, group_id: &str) -> EntityRow {
    EntityRow {
        uuid: Uuid::new_v4().to_string(),
        name: name.to_string(),
        group_id: group_id.to_string(),
        labels: vec!["Entity".to_string()],
        created_at: TS.to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: format!("summary of {name}"),
        attributes: "{}".to_string(),
        ..Default::default()
    }
}

fn make_edge(src: &str, dst: &str, rt: Option<&str>, fact: &str) -> RelatesToEdge {
    make_edge_in_group(src, dst, rt, fact, GRP)
}

fn make_edge_in_group(
    src: &str,
    dst: &str,
    rt: Option<&str>,
    fact: &str,
    group_id: &str,
) -> RelatesToEdge {
    RelatesToEdge {
        uuid: Uuid::new_v4().to_string(),
        name: format!("{src} → {dst}"),
        source_node_uuid: src.to_string(),
        target_node_uuid: dst.to_string(),
        group_id: group_id.to_string(),
        fact: fact.to_string(),
        fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
        created_at: TS.to_string(),
        valid_at: None,
        invalid_at: None,
        attributes: "{}".to_string(),
        relation_type: rt.map(|s| s.to_string()),
        episode_uuids: vec![],
        source_descriptions: vec![],
    }
}

fn req(method: &str, params: Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    }
}

/// Dispatches a request and returns the JSON-RPC response as a Value.
async fn dispatch_raw(method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

async fn dispatch(method: &str, params: Value, state: Arc<AppState>) -> Value {
    let v = dispatch_raw(method, params, state).await;
    assert!(
        v.get("error").is_none(),
        "expected result, got error: {}",
        v["error"]
    );
    v["result"].clone()
}

fn count_wal_lines(wal_dir: &std::path::Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(wal_dir) {
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                == "jsonl"
            {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    count += content.lines().filter(|l| !l.trim().is_empty()).count();
                }
            }
        }
    }
    count
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// FR-012, SC-005: WAL replay after a live backfill reproduces the same relation_type
/// values on every edge. Backfill mutations are WAL-durable.
#[tokio::test]
async fn test_backfill_wal_round_trip() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let src = make_entity("Alice");
    let dst = make_entity("Bob");

    let state = make_state_with_wal(db.clone(), wal_dir.path());

    // Insert entities + edges via the DB, then write seed mutations to the WAL.
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();
        // 3 edges with empty relation_type
        for i in 0..3usize {
            conn.insert_relates_to_edge(&make_edge(
                &src.uuid,
                &dst.uuid,
                None,
                &format!("Alice knows Bob (fact {i})"),
            ))
            .unwrap();
        }
        // 2 edges with populated relation_type
        for _ in 0..2usize {
            conn.insert_relates_to_edge(&make_edge(
                &src.uuid,
                &dst.uuid,
                Some("KNOWS"),
                "Alice knows Bob well",
            ))
            .unwrap();
        }
        // Write seed mutations to WAL through the same WalWriter session as canonicalize,
        // so file-sequence ordering is deterministic on replay.
        let seed_mutations = conn.drain_mutations();
        let mut wal_guard = state.wal_writers.lock().unwrap();
        if let Some(writer) = wal_guard.get_mut("liminis") {
            writer
                .with_chunk(|w| {
                    for (cypher, params) in &seed_mutations {
                        w.log_mutation(cypher, params.clone(), "")?;
                    }
                    Ok(())
                })
                .unwrap();
        }
    }

    // Run live backfill — writes relation_type SET mutations to the WAL
    let result = dispatch(
        "knowledge_backfill_relation_types",
        json!({ "group_id": GRP, "dry_run": false }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(
        result["backfilled"], 3,
        "must backfill 3 empty edges: {result}"
    );
    assert_eq!(
        result["total_edges"], 5,
        "must count 5 total edges: {result}"
    );

    // WAL has mutations from the backfill (seed + 3 SET mutations)
    let wal_after_backfill = count_wal_lines(wal_dir.path());
    assert!(
        wal_after_backfill > 0,
        "WAL must have entries after backfill (FR-012)"
    );

    // Snapshot post-backfill edge states
    let post_backfill: Vec<(String, Option<String>)> = {
        let conn = db.connect().unwrap();
        let mut edges = conn.list_relationships(None, 100).unwrap();
        edges.sort_by(|a, b| a.uuid.cmp(&b.uuid));
        edges
            .into_iter()
            .map(|e| (e.uuid, e.relation_type))
            .collect()
    };
    assert_eq!(post_backfill.len(), 5, "all 5 edges must survive backfill");
    for (_, rt) in &post_backfill {
        assert!(
            rt.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
            "every edge must have a non-empty relation_type after backfill: {rt:?}"
        );
    }

    // ── Replay WAL into a fresh DB and compare ────────────────────────────────
    let dir2 = TempDir::new().unwrap();
    let db2 = Arc::new(Db::open(dir2.path().join("replay.db").to_str().unwrap()).unwrap());
    {
        let conn2 = db2.connect().unwrap();
        conn2.init_schema(DIM).unwrap();
        let stats = WalReplayer::new(wal_dir.path()).replay(&conn2).unwrap();
        assert!(
            stats.lines_replayed > 0,
            "WAL replay must replay lines (SC-005)"
        );
    }

    let replayed: Vec<(String, Option<String>)> = {
        let conn2 = db2.connect().unwrap();
        let mut edges = conn2.list_relationships(None, 100).unwrap();
        edges.sort_by(|a, b| a.uuid.cmp(&b.uuid));
        edges
            .into_iter()
            .map(|e| (e.uuid, e.relation_type))
            .collect()
    };

    assert_eq!(
        post_backfill, replayed,
        "WAL replay must reproduce exact post-backfill edge UUIDs and relation_types (SC-005)"
    );
}

/// FR-013: running live backfill twice produces zero new WAL mutations on second run.
#[tokio::test]
async fn test_backfill_idempotency_wal() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let src = make_entity("Alice");
    let dst = make_entity("Bob");

    let state = make_state_with_wal(db.clone(), wal_dir.path());

    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();
        for i in 0..3usize {
            conn.insert_relates_to_edge(&make_edge(
                &src.uuid,
                &dst.uuid,
                None,
                &format!("Alice is connected to Bob ({i})"),
            ))
            .unwrap();
        }
        let seed_mutations = conn.drain_mutations();
        let mut wal_guard = state.wal_writers.lock().unwrap();
        if let Some(writer) = wal_guard.get_mut("liminis") {
            writer
                .with_chunk(|w| {
                    for (cypher, params) in &seed_mutations {
                        w.log_mutation(cypher, params.clone(), "")?;
                    }
                    Ok(())
                })
                .unwrap();
        }
    }

    // First run — fills 3 empty edges
    let result1 = dispatch(
        "knowledge_backfill_relation_types",
        json!({ "group_id": GRP, "dry_run": false }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(result1["backfilled"], 3, "first run must backfill 3");
    let wal_after_first = count_wal_lines(wal_dir.path());
    assert!(wal_after_first > 0, "first run must write WAL mutations");

    // Second run — all edges already have relation_type, so Phase A finds zero candidates
    let state2 = make_state_with_wal(db.clone(), wal_dir.path());
    let result2 = dispatch(
        "knowledge_backfill_relation_types",
        json!({ "group_id": GRP, "dry_run": false }),
        state2,
    )
    .await;
    assert_eq!(
        result2["backfilled"], 0,
        "second run must report backfilled=0 (FR-013): {result2}"
    );

    let wal_after_second = count_wal_lines(wal_dir.path());
    assert_eq!(
        wal_after_first, wal_after_second,
        "second backfill run must emit zero new WAL mutations (FR-013)"
    );
}

// ── Two-group isolation (#447 FR-002/003/004/006, SC-003) ─────────────────────

/// Scoping backfill to group A must not touch group B's edges or WAL stream.
#[tokio::test]
async fn test_backfill_two_group_isolation_scoped_to_group_a() {
    const GROUP_A: &str = "group_a";
    const GROUP_B: &str = "group_b";

    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let a_src = make_entity_in_group("Alice", GROUP_A);
    let a_dst = make_entity_in_group("Bob", GROUP_A);
    let b_src = make_entity_in_group("Carol", GROUP_B);
    let b_dst = make_entity_in_group("Dave", GROUP_B);

    let a_edge_uuid;
    let b_edge_uuid;
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&a_src).unwrap();
        conn.insert_entity(&a_dst).unwrap();
        conn.insert_entity(&b_src).unwrap();
        conn.insert_entity(&b_dst).unwrap();

        let a_edge = make_edge_in_group(&a_src.uuid, &a_dst.uuid, None, "Alice knows Bob", GROUP_A);
        let b_edge =
            make_edge_in_group(&b_src.uuid, &b_dst.uuid, None, "Carol knows Dave", GROUP_B);
        a_edge_uuid = a_edge.uuid.clone();
        b_edge_uuid = b_edge.uuid.clone();
        conn.insert_relates_to_edge(&a_edge).unwrap();
        conn.insert_relates_to_edge(&b_edge).unwrap();
    }

    let state = make_state_with_wal(db.clone(), wal_dir.path());

    let b_wal_dir = lcg_core::wal_group::group_wal_dir(wal_dir.path(), GROUP_B).unwrap();
    let b_wal_lines_before = count_wal_lines(&b_wal_dir);
    let b_before = {
        let conn = db.connect().unwrap();
        conn.get_edges_by_uuids(&[b_edge_uuid.as_str()]).unwrap()
    };

    let result = dispatch(
        "knowledge_backfill_relation_types",
        json!({ "group_id": GROUP_A, "dry_run": false }),
        state,
    )
    .await;
    assert_eq!(result["group_id"], json!(GROUP_A));
    assert_eq!(
        result["total_edges"],
        json!(1),
        "must only see group A's edge"
    );
    assert_eq!(result["backfilled"], json!(1));

    let conn = db.connect().unwrap();

    // Group A's edge was backfilled.
    let a_edges = conn.get_edges_by_uuids(&[a_edge_uuid.as_str()]).unwrap();
    assert_eq!(a_edges.len(), 1);
    assert!(
        a_edges[0]
            .relation_type
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "group A's edge should be backfilled"
    );

    // Group B's edge is byte-identical to its pre-call state.
    let b_after = conn.get_edges_by_uuids(&[b_edge_uuid.as_str()]).unwrap();
    assert_eq!(
        serde_json::to_value(&b_before).unwrap(),
        serde_json::to_value(&b_after).unwrap(),
        "group B's edge must be byte-identical after scoping backfill to group A"
    );
    assert_eq!(
        b_after[0].relation_type, None,
        "group B's edge must not be backfilled as a side effect"
    );

    // Group B's own WAL stream is untouched.
    let b_wal_lines_after = count_wal_lines(&b_wal_dir);
    assert_eq!(
        b_wal_lines_before, b_wal_lines_after,
        "group B's WAL stream must be untouched by a call scoped to group A"
    );
}

// ── Omitted/null/empty group_id is rejected (#447 FR-005, SC-005) ─────────────

/// An omitted, null, or empty `group_id` must be rejected before any candidate selection or
/// WAL write — never fall back to a database-wide rewrite or the default group.
#[tokio::test]
async fn test_backfill_missing_group_id_rejected_no_mutation() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let src = make_entity("Alice");
    let dst = make_entity("Bob");
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();
        conn.insert_relates_to_edge(&make_edge(&src.uuid, &dst.uuid, None, "Alice knows Bob"))
            .unwrap();
    }

    let wal_lines_before = count_wal_lines(wal_dir.path());
    let edges_before = {
        let conn = db.connect().unwrap();
        conn.list_relationships(None, 100).unwrap()
    };

    for params in [
        json!({ "dry_run": false }),
        json!({ "group_id": Value::Null, "dry_run": false }),
        json!({ "group_id": "", "dry_run": false }),
    ] {
        let state = make_state_with_wal(db.clone(), wal_dir.path());
        let v = dispatch_raw("knowledge_backfill_relation_types", params.clone(), state).await;
        assert!(
            v.get("error").is_some(),
            "expected error for group_id={:?}, got: {v}",
            params.get("group_id")
        );
        let msg = v["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains("group_id"),
            "error message should mention group_id, got: {msg}"
        );
    }

    let wal_lines_after = count_wal_lines(wal_dir.path());
    let edges_after = {
        let conn = db.connect().unwrap();
        conn.list_relationships(None, 100).unwrap()
    };
    assert_eq!(
        wal_lines_before, wal_lines_after,
        "a rejected call must not write any WAL entries"
    );
    assert_eq!(
        serde_json::to_value(&edges_before).unwrap(),
        serde_json::to_value(&edges_after).unwrap(),
        "a rejected call must not mutate any edges"
    );
}

// ── Zero-candidate / nonexistent group_id is a no-op, not an error ────────────

/// A `group_id` naming a group with zero candidates (including one that doesn't exist at all)
/// succeeds as a no-op: zero rows rewritten, zero WAL entries written.
#[tokio::test]
async fn test_backfill_unknown_group_id_is_noop_not_error() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let src = make_entity("Alice");
    let dst = make_entity("Bob");
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();
        conn.insert_relates_to_edge(&make_edge(&src.uuid, &dst.uuid, None, "Alice knows Bob"))
            .unwrap();
    }

    let state = make_state_with_wal(db.clone(), wal_dir.path());
    let wal_lines_before = count_wal_lines(wal_dir.path());

    let result = dispatch(
        "knowledge_backfill_relation_types",
        json!({ "group_id": "no_such_group", "dry_run": false }),
        state,
    )
    .await;

    assert_eq!(result["total_edges"], json!(0));
    assert_eq!(result["backfilled"], json!(0));

    let wal_lines_after = count_wal_lines(wal_dir.path());
    assert_eq!(
        wal_lines_before, wal_lines_after,
        "a nonexistent group_id must produce zero WAL entries"
    );

    // The real group's edge is untouched.
    let conn = db.connect().unwrap();
    let edges = conn.list_relationships(None, 100).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].relation_type, None);
}
