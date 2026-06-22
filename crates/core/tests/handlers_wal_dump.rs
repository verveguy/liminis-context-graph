// Round-trip integration test for knowledge_dump_wal (issue #161).
//
// Verifies SC-001 (dump→fresh-DB→replay produces matching counts), SC-002 (no WARN/SKIP),
// SC-004 (empty graph returns zero counts), and SC-006 (no vecf32 in output).

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
    replay::WalReplayer,
    schema,
    telemetry::{NoopSink, TelemetrySink},
    WalWriter,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const DIM: usize = 4;

// ── helpers ───────────────────────────────────────────────────────────────────

fn open_db(path: &std::path::Path) -> Arc<Db> {
    let db = Arc::new(Db::open(path.to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
        schema::migrate(&conn);
    }
    db
}

fn make_state(db: Arc<Db>, db_path: &str) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_path.to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
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

async fn dispatch(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

/// Writes test WAL files into `wal_dir` with one Entity, one Episodic, and one MENTIONS edge.
fn write_test_wal(wal_dir: &std::path::Path) {
    let mut writer = WalWriter::new(wal_dir, 10_000, 0).unwrap();
    writer
        .with_chunk(|w| {
            // Entity
            w.log_mutation(
                "MERGE (n:Entity {uuid: $uuid}) SET \
                 n.name = $name, n.group_id = $gid, n.labels = $labels, \
                 n.created_at = timestamp($created_at), n.name_embedding = $emb, \
                 n.summary = $summary, n.attributes = $attrs",
                json!({
                    "uuid": "rt-entity-1",
                    "name": "Alice",
                    "gid": "rt-group",
                    "labels": ["Entity"],
                    "created_at": "2026-01-01 00:00:00",
                    "emb": [1.0_f64, 0.0_f64, 0.0_f64, 0.0_f64],
                    "summary": "Alice summary",
                    "attrs": "{}",
                }),
                "",
            )?;
            // Episodic
            w.log_mutation(
                "MERGE (n:Episodic {uuid: $uuid}) SET \
                 n.name = $name, n.group_id = $gid, \
                 n.created_at = timestamp($created_at), n.source = $source, \
                 n.source_description = $src_desc, n.content = $content, \
                 n.content_embedding = $emb, \
                 n.valid_at = timestamp($valid_at), n.entity_edges = $edges",
                json!({
                    "uuid": "rt-ep-1",
                    "name": "Test episode",
                    "gid": "rt-group",
                    "created_at": "2026-01-01 00:00:00",
                    "source": "text",
                    "src_desc": "test source",
                    "content": "Alice is a person.",
                    "emb": [0.0_f64, 1.0_f64, 0.0_f64, 0.0_f64],
                    "valid_at": "2026-01-01 00:00:00",
                    "edges": [],
                }),
                "",
            )?;
            // MENTIONS edge
            w.log_mutation(
                "MATCH (ep:Episodic {uuid: $ep_uuid}), (en:Entity {uuid: $en_uuid}) \
                 MERGE (ep)-[r:MENTIONS]->(en) \
                 SET r.uuid = $uuid, r.group_id = $gid, \
                 r.created_at = timestamp($created_at)",
                json!({
                    "ep_uuid": "rt-ep-1",
                    "en_uuid": "rt-entity-1",
                    "uuid": "rt-mentions-1",
                    "gid": "rt-group",
                    "created_at": "2026-01-01 00:00:00",
                }),
                "",
            )?;
            Ok(())
        })
        .unwrap();
}

// ── SC-004: empty graph ────────────────────────────────────────────────────────

/// knowledge_dump_wal on a DB with zero nodes/edges returns success with zero counts.
#[tokio::test]
async fn test_dump_wal_empty_graph() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("dump_empty.db");
    let db = open_db(&db_path);
    let state = make_state(db, db_path.to_str().unwrap());

    let target_dir = dir.path().join("dump-out-empty");
    let v = dispatch(
        1,
        "knowledge_dump_wal",
        json!({ "target_dir": target_dir.to_str().unwrap() }),
        state,
    )
    .await;

    assert_eq!(v["jsonrpc"], "2.0");
    assert!(v.get("result").is_some(), "expected result: {v}");
    let r = &v["result"];
    assert_eq!(r["success"], true, "{v}");
    assert_eq!(r["nodes_dumped"], 0, "{v}");
    assert_eq!(r["edges_dumped"], 0, "{v}");
    assert_eq!(r["files_written"], 0, "{v}");
    assert!(
        r["target_dir"].is_string(),
        "target_dir must be string: {v}"
    );
}

// ── SC-001, SC-002: round-trip dump → fresh-DB → replay ──────────────────────

/// Inserts known Entity + Episodic + MENTIONS via WAL replay, dumps to a fresh WAL,
/// replays the dump into a second DB, and asserts counts match.
#[tokio::test]
async fn test_dump_wal_round_trip() {
    let dir = TempDir::new().unwrap();

    // ── Phase A: populate db1 via WAL replay ──────────────────────────────────
    let db1_path = dir.path().join("db1.db");
    let seed_wal_dir = dir.path().join("seed-wal");
    write_test_wal(&seed_wal_dir);

    let db1 = open_db(&db1_path);
    {
        let conn = db1.connect().unwrap();
        WalReplayer::new(&seed_wal_dir).replay(&conn).unwrap();
    }

    let entities_before = db1.connect().unwrap().count_nodes("Entity").unwrap();
    let episodics_before = db1.connect().unwrap().count_nodes("Episodic").unwrap();
    let mentions_before = db1.connect().unwrap().count_mentions_edges().unwrap();
    assert_eq!(entities_before, 1, "should have 1 entity after seed replay");
    assert_eq!(
        episodics_before, 1,
        "should have 1 episodic after seed replay"
    );
    assert_eq!(
        mentions_before, 1,
        "should have 1 MENTIONS edge after seed replay"
    );

    // ── Phase B: dump db1 to a fresh WAL directory ────────────────────────────
    let dump_dir = dir.path().join("dump-out");
    let state1 = make_state(Arc::clone(&db1), db1_path.to_str().unwrap());
    let dump_v = dispatch(
        2,
        "knowledge_dump_wal",
        json!({ "target_dir": dump_dir.to_str().unwrap() }),
        state1,
    )
    .await;

    assert_eq!(
        dump_v["result"]["success"], true,
        "dump must succeed: {dump_v}"
    );
    let nodes_dumped = dump_v["result"]["nodes_dumped"].as_u64().unwrap_or(0);
    let edges_dumped = dump_v["result"]["edges_dumped"].as_u64().unwrap_or(0);
    let files_written = dump_v["result"]["files_written"].as_u64().unwrap_or(0);
    assert!(nodes_dumped >= 2, "must dump at least 2 nodes: {dump_v}");
    assert!(edges_dumped >= 1, "must dump at least 1 edge: {dump_v}");
    assert!(files_written >= 1, "must write at least 1 file: {dump_v}");

    // ── Phase C: replay dump into a fresh db2 ────────────────────────────────
    let db2_path = dir.path().join("db2.db");
    let db2 = open_db(&db2_path);
    {
        let conn = db2.connect().unwrap();
        let stats = WalReplayer::new(&dump_dir)
            .replay(&conn)
            .expect("dump replay must succeed");
        assert_eq!(stats.failed_lines, 0, "zero replay failures");
        assert!(
            stats.lines_replayed > 0,
            "should have replayed some mutations"
        );
    }

    // ── Phase D: verify counts match ──────────────────────────────────────────
    let entities_after = db2.connect().unwrap().count_nodes("Entity").unwrap();
    let episodics_after = db2.connect().unwrap().count_nodes("Episodic").unwrap();
    let mentions_after = db2.connect().unwrap().count_mentions_edges().unwrap();

    assert_eq!(
        entities_after, entities_before,
        "entity count must match after round-trip"
    );
    assert_eq!(
        episodics_after, episodics_before,
        "episodic count must match after round-trip"
    );
    assert_eq!(
        mentions_after, mentions_before,
        "mentions edge count must match after round-trip"
    );
}

// ── SC-006: no vecf32 in output ────────────────────────────────────────────────

/// Verifies that dump output files contain no legacy vecf32(...) syntax.
#[tokio::test]
async fn test_dump_wal_no_vecf32_in_output() {
    let dir = TempDir::new().unwrap();

    // Seed db with one entity that has a non-trivial embedding.
    let db_path = dir.path().join("db_vf.db");
    let seed_wal_dir = dir.path().join("seed-wal-vf");
    {
        let mut writer = WalWriter::new(&seed_wal_dir, 10_000, 0).unwrap();
        writer
            .with_chunk(|w| {
                w.log_mutation(
                    "MERGE (n:Entity {uuid: $uuid}) SET \
                     n.name = $name, n.group_id = $gid, n.labels = $labels, \
                     n.created_at = timestamp($created_at), n.name_embedding = $emb, \
                     n.summary = $summary, n.attributes = $attrs",
                    json!({
                        "uuid": "vf-entity-1",
                        "name": "VecTest",
                        "gid": "vf-group",
                        "labels": ["Entity"],
                        "created_at": "2026-01-01 00:00:00",
                        "emb": [0.1_f64, 0.2_f64, 0.3_f64, 0.4_f64],
                        "summary": "embedding test",
                        "attrs": "{}",
                    }),
                    "",
                )
            })
            .unwrap();
    }

    let db = open_db(&db_path);
    {
        let conn = db.connect().unwrap();
        WalReplayer::new(&seed_wal_dir).replay(&conn).unwrap();
    }

    let dump_dir = dir.path().join("dump-vf");
    let state = make_state(Arc::clone(&db), db_path.to_str().unwrap());
    let v = dispatch(
        3,
        "knowledge_dump_wal",
        json!({ "target_dir": dump_dir.to_str().unwrap() }),
        state,
    )
    .await;
    assert_eq!(v["result"]["success"], true, "{v}");

    // Grep all .jsonl files in the dump for vecf32.
    if dump_dir.exists() {
        for entry in std::fs::read_dir(&dump_dir).unwrap().flatten() {
            if entry.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
                let content = std::fs::read_to_string(entry.path()).unwrap();
                assert!(
                    !content.contains("vecf32"),
                    "dump file {:?} must not contain vecf32",
                    entry.path()
                );
            }
        }
    }
}

