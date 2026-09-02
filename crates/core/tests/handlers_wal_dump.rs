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
    EntityRow, EpisodicRow, WalWriter,
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
        schema::migrate(&conn, DIM);
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
        wal_root: None,
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
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
        embedding_cache: std::sync::Arc::new(lcg_core::EmbeddingCache::new()),
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
///
/// Uses the real `$name_embedding`/`$content_embedding` param names (matching the column names,
/// the convention every real writer uses), not a synthetic mismatched name — a mismatched name
/// would bypass both `WalWriter::log_mutation`'s FR-001 strip list and replay's FR-002 template-
/// based recompute detection, silently defeating the very contract this issue's round-trip tests
/// exist to exercise (a review finding on this issue's own PR).
fn write_test_wal(wal_dir: &std::path::Path) {
    let mut writer = WalWriter::new(wal_dir, 10_000, 0).unwrap();
    writer
        .with_chunk(|w| {
            // Entity
            w.log_mutation(
                "MERGE (n:Entity {uuid: $uuid}) SET \
                 n.name = $name, n.group_id = $gid, n.labels = $labels, \
                 n.created_at = timestamp($created_at), n.name_embedding = $name_embedding, \
                 n.summary = $summary, n.attributes = $attrs",
                json!({
                    "uuid": "rt-entity-1",
                    "name": "Alice",
                    "gid": "rt-group",
                    "labels": ["Entity"],
                    "created_at": "2026-01-01 00:00:00",
                    "name_embedding": [1.0_f64, 0.0_f64, 0.0_f64, 0.0_f64],
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
                 n.content_embedding = $content_embedding, \
                 n.valid_at = timestamp($valid_at), n.entity_edges = $edges",
                json!({
                    "uuid": "rt-ep-1",
                    "name": "Test episode",
                    "gid": "rt-group",
                    "created_at": "2026-01-01 00:00:00",
                    "source": "text",
                    "src_desc": "test source",
                    "content": "Alice is a person.",
                    "content_embedding": [0.0_f64, 1.0_f64, 0.0_f64, 0.0_f64],
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
        WalReplayer::new(&seed_wal_dir)
            .replay(&conn, lcg_core::zero_vector_embed_fn(4), 4)
            .unwrap();
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
            .replay(&conn, lcg_core::zero_vector_embed_fn(4), 4)
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
                     n.created_at = timestamp($created_at), n.name_embedding = $name_embedding, \
                     n.summary = $summary, n.attributes = $attrs",
                    json!({
                        "uuid": "vf-entity-1",
                        "name": "VecTest",
                        "gid": "vf-group",
                        "labels": ["Entity"],
                        "created_at": "2026-01-01 00:00:00",
                        "name_embedding": [0.1_f64, 0.2_f64, 0.3_f64, 0.4_f64],
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
        WalReplayer::new(&seed_wal_dir)
            .replay(&conn, lcg_core::zero_vector_embed_fn(4), 4)
            .unwrap();
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
                     n.created_at = timestamp($created_at), n.name_embedding = $name_embedding, \
                     n.summary = $summary, n.attributes = $attrs",
                    json!({
                        "uuid": "dup-entity-1",
                        "name": "DupTest",
                        "gid": "dup-group",
                        "labels": ["Entity"],
                        "created_at": "2026-01-01 00:00:00",
                        "name_embedding": [0.5_f64, 0.5_f64, 0.5_f64, 0.5_f64],
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
        WalReplayer::new(&seed_wal_dir)
            .replay(&conn, lcg_core::zero_vector_embed_fn(4), 4)
            .unwrap();
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

// ── FR-010, SC-003, SC-007: TIMESTAMP microsecond fidelity through dump→replay ─

/// Verifies that dump WAL output preserves sub-second (microsecond) TIMESTAMP precision and that
/// dump→wipe→replay produces a queryable entity with the original timestamp. SC-003.
///
/// Also verifies SC-007: no `vecf32(` appears in dump output.
#[tokio::test]
async fn test_dump_wal_timestamp_fidelity() {
    const ENTITY_UUID: &str = "ts-fidelity-entity-1";
    const ENTITY_NAME: &str = "TimestampFidelityEntity";
    const MICROSECOND_TS: &str = "2024-06-01T12:00:00.123456Z";

    let dir = TempDir::new().unwrap();

    // ── Phase A: seed db1 with an entity having a microsecond RFC-3339 timestamp ──
    let db1_path = dir.path().join("db1_tsf.db");
    let seed_wal_dir = dir.path().join("seed-wal-tsf");
    {
        let mut writer = WalWriter::new(&seed_wal_dir, 10_000, 0).unwrap();
        writer
            .with_chunk(|w| {
                w.log_mutation(
                    "MERGE (n:Entity {uuid: $uuid}) SET \
                     n.name = $name, n.group_id = $group_id, n.labels = $labels, \
                     n.created_at = timestamp($created_at), n.name_embedding = $name_embedding, \
                     n.summary = $summary, n.attributes = $attrs",
                    json!({
                        "uuid": ENTITY_UUID,
                        "name": ENTITY_NAME,
                        "group_id": "tsf-group",
                        "labels": ["Entity"],
                        "created_at": MICROSECOND_TS,
                        "name_embedding": [0.1_f64, 0.2_f64, 0.3_f64, 0.4_f64],
                        "summary": "timestamp fidelity test entity",
                        "attrs": "{}",
                    }),
                    "",
                )
            })
            .unwrap();
    }
    let db1 = open_db(&db1_path);
    {
        let conn = db1.connect().unwrap();
        WalReplayer::new(&seed_wal_dir)
            .replay(&conn, lcg_core::zero_vector_embed_fn(4), 4)
            .unwrap();
    }
    assert_eq!(
        db1.connect().unwrap().count_nodes("Entity").unwrap(),
        1,
        "seed entity must be present"
    );

    // ── Phase B: dump db1 → dump_dir ──────────────────────────────────────────
    let dump_dir = dir.path().join("dump-tsf");
    let state1 = make_state(Arc::clone(&db1), db1_path.to_str().unwrap());
    let dump_v = dispatch(
        10,
        "knowledge_dump_wal",
        json!({ "target_dir": dump_dir.to_str().unwrap() }),
        state1,
    )
    .await;
    assert_eq!(
        dump_v["result"]["success"], true,
        "dump must succeed: {dump_v}"
    );

    // ── Phase C: inspect dump WAL files ───────────────────────────────────────
    assert!(dump_dir.exists(), "dump directory must exist");
    let mut found_microseconds = false;
    let mut found_vecf32 = false;
    for entry in std::fs::read_dir(&dump_dir).unwrap().flatten() {
        if entry.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            if content.contains("vecf32(") {
                found_vecf32 = true;
            }
            // Parse each WAL line and check params for entity timestamp
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line) {
                    let params = &parsed["params"];
                    // Find the entity WAL line by uuid
                    if params.get("uuid").and_then(|v| v.as_str()) == Some(ENTITY_UUID) {
                        let ts = params["created_at"].as_str().unwrap_or("");
                        // Must contain microseconds (`.123456`)
                        if ts.contains(".123456") {
                            found_microseconds = true;
                        }
                        assert!(
                            !ts.is_empty(),
                            "created_at must be non-empty in dump WAL params"
                        );
                    }
                }
            }
        }
    }
    assert!(!found_vecf32, "dump WAL must not contain vecf32 (SC-007)");
    assert!(
        found_microseconds,
        "dump WAL must preserve microsecond precision in created_at — \
         expected '{MICROSECOND_TS}' in a WAL params.created_at field (SC-003)"
    );

    // ── Phase D: replay dump into fresh db2 ───────────────────────────────────
    let db2_path = dir.path().join("db2_tsf.db");
    let db2 = open_db(&db2_path);
    let replay_stats = {
        let conn = db2.connect().unwrap();
        WalReplayer::new(&dump_dir)
            .replay(&conn, lcg_core::zero_vector_embed_fn(4), 4)
            .expect("dump WAL replay must succeed")
    };
    assert_eq!(
        replay_stats.failed_lines, 0,
        "dump→replay must produce zero failed lines (SC-003)"
    );
    assert!(
        replay_stats.lines_replayed > 0,
        "must replay at least one line"
    );

    // ── Phase E: entity must exist and have a valid created_at after replay ──
    let entity = db2
        .connect()
        .unwrap()
        .get_entity_by_uuid(ENTITY_UUID)
        .expect("get_entity_by_uuid must not fail after dump→replay");
    let entity = entity.unwrap_or_else(|| panic!("entity {ENTITY_UUID} must exist after replay"));
    let created_at = &entity.created_at;
    // `get_entity_by_uuid` returns the space-format read-back ("YYYY-MM-DD HH:MM:SS"). Assert the
    // expected date portion is present — this confirms the correct timestamp was replayed, not
    // truncated or corrupted (a TYPE_MISMATCH would have caused the replay to fail at Phase D).
    assert!(
        created_at.contains("2024-06-01"),
        "replayed entity created_at must contain the expected date '2024-06-01' (SC-003): {created_at}"
    );

    // Query the raw TIMESTAMP value via Cypher to verify lbug stored a real TIMESTAMP type.
    // The raw representation includes sub-second precision; check it starts with the expected date.
    let raw_rows = db2
        .connect()
        .unwrap()
        .cypher_query(&format!(
            "MATCH (n:Entity {{uuid: '{ENTITY_UUID}'}}) RETURN n.created_at"
        ))
        .expect("Cypher query for created_at must succeed after replay (SC-003)");
    assert_eq!(raw_rows.len(), 1, "must return exactly one row");
    let raw_ts = &raw_rows[0][0];
    assert!(
        raw_ts.contains("2024-06-01"),
        "raw Cypher-returned created_at must contain the expected date after replay (SC-003): {raw_ts}"
    );
    // Verify microsecond component is preserved in the raw TIMESTAMP.
    // lbug/Kuzu includes sub-second digits in its string representation when non-zero.
    assert!(
        raw_ts.contains(".123456") || raw_ts.contains("123456"),
        "raw TIMESTAMP must preserve microsecond component .123456 after dump→replay (SC-003): {raw_ts}"
    );
}

// ── #470: Entity.summary_embedding survives dump→replay ──────────────────────

/// A `knowledge_dump_wal` → replay round trip must leave `Entity.summary_embedding` recomputed
/// from the dumped `summary` text, not NULL and not left bound to whatever value db1 happened to
/// store. Regression test for a gap where `dump_entities_page` and `ENTITY_CYPHER` were updated
/// for every other Entity column except this one, added alongside #470's own column — the dumped
/// WAL line would omit `summary_embedding`'s co-located `summary` text entirely, and replaying it
/// would leave the column NULL on the target DB, breaking the "always a same-length FLOAT[dim]
/// vector, never absent" invariant the schema migration establishes.
///
/// Updated for issue #526: replay never binds a *stored* vector any more (FR-002), so the
/// meaningful assertion is no longer "the exact same bytes survive the round trip" — it's "the
/// dumped record still carries `summary` co-located with `summary_embedding`'s placeholder, so
/// replay's mandatory recompute produces a real, non-zero vector derived from that text" rather
/// than silently leaving the column NULL or zero-filled.
#[tokio::test]
async fn test_dump_wal_preserves_entity_summary_embedding() {
    let dir = TempDir::new().unwrap();

    let db1_path = dir.path().join("db1-se.db");
    let db1 = open_db(&db1_path);
    const UUID: &str = "se-entity-1";
    {
        let conn = db1.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: UUID.to_string(),
            name: "widget-1".to_string(),
            group_id: "se-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "a pump manufacturer".to_string(),
            attributes: "{}".to_string(),
            summary_embedding: vec![0.1, 0.2, 0.3, 0.4],
            ..Default::default()
        })
        .unwrap();
    }

    let before_rows = db1
        .connect()
        .unwrap()
        .cypher_query(&format!(
            "MATCH (n:Entity {{uuid: '{UUID}'}}) RETURN n.summary_embedding"
        ))
        .unwrap();
    assert_eq!(before_rows.len(), 1);
    let before = before_rows[0][0].clone();
    assert!(
        !before.is_empty(),
        "precondition: seeded entity must have a non-empty summary_embedding: {before:?}"
    );

    let dump_dir = dir.path().join("dump-se");
    let state1 = make_state(Arc::clone(&db1), db1_path.to_str().unwrap());
    let dump_v = dispatch(
        10,
        "knowledge_dump_wal",
        json!({ "target_dir": dump_dir.to_str().unwrap() }),
        state1,
    )
    .await;
    assert_eq!(
        dump_v["result"]["success"], true,
        "dump must succeed: {dump_v}"
    );

    // A deterministic embed fn keyed on the dumped `summary` text (issue #526): if dump.rs's
    // ENTITY_CYPHER ever regressed to omitting the co-located `summary` param, replay would find
    // no text for `summary_embedding` and zero-fill it (a CREATE-type row, per FR-005) instead of
    // producing this mapped vector — making that regression visible here again.
    let recomputed_summary_vec = vec![0.5_f32, 0.6, 0.7, 0.8];
    let mut text_to_vec = std::collections::HashMap::new();
    text_to_vec.insert(
        "a pump manufacturer".to_string(),
        recomputed_summary_vec.clone(),
    );
    let embed_fn: lcg_core::RecomputeEmbedFn = Box::new(move |texts: &[&str]| {
        Ok(texts
            .iter()
            .map(|text| {
                text_to_vec
                    .get(*text)
                    .cloned()
                    .unwrap_or_else(|| vec![0.0; 4])
            })
            .collect())
    });

    let db2_path = dir.path().join("db2-se.db");
    let db2 = open_db(&db2_path);
    {
        let conn = db2.connect().unwrap();
        let stats = WalReplayer::new(&dump_dir)
            .replay(&conn, embed_fn, 4)
            .expect("dump replay must succeed");
        assert_eq!(stats.failed_lines, 0, "zero replay failures");
    }

    let after_rows = db2
        .connect()
        .unwrap()
        .cypher_query(&format!(
            "MATCH (n:Entity {{uuid: '{UUID}'}}) RETURN n.summary_embedding"
        ))
        .unwrap();
    assert_eq!(after_rows.len(), 1);
    let after = &after_rows[0][0];
    let expected = format!(
        "[{}]",
        recomputed_summary_vec
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    assert_eq!(
        after, &expected,
        "summary_embedding must be recomputed from the dumped summary text (FR-002), not left \
         NULL or zero-filled — before dump: {before:?}, after replay: {after:?}"
    );
}

/// FR-008 (issue #528): an episode's `attributes` must survive being dumped and replayed during
/// WAL compaction, the same as every other `Episodic` column — mirrors
/// `test_dump_wal_preserves_entity_summary_embedding`'s shape, but `attributes` is opaque
/// caller-supplied data (no recompute step), so the dumped and replayed value must match
/// verbatim.
#[tokio::test]
async fn test_dump_wal_preserves_episodic_attributes() {
    let dir = TempDir::new().unwrap();

    let db1_path = dir.path().join("db1-attrs.db");
    let db1 = open_db(&db1_path);
    const UUID: &str = "attrs-episode-1";
    let attrs = r#"{"originating_system":"orac","ingestion_batch":"batch-7"}"#;
    {
        let conn = db1.connect().unwrap();
        conn.insert_episodic(&EpisodicRow {
            uuid: UUID.to_string(),
            name: "Test episode".to_string(),
            group_id: "attrs-group".to_string(),
            created_at: "2026-01-01 00:00:00".to_string(),
            source: "text".to_string(),
            source_description: "test source".to_string(),
            content: "Alice is a person.".to_string(),
            content_embedding: vec![0.0, 1.0, 0.0, 0.0],
            valid_at: "2026-01-01 00:00:00".to_string(),
            entity_edges: vec![],
            attributes: attrs.to_string(),
        })
        .unwrap();
    }

    let before_rows = db1
        .connect()
        .unwrap()
        .cypher_query(&format!(
            "MATCH (n:Episodic {{uuid: '{UUID}'}}) RETURN n.attributes"
        ))
        .unwrap();
    assert_eq!(before_rows.len(), 1);
    assert_eq!(
        before_rows[0][0], attrs,
        "precondition: seeded episode must carry the attributes it was created with"
    );

    let dump_dir = dir.path().join("dump-attrs");
    let state1 = make_state(Arc::clone(&db1), db1_path.to_str().unwrap());
    let dump_v = dispatch(
        11,
        "knowledge_dump_wal",
        json!({ "target_dir": dump_dir.to_str().unwrap() }),
        state1,
    )
    .await;
    assert_eq!(
        dump_v["result"]["success"], true,
        "dump must succeed: {dump_v}"
    );

    let db2_path = dir.path().join("db2-attrs.db");
    let db2 = open_db(&db2_path);
    {
        let conn = db2.connect().unwrap();
        let stats = WalReplayer::new(&dump_dir)
            .replay(&conn, lcg_core::zero_vector_embed_fn(4), 4)
            .expect("dump replay must succeed");
        assert_eq!(stats.failed_lines, 0, "zero replay failures");
    }

    let after_rows = db2
        .connect()
        .unwrap()
        .cypher_query(&format!(
            "MATCH (n:Episodic {{uuid: '{UUID}'}}) RETURN n.attributes"
        ))
        .unwrap();
    assert_eq!(after_rows.len(), 1);
    assert_eq!(
        after_rows[0][0], attrs,
        "attributes must survive dump→replay verbatim (FR-008) — before: {:?}, after: {:?}",
        before_rows[0][0], after_rows[0][0]
    );
}
