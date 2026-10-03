//! Issue #673 / ADR-0673: `ingested_at`, the service-clock "knowledge time" stored on episodes,
//! entities and edges separately from the caller-supplied event time (`created_at`/`valid_at`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    schema,
    telemetry::{NoopSink, TelemetrySink},
    Db, EntityRow, EpisodicRow, RelatesToEdge, WalReplayer, WalWriter,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const DIM: usize = 4;

fn make_db() -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

fn make_state(db: Arc<Db>, wal_dir: Option<PathBuf>) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let writers: HashMap<String, WalWriter> = wal_dir
        .as_ref()
        .and_then(|d| WalWriter::new(d, 10_000, 5 * 1024 * 1024).ok())
        .map(|w| ("liminis".to_string(), w))
        .into_iter()
        .collect();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(tokio::sync::RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_root: wal_dir,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writers: Arc::new(Mutex::new(writers)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
        embedding_cache: Arc::new(lcg_core::EmbeddingCache::new()),
    })
}

async fn call(method: &str, params: Value, state: &Arc<AppState>) -> Value {
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    };
    let v = serde_json::to_value(handlers::dispatch(req, Arc::clone(state), None).await).unwrap();
    assert!(v.get("error").is_none(), "{method} errored: {v}");
    v["result"].clone()
}

/// Seconds between a read-back timestamp ("YYYY-MM-DD HH:MM:SS[.ffffff]") and now.
fn age_secs(ts: &str) -> i64 {
    let naive = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S%.f")
        .unwrap_or_else(|e| panic!("unparseable timestamp {ts:?}: {e}"));
    (chrono::Utc::now().naive_utc() - naive).num_seconds()
}

async fn ingest(state: &Arc<AppState>, chunk_id: &str, text: &str, reference_time: &str) {
    let r = call(
        "knowledge_process_chunk",
        json!({
            "chunk_text": text, "chunk_id": chunk_id, "source_file": "doc.txt",
            "reference_time": reference_time,
        }),
        state,
    )
    .await;
    assert!(r.is_object(), "{r}");
}

type Snapshot = (
    Vec<(String, String)>,
    Vec<(String, String)>,
    Vec<(String, String)>,
);

/// Everything the graph holds, keyed by uuid → ingested_at, per record type.
fn snapshot(db: &Db) -> Snapshot {
    let conn = db.connect().unwrap();
    let mut ents: Vec<_> = conn
        .get_entities_by_group_ids(None)
        .unwrap()
        .into_iter()
        .map(|e| (e.uuid, e.ingested_at))
        .collect();
    let mut eps: Vec<_> = conn
        .retrieve_episodes_page(None, 1000, None, None)
        .unwrap()
        .into_iter()
        .map(|(e, _)| (e.uuid, e.ingested_at))
        .collect();
    let mut edges: Vec<_> = conn
        .get_edges_by_group_ids(None)
        .unwrap()
        .into_iter()
        .map(|e| (e.uuid, e.ingested_at))
        .collect();
    ents.sort();
    eps.sort();
    edges.sort();
    (ents, eps, edges)
}

/// SC-001 / US1 / US2: a backdated chunk keeps event time in `valid_at`/`created_at` and gets the
/// service clock in `ingested_at`, on episodes, entities and edges.
#[tokio::test]
async fn backdated_chunk_has_event_time_and_ingest_time() {
    let (db, _dir) = make_db();
    let state = make_state(Arc::clone(&db), None);
    ingest(
        &state,
        "c1",
        "Alice works at Acme Corp.",
        "2026-08-04T00:00:00Z",
    )
    .await;

    let eps = call("knowledge_get_episodes", json!({"last_n": 10}), &state).await;
    let ep = &eps["episodes"][0];
    assert!(
        ep["valid_at"].as_str().unwrap().starts_with("2026-08-04"),
        "{ep}"
    );
    assert!(
        ep["created_at"].as_str().unwrap().starts_with("2026-08-04"),
        "{ep}"
    );
    let ing = ep["ingested_at"].as_str().expect("episode ingested_at");
    assert!(
        (0..60).contains(&age_secs(ing)),
        "episode ingested_at not ~now: {ing}"
    );

    let ents = call("knowledge_list_entities", json!({}), &state).await;
    let nodes = ents["nodes"].as_array().unwrap();
    assert!(!nodes.is_empty());
    for n in nodes {
        assert!(
            n["created_at"].as_str().unwrap().starts_with("2026-08-04"),
            "{n}"
        );
        let ing = n["ingested_at"].as_str().expect("entity ingested_at");
        assert!(
            (0..60).contains(&age_secs(ing)),
            "entity ingested_at not ~now: {ing}"
        );
    }

    let edges = call("knowledge_get_edges_by_group", json!({}), &state).await;
    let edges = edges["edges"].as_array().unwrap();
    assert!(!edges.is_empty());
    for e in edges {
        let ing = e["ingested_at"].as_str().expect("edge ingested_at");
        assert!(
            (0..60).contains(&age_secs(ing)),
            "edge ingested_at not ~now: {ing}"
        );
    }
    // The other edge read surfaces expose it too (FR-004).
    for (method, key) in [
        ("knowledge_list_relationships", "edges"),
        ("knowledge_find_relationships", "edges"),
    ] {
        let r = call(method, json!({"query": "Alice", "num_results": 10}), &state).await;
        if let Some(arr) = r[key].as_array() {
            for e in arr {
                assert!(e["ingested_at"].is_string(), "{method}: {e}");
            }
        }
    }
}

