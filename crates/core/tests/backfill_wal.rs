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
const GRP: &str = "test";

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
        wal_dir: Some(wal_dir.to_path_buf()),
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

fn make_entity(name: &str) -> EntityRow {
    EntityRow {
        uuid: Uuid::new_v4().to_string(),
        name: name.to_string(),
        group_id: GRP.to_string(),
        labels: vec!["Entity".to_string()],
        created_at: TS.to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: format!("summary of {name}"),
        attributes: "{}".to_string(),
        ..Default::default()
    }
}

fn make_edge(src: &str, dst: &str, rt: Option<&str>, fact: &str) -> RelatesToEdge {
    RelatesToEdge {
        uuid: Uuid::new_v4().to_string(),
        name: format!("{src} → {dst}"),
        source_node_uuid: src.to_string(),
        target_node_uuid: dst.to_string(),
        group_id: GRP.to_string(),
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

async fn dispatch(method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(method, params), state, None).await;
    let v = serde_json::to_value(resp).unwrap();
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
        let mut wal_guard = state.wal_writer.lock().unwrap();
        if let Some(ref mut writer) = *wal_guard {
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
        json!({ "dry_run": false }),
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
        let mut wal_guard = state.wal_writer.lock().unwrap();
        if let Some(ref mut writer) = *wal_guard {
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
        json!({ "dry_run": false }),
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
        json!({ "dry_run": false }),
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