// ── FR-004: duplicate target_dir guard ────────────────────────────────────────

/// A second call with the same non-empty target_dir returns an error (FR-004).
#[tokio::test]
async fn test_dump_wal_refuses_existing_nonempty_dir() {
    let dir = TempDir::new().unwrap();

    // Seed one entity so the first dump produces at least one .jsonl file.
    let db_path = dir.path().join("db_dup.db");
    let seed_wal_dir = dir.path().join("seed-wal-dup");
    {
        let mut writer = WalWriter::new(&seed_wal_dir, 10_000, 0).unwrap();
        writer
            .with_chunk(|w| {
                w.log_mutation(
                    "MERGE (n:Entity {uuid: $uuid}) SET \
                     n.name = $name, n.group_id = $gid, n.labels = $labels, \
                     n.created_at = timestamp($created_at), n.name_embedding = $emb, \
                     n.summary = $summary, n.attributes = $attrs",
                    json!({
                        "uuid": "dup-entity-1",
                        "name": "DupTest",
                        "gid": "dup-group",
                        "labels": ["Entity"],
                        "created_at": "2026-01-01 00:00:00",
                        "emb": [0.5_f64, 0.5_f64, 0.5_f64, 0.5_f64],
                        "summary": "",
                        "attrs": "{}",
                    }),
                    "",
                )
            })
            .unwrap();
    }

    let db = open_db(&db_path);
    {
        let conn = db.connect().unwrap();
        WalReplayer::new(&seed_wal_dir).replay(&conn).unwrap();
    }

    let dump_dir = dir.path().join("dump-dup");
    let state1 = make_state(Arc::clone(&db), db_path.to_str().unwrap());

    // First call must succeed.
    let v1 = dispatch(
        4,
        "knowledge_dump_wal",
        json!({ "target_dir": dump_dir.to_str().unwrap() }),
        state1,
    )
    .await;
    assert_eq!(v1["result"]["success"], true, "first dump: {v1}");

    // Second call to the same non-empty dir must return an error.
    let state2 = make_state(db, db_path.to_str().unwrap());
    let v2 = dispatch(
        5,
        "knowledge_dump_wal",
        json!({ "target_dir": dump_dir.to_str().unwrap() }),
        state2,
    )
    .await;
    assert!(
        v2.get("error").is_some(),
        "second dump to same dir must return error: {v2}"
    );
}