/// FR-002: a caller-supplied `ingested_at` never influences the stored value.
#[tokio::test]
async fn caller_supplied_ingested_at_is_ignored() {
    let (db, _dir) = make_db();
    let state = make_state(Arc::clone(&db), None);
    let bogus = "2001-01-01T00:00:00Z";
    let r = call(
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.", "chunk_id": "c1", "source_file": "d.txt",
            "reference_time": "2026-08-04T00:00:00Z", "ingested_at": bogus,
        }),
        &state,
    )
    .await;
    assert!(r.is_object());
    call(
        "knowledge_assert_entity",
        json!({"name": "Zed", "group_id": "liminis", "ingested_at": bogus}),
        &state,
    )
    .await;
    let (ents, eps, _) = snapshot(&db);
    for (_, ing) in ents.iter().chain(eps.iter()) {
        assert!(!ing.starts_with("2001"), "caller value leaked: {ing}");
        assert!((0..60).contains(&age_secs(ing)), "{ing}");
    }
}

/// SC-005 / US2: direct assertions report the service clock, independent of `valid_at`.
#[tokio::test]
async fn direct_assertions_stamp_service_clock() {
    let (db, _dir) = make_db();
    let state = make_state(Arc::clone(&db), None);
    for name in ["Alice", "Bob"] {
        call(
            "knowledge_assert_entity",
            json!({"name": name, "group_id": "liminis"}),
            &state,
        )
        .await;
    }
    call(
        "knowledge_assert_relationship",
        json!({
            "source_name": "Alice", "target_name": "Bob", "predicate": "KNOWS",
            "group_id": "liminis", "valid_at": "2020-01-01T00:00:00Z",
        }),
        &state,
    )
    .await;
    let (ents, _, edges) = snapshot(&db);
    assert_eq!(ents.len(), 2);
    assert_eq!(edges.len(), 1);
    for (_, ing) in ents.iter().chain(edges.iter()) {
        assert!((0..60).contains(&age_secs(ing)), "not ~now: {ing}");
    }
}

/// US5 / FR-005: `fields` accepts `ingested_at` and honours it both ways.
#[tokio::test]
async fn fields_projection_honours_ingested_at() {
    let (db, _dir) = make_db();
    let state = make_state(Arc::clone(&db), None);
    ingest(
        &state,
        "c1",
        "Alice works at Acme Corp.",
        "2026-08-04T00:00:00Z",
    )
    .await;

    let with = call(
        "knowledge_get_episodes",
        json!({"last_n": 5, "fields": ["uuid", "ingested_at"]}),
        &state,
    )
    .await;
    assert!(with["episodes"][0]["ingested_at"].is_string(), "{with}");
    let without = call(
        "knowledge_get_episodes",
        json!({"last_n": 5, "fields": ["uuid"]}),
        &state,
    )
    .await;
    assert!(
        without["episodes"][0].get("ingested_at").is_none(),
        "{without}"
    );

    let with = call(
        "knowledge_list_entities",
        json!({"fields": ["uuid", "ingested_at"]}),
        &state,
    )
    .await;
    assert!(with["nodes"][0]["ingested_at"].is_string(), "{with}");
    let without = call(
        "knowledge_list_entities",
        json!({"fields": ["uuid"]}),
        &state,
    )
    .await;
    assert!(
        without["nodes"][0].get("ingested_at").is_none(),
        "{without}"
    );
}

/// FR-008: a re-observed entity keeps the ingest time of its first write.
#[tokio::test]
async fn reobserved_entity_keeps_first_ingest_time() {
    let (db, _dir) = make_db();
    let state = make_state(Arc::clone(&db), None);
    ingest(
        &state,
        "c1",
        "Alice works at Acme Corp.",
        "2026-08-04T00:00:00Z",
    )
    .await;
    let (before, _, _) = snapshot(&db);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    ingest(
        &state,
        "c2",
        "Alice works at Acme Corp.",
        "2026-08-05T00:00:00Z",
    )
    .await;
    let (after, _, _) = snapshot(&db);
    for (uuid, ing) in &before {
        let again = after
            .iter()
            .find(|(u, _)| u == uuid)
            .expect("entity survives");
        assert_eq!(&again.1, ing, "re-observation restamped {uuid}");
    }
}

/// FR-008: `knowledge_merge_entities` leaves the canonical entity's ingest time alone, even
/// though it re-dates its `created_at` to the earliest alias.
#[tokio::test]
async fn merge_does_not_restamp_canonical() {
    let (db, _dir) = make_db();
    let state = make_state(Arc::clone(&db), None);
    {
        let conn = db.connect().unwrap();
        for (uuid, created) in [
            ("e-new", "2026-02-01 00:00:00"),
            ("e-old", "2026-01-01 00:00:00"),
        ] {
            conn.insert_entity(&EntityRow {
                uuid: uuid.into(),
                name: "Brett".into(),
                group_id: "liminis".into(),
                labels: vec!["Entity".into()],
                created_at: created.into(),
                name_embedding: vec![1.0, 0.0, 0.0, 0.0],
                summary: "s".into(),
                attributes: "{}".into(),
                ..Default::default()
            })
            .unwrap();
        }
    }
    let (before, _, _) = snapshot(&db);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let r = call(
        "knowledge_merge_entities",
        json!({"canonical_name": "Brett", "merge_all_by_name": true, "group_id": "liminis"}),
        &state,
    )
    .await;
    assert_eq!(r["success"], true, "{r}");
    let (after, _, _) = snapshot(&db);
    for (uuid, ing) in &before {
        let again = after.iter().find(|(u, _)| u == uuid).unwrap();
        assert_eq!(&again.1, ing, "merge restamped {uuid}");
    }
}

/// An edge copied by corrections would pass its original `ingested_at`; the insert helper keeps
/// a non-empty value (it only stamps when empty).
#[test]
fn insert_helpers_keep_a_preexisting_ingest_time() {
    let (db, _dir) = make_db();
    let conn = db.connect().unwrap();
    for u in ["a", "b"] {
        conn.insert_entity(&EntityRow {
            uuid: u.into(),
            name: u.into(),
            group_id: "g".into(),
            labels: vec!["Entity".into()],
            created_at: "2026-01-01 00:00:00".into(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            ..Default::default()
        })
        .unwrap();
    }
    conn.insert_relates_to_edge(&RelatesToEdge {
        uuid: "r1".into(),
        name: "KNOWS".into(),
        source_node_uuid: "a".into(),
        target_node_uuid: "b".into(),
        group_id: "g".into(),
        fact: "f".into(),
        fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
        created_at: "2026-01-01 00:00:00".into(),
        ingested_at: "2025-05-05T05:05:05.000000+00:00".into(),
        attributes: "{}".into(),
        ..Default::default()
    })
    .unwrap();
    conn.insert_episodic(&EpisodicRow {
        uuid: "ep".into(),
        name: "ep".into(),
        group_id: "g".into(),
        created_at: "2026-01-01 00:00:00".into(),
        valid_at: "2026-01-01 00:00:00".into(),
        content_embedding: vec![1.0, 0.0, 0.0, 0.0],
        attributes: "{}".into(),
        ..Default::default()
    })
    .unwrap();
    let edges = conn.get_edges_by_uuids(&["r1"]).unwrap();
    assert_eq!(edges[0].ingested_at, "2025-05-05 05:05:05");
    let (_, eps, _) = snapshot(&db);
    assert!((0..60).contains(&age_secs(&eps[0].1)));
}

/// SC-002 / FR-006: a WAL replay onto a fresh DB reproduces every record's ingest time.
#[tokio::test]
async fn wal_replay_preserves_ingest_times() {
    let (db, _dir) = make_db();
    let wal_dir = TempDir::new().unwrap();
    let state = make_state(Arc::clone(&db), Some(wal_dir.path().to_path_buf()));
    ingest(
        &state,
        "c1",
        "Alice works at Acme Corp.",
        "2026-08-04T00:00:00Z",
    )
    .await;
    call(
        "knowledge_assert_entity",
        json!({"name": "Zed", "group_id": "liminis"}),
        &state,
    )
    .await;
    let original = snapshot(&db);
    assert!(!original.0.is_empty() && !original.1.is_empty() && !original.2.is_empty());

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let dir2 = TempDir::new().unwrap();
    let db2 = Db::open(dir2.path().join("r.db").to_str().unwrap()).unwrap();
    {
        let conn = db2.connect().unwrap();
        conn.init_schema(DIM).unwrap();
        WalReplayer::new(wal_dir.path())
            .replay(&conn, lcg_core::zero_vector_embed_fn(DIM), DIM)
            .unwrap();
        // The post-replay backfill every rebuild site runs must be a no-op for records whose
        // WAL lines already carry `ingested_at`.
        schema::backfill_ingested_at(&conn, &[wal_dir.path().to_path_buf()]).unwrap();
    }
    assert_eq!(snapshot(&db2), original);
}

/// Upgrade backfill (FR-007, SC-004): WAL creating-line `ts` first, `created_at` otherwise;
/// dump-style `MERGE … SET` lines (stamped with compaction time) are never a signal; idempotent.
#[test]
fn upgrade_backfill_prefers_wal_creating_ts_then_created_at() {
    let (db, _dir) = make_db();
    let wal_dir = TempDir::new().unwrap();
    let conn = db.connect().unwrap();
    // Pre-feature rows: no ingested_at.
    for (u, label_cypher) in [
        ("ent-wal", "CREATE (:Entity {uuid: 'ent-wal', name: 'w', group_id: 'g', created_at: timestamp('2020-01-01 00:00:00')})"),
        ("ent-dump", "CREATE (:Entity {uuid: 'ent-dump', name: 'd', group_id: 'g', created_at: timestamp('2020-02-02 00:00:00')})"),
        ("ent-none", "CREATE (:Entity {uuid: 'ent-none', name: 'n', group_id: 'g', created_at: timestamp('2020-03-03 00:00:00')})"),
    ] {
        let _ = u;
        conn.run_cypher(label_cypher).unwrap();
    }
    conn.run_cypher("CREATE (:Episodic {uuid: 'ep-wal', name: 'e', group_id: 'g', created_at: timestamp('2020-04-04 00:00:00'), valid_at: timestamp('2020-04-04 00:00:00')})").unwrap();
    let lines = [
        json!({"seq":1,"ts":"2026-09-01T10:00:00.000000+00:00","db":"d",
               "cypher":"CREATE (:Entity {uuid: $uuid, name: $name})","params":{"uuid":"ent-wal","name":"w"}}),
        // A later duplicate creator for the same uuid must lose to the first.
        json!({"seq":2,"ts":"2026-09-02T10:00:00.000000+00:00","db":"d",
               "cypher":"CREATE (:Entity {uuid: $uuid, name: $name})","params":{"uuid":"ent-wal","name":"w"}}),
        // Dump/compaction template: ts is compaction time → ignored.
        json!({"seq":3,"ts":"2026-09-30T10:00:00.000000+00:00","db":"d",
               "cypher":"MERGE (n:Entity {uuid: $uuid}) SET n.name = $name","params":{"uuid":"ent-dump","name":"d"}}),
        json!({"seq":4,"ts":"2026-09-03T10:00:00.000000+00:00","db":"d",
               "cypher":"CREATE (:Episodic {uuid: $uuid, name: $name})","params":{"uuid":"ep-wal","name":"e"}}),
    ];
    std::fs::write(
        wal_dir.path().join("wal-0-0.jsonl"),
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();

    schema::ensure_ingested_at_backfill(&conn, &[wal_dir.path().to_path_buf()]);

    let (ents, eps, _) = snapshot(&db);
    let get = |v: &[(String, String)], u: &str| v.iter().find(|(x, _)| x == u).unwrap().1.clone();
    assert_eq!(get(&ents, "ent-wal"), "2026-09-01 10:00:00");
    assert_eq!(get(&ents, "ent-dump"), "2020-02-02 00:00:00");
    assert_eq!(get(&ents, "ent-none"), "2020-03-03 00:00:00");
    assert_eq!(get(&eps, "ep-wal"), "2026-09-03 10:00:00");

    // Marker is complete: a second call (even with changed data) does no work.
    conn.run_cypher("MATCH (n:Entity {uuid: 'ent-none'}) SET n.ingested_at = NULL")
        .unwrap();
    schema::ensure_ingested_at_backfill(&conn, &[wal_dir.path().to_path_buf()]);
    let (ents, _, _) = snapshot(&db);
    assert_eq!(get(&ents, "ent-none"), "");
    // …while the unconditional rebuild-site entry point heals it, and never rewrites a set value.
    schema::backfill_ingested_at(&conn, &[wal_dir.path().to_path_buf()]).unwrap();
    let (ents, _, _) = snapshot(&db);
    assert_eq!(get(&ents, "ent-none"), "2020-03-03 00:00:00");
    assert_eq!(get(&ents, "ent-wal"), "2026-09-01 10:00:00");
}

/// A torn / non-UTF-8 WAL line must be skipped, not end the pass: creating lines after it still
/// supply their WAL `ts` rather than silently falling back to `created_at`.
#[test]
fn upgrade_backfill_skips_an_unreadable_wal_line() {
    let (db, _dir) = make_db();
    let wal_dir = TempDir::new().unwrap();
    let conn = db.connect().unwrap();
    conn.run_cypher("CREATE (:Entity {uuid: 'ent-after-bad', name: 'a', group_id: 'g', created_at: timestamp('2020-01-01 00:00:00')})").unwrap();
    let good = json!({"seq":2,"ts":"2026-09-01T10:00:00.000000+00:00","db":"d",
        "cypher":"CREATE (:Entity {uuid: $uuid, name: $name})","params":{"uuid":"ent-after-bad","name":"a"}});
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"{\"seq\":1,\"cypher\":\"CREATE (:Entity {\xff\xfe\n");
    bytes.extend_from_slice(good.to_string().as_bytes());
    bytes.push(b'\n');
    std::fs::write(wal_dir.path().join("wal-0-0.jsonl"), bytes).unwrap();

    schema::ensure_ingested_at_backfill(&conn, &[wal_dir.path().to_path_buf()]);

    let (ents, _, _) = snapshot(&db);
    let v = &ents.iter().find(|(x, _)| x == "ent-after-bad").unwrap().1;
    assert_eq!(v, "2026-09-01 10:00:00");
}

/// `migrate` adds the column to a pre-feature database exactly once (probe-then-ALTER).
#[test]
fn migrate_adds_ingested_at_columns_idempotently() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().join("old.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.run_cypher("CREATE NODE TABLE Entity (uuid STRING PRIMARY KEY, name STRING)")
        .unwrap();
    conn.run_cypher("CREATE NODE TABLE Episodic (uuid STRING PRIMARY KEY, name STRING)")
        .unwrap();
    conn.run_cypher(
        "CREATE NODE TABLE RelatesToNode_ (uuid STRING PRIMARY KEY, relation_type STRING, \
         episodes STRING[], expired_at TIMESTAMP)",
    )
    .unwrap();
    conn.run_cypher("CREATE REL TABLE MENTIONS (FROM Episodic TO Entity, group_id STRING)")
        .unwrap();
    for t in ["Entity", "Episodic", "RelatesToNode_"] {
        assert!(conn
            .run_cypher(&format!("MATCH (n:{t}) RETURN n.ingested_at LIMIT 0"))
            .is_err());
    }
    schema::migrate(&conn, DIM);
    schema::migrate(&conn, DIM);
    for t in ["Entity", "Episodic", "RelatesToNode_"] {
        conn.run_cypher(&format!("MATCH (n:{t}) RETURN n.ingested_at LIMIT 0"))
            .unwrap_or_else(|e| panic!("{t}.ingested_at must bind after migrate: {e}"));
    }
}

/// ADR-0028 / FR-006: dump compaction carries `ingested_at` through unchanged — it neither
/// drops the column nor restamps with the compaction time.
#[tokio::test]
async fn dump_then_replay_preserves_ingest_times() {
    let (db, _dir) = make_db();
    let state = make_state(Arc::clone(&db), None);
    ingest(
        &state,
        "c1",
        "Alice works at Acme Corp.",
        "2026-08-04T00:00:00Z",
    )
    .await;
    let original = snapshot(&db);
    assert!(!original.0.is_empty() && !original.1.is_empty() && !original.2.is_empty());

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let out = TempDir::new().unwrap();
    let target = out.path().join("dump");
    let r = call(
        "knowledge_dump_wal",
        json!({"target_dir": target.to_str().unwrap()}),
        &state,
    )
    .await;
    assert_eq!(r["success"], true, "{r}");

    let dir2 = TempDir::new().unwrap();
    let db2 = Db::open(dir2.path().join("r.db").to_str().unwrap()).unwrap();
    {
        let conn = db2.connect().unwrap();
        conn.init_schema(DIM).unwrap();
        WalReplayer::new(&target)
            .replay(&conn, lcg_core::zero_vector_embed_fn(DIM), DIM)
            .unwrap();
        // Dump lines are not a creating-line signal, so the WAL pass must leave them alone.
        schema::backfill_ingested_at(&conn, std::slice::from_ref(&target)).unwrap();
    }
    assert_eq!(snapshot(&db2), original);
}

/// Corrections that re-create an edge under a new uuid (`same_as` re-pointing) carry the
/// original edge's ingest time rather than restamping the fact as newly learned.
#[tokio::test]
async fn same_as_edge_copy_keeps_original_ingest_time() {
    let (db, _dir) = make_db();
    let workspace = TempDir::new().unwrap();
    let liminis = workspace.path().join(".liminis");
    std::fs::create_dir_all(&liminis).unwrap();
    std::fs::write(
        liminis.join("knowledge-corrections.yaml"),
        "corrections:\n  - id: c-1\n    type: same_as\n    canonical: \"Canon\"\n    aliases:\n      - \"Alias\"\n",
    )
    .unwrap();
    {
        let conn = db.connect().unwrap();
        for (u, n) in [("canon", "Canon"), ("alias", "Alias"), ("other", "Other")] {
            conn.insert_entity(&EntityRow {
                uuid: u.into(),
                name: n.into(),
                group_id: "g".into(),
                labels: vec!["Entity".into()],
                created_at: "2026-01-01 00:00:00".into(),
                name_embedding: vec![1.0, 0.0, 0.0, 0.0],
                summary: "s".into(),
                attributes: "{}".into(),
                ..Default::default()
            })
            .unwrap();
        }
        conn.insert_relates_to_edge(&RelatesToEdge {
            uuid: "old-edge".into(),
            name: "KNOWS".into(),
            source_node_uuid: "alias".into(),
            target_node_uuid: "other".into(),
            group_id: "g".into(),
            fact: "alias knows other".into(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            created_at: "2026-01-01 00:00:00".into(),
            ingested_at: "2025-05-05T05:05:05.000000+00:00".into(),
            attributes: "{}".into(),
            ..Default::default()
        })
        .unwrap();
    }
    let mut state = make_state(Arc::clone(&db), None);
    Arc::get_mut(&mut state).unwrap().workspace_root = Some(workspace.path().to_path_buf());
    let r = call("knowledge_apply_corrections", json!({}), &state).await;
    assert_eq!(r["success"], true, "{r}");

    let conn = db.connect().unwrap();
    let edges = conn.get_edges_by_group_ids(None).unwrap();
    let copy = edges
        .iter()
        .find(|e| e.uuid != "old-edge" && e.source_node_uuid == "canon")
        .expect("re-pointed copy of the alias edge");
    assert_eq!(
        copy.ingested_at, "2025-05-05 05:05:05",
        "copy must keep the original ingest time"
    );
}

/// `knowledge_add_cross_group_edge` stamps the service clock on the shadow node it creates.
#[tokio::test]
async fn cross_group_edge_is_stamped() {
    let (db, _dir) = make_db();
    let state = make_state(Arc::clone(&db), None);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "hub".into(),
            name: "hub".into(),
            group_id: "layer".into(),
            labels: vec!["Entity".into()],
            created_at: "2026-01-01 00:00:00".into(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "s".into(),
            attributes: "{}".into(),
            ..Default::default()
        })
        .unwrap();
    }
    let r = call(
        "knowledge_add_cross_group_edge",
        json!({
            "name": "REFERENCES", "group_id": "layer", "source": {"uuid": "hub"},
            "target": {"source_group_id": "other", "endpoint_name": "widget"},
            "fact": "hub references widget",
        }),
        &state,
    )
    .await;
    let uuid = r["uuid"].as_str().unwrap();
    let conn = db.connect().unwrap();
    let edges = conn.get_relates_to_by_uuids(&[uuid.to_string()]).unwrap();
    assert!(
        (0..60).contains(&age_secs(&edges[0].ingested_at)),
        "{:?}",
        edges[0].ingested_at
    );
}
