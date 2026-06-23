/// Integration tests for knowledge_canonicalize_relations — SC-001 through SC-007 and spec #163.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{MockEmbedder, NameMapEmbedder},
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    ontology::{EntityTypeDef, Ontology, OntologyMode, RelationTypeDef},
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

fn test_ontology() -> Arc<Ontology> {
    Arc::new(Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![EntityTypeDef {
            name: "Person".to_string(),
            description: None,
            parent: None,
        }],
        ancestor_map: HashMap::new(),
        relation_types: vec![
            RelationTypeDef {
                name: "AUTHORED".to_string(),
                description: Some("a person authored something".to_string()),
                source_type: None,
                target_type: None,
                aliases: vec!["WROTE".to_string(), "AUTHORED_BY".to_string()],
                keywords: vec!["author".to_string(), "writ".to_string()],
            },
            RelationTypeDef {
                name: "AFFILIATED_WITH".to_string(),
                description: Some("an entity is affiliated with another".to_string()),
                source_type: None,
                target_type: None,
                aliases: vec!["WORKS_FOR".to_string()],
                keywords: vec!["affiliat".to_string(), "employ".to_string()],
            },
            RelationTypeDef {
                name: "MANAGES".to_string(),
                description: Some("one entity manages or oversees another".to_string()),
                source_type: None,
                target_type: None,
                aliases: vec![],
                keywords: vec!["manag".to_string(), "supervis".to_string()],
            },
        ],
    })
}

fn make_state(db: Arc<Db>, ontology: Option<Arc<Ontology>>) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
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
        ontology,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

fn make_state_with_wal(
    db: Arc<Db>,
    wal_dir: &std::path::Path,
    ontology: Option<Arc<Ontology>>,
) -> Arc<AppState> {
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
        ontology,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

fn make_state_with_name_map_embedder(
    db: Arc<Db>,
    wal_dir: &std::path::Path,
    ontology: Option<Arc<Ontology>>,
    map: HashMap<String, Vec<f32>>,
) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = WalWriter::new(wal_dir, 10_000, 0).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(NameMapEmbedder::new(DIM, map)),
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
        ontology,
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

fn make_edge(src: &str, dst: &str, name: &str) -> RelatesToEdge {
    make_edge_with_rt(src, dst, name, None, "")
}

fn make_edge_with_rt(
    src: &str,
    dst: &str,
    name: &str,
    rt: Option<&str>,
    fact: &str,
) -> RelatesToEdge {
    RelatesToEdge {
        uuid: Uuid::new_v4().to_string(),
        name: name.to_string(),
        source_node_uuid: src.to_string(),
        target_node_uuid: dst.to_string(),
        group_id: GRP.to_string(),
        fact: if fact.is_empty() {
            format!("{src} {name} {dst}")
        } else {
            fact.to_string()
        },
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

/// Dispatches a request and returns the `result` field (panics if error).
async fn dispatch(method: &str, params: Value, state: Arc<AppState>) -> Value {
    let v = dispatch_raw(method, params, state).await;
    assert!(
        v.get("error").is_none(),
        "expected result, got error: {}",
        v["error"]
    );
    v["result"].clone()
}

/// Reads all JSONL files in wal_dir and counts total non-empty lines.
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

// ── Test 1: dry_run returns counts without mutations ──────────────────────────

/// SC-004: dry_run: true returns coverage report, no mutations.
#[tokio::test]
async fn test_dry_run_no_mutations() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let onto = test_ontology();

    // Insert 2 entities + edges in all 3 categories
    let src = make_entity("Alice");
    let dst = make_entity("Bob");
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();
        // Mappable: WROTE → will map to AUTHORED
        conn.insert_relates_to_edge(&make_edge(&src.uuid, &dst.uuid, "WROTE"))
            .unwrap();
        // Noise: X → Y pattern
        conn.insert_relates_to_edge(&make_edge(&src.uuid, &dst.uuid, "ALICE → BOB"))
            .unwrap();
        // Residual: unique name, no keyword match
        conn.insert_relates_to_edge(&make_edge(
            &src.uuid,
            &dst.uuid,
            "IS_RELATED_TO_CONCEPT_DELTA",
        ))
        .unwrap();
    }

    let state = make_state_with_wal(db.clone(), wal_dir.path(), Some(onto));
    let wal_lines_before = count_wal_lines(wal_dir.path());

    let result = dispatch(
        "knowledge_canonicalize_relations",
        json!({ "dry_run": true }),
        state,
    )
    .await;

    assert_eq!(result["dry_run"], json!(true));
    assert_eq!(result["total_edges"], json!(3));
    assert_eq!(result["mapped_count"], json!(1));
    assert_eq!(result["noise_count"], json!(1));
    assert_eq!(result["residual_count"], json!(1));

    // No WAL mutations written
    let wal_lines_after = count_wal_lines(wal_dir.path());
    assert_eq!(
        wal_lines_after, wal_lines_before,
        "dry_run must not write WAL entries"
    );

    // DB is unchanged: all 3 edges still exist
    let conn = db.connect().unwrap();
    let edges = conn.list_relationships(None, 100).unwrap();
    assert_eq!(edges.len(), 3, "dry_run must not delete any edges");
}

// ── Test 2: lexical mapping sets relation_type ────────────────────────────────

/// SC-001: edges matching aliases/keywords get their relation_type set.
#[tokio::test]
async fn test_lexical_mapping_sets_relation_type() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let onto = test_ontology();

    let src = make_entity("Alice");
    let dst = make_entity("Acme");
    let edge_uuids: Vec<String>;
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();

        let edges = vec![
            make_edge(&src.uuid, &dst.uuid, "WROTE"),
            make_edge(&src.uuid, &dst.uuid, "AUTHORED_BY"),
            make_edge(&src.uuid, &dst.uuid, "authoring"),
            make_edge_with_rt(&src.uuid, &dst.uuid, "AUTHORED", None, ""),
        ];
        edge_uuids = edges.iter().map(|e| e.uuid.clone()).collect();
        for e in &edges {
            conn.insert_relates_to_edge(e).unwrap();
        }
    }

    let state = make_state(db.clone(), Some(onto));
    let result = dispatch(
        "knowledge_canonicalize_relations",
        json!({ "dry_run": false }),
        state,
    )
    .await;

    assert!(!result["dry_run"].as_bool().unwrap_or(true));
    assert_eq!(result["mapped_count"], json!(4));
    assert_eq!(result["noise_count"], json!(0));
    assert_eq!(result["residual_count"], json!(0));

    // All edges now have relation_type = "AUTHORED"
    let conn = db.connect().unwrap();
    let edges = conn
        .get_edges_by_uuids(&edge_uuids.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .unwrap();
    for edge in &edges {
        assert_eq!(
            edge.relation_type.as_deref(),
            Some("AUTHORED"),
            "edge {} should have relation_type AUTHORED, got {:?}",
            edge.uuid,
            edge.relation_type
        );
        // fact unchanged (contains src/dst/name from make_edge)
        assert!(!edge.fact.is_empty(), "fact should be preserved");
    }
    // Confirm edge count: all 4 edges still exist (no deletions)
    let all = conn.list_relationships(None, 100).unwrap();
    assert_eq!(all.len(), 4, "lexical mapping should not delete edges");
    let _ = edge_uuids; // used above
}

// ── Test 3: noise edges are deleted ──────────────────────────────────────────

/// SC-002: co-occurrence noise edges (X → Y pattern) are deleted.
#[tokio::test]
async fn test_noise_edges_deleted() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let onto = test_ontology();

    let src = make_entity("Alice");
    let dst = make_entity("Bob");
    let noise_uuids: Vec<String>;
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();

        let noise = vec![
            make_edge(&src.uuid, &dst.uuid, "ALICE → BOB"),
            make_edge(&src.uuid, &dst.uuid, "BRETT -> RAJI"),
            make_edge(&src.uuid, &dst.uuid, "FOO → BAR"),
        ];
        noise_uuids = noise.iter().map(|e| e.uuid.clone()).collect();
        for e in &noise {
            conn.insert_relates_to_edge(e).unwrap();
        }
        // Add one non-noise edge that should survive
        conn.insert_relates_to_edge(&make_edge(&src.uuid, &dst.uuid, "WROTE"))
            .unwrap();
    }

    let state = make_state_with_wal(db.clone(), wal_dir.path(), Some(onto));
    let result = dispatch(
        "knowledge_canonicalize_relations",
        json!({ "dry_run": false }),
        state,
    )
    .await;

    assert_eq!(result["noise_count"], json!(3));
    assert_eq!(result["mapped_count"], json!(1)); // WROTE → AUTHORED

    // Noise edges no longer exist
    let conn = db.connect().unwrap();
    let remaining = conn.list_relationships(None, 100).unwrap();
    assert_eq!(remaining.len(), 1, "only the non-noise edge should remain");

    // Check that noise UUIDs don't appear in surviving edges
    let surviving_uuids: Vec<&str> = remaining.iter().map(|e| e.uuid.as_str()).collect();
    for noise_uuid in &noise_uuids {
        assert!(
            !surviving_uuids.contains(&noise_uuid.as_str()),
            "noise edge {noise_uuid} should have been deleted"
        );
    }

    // WAL has delete mutations for the noise edges
    let wal_lines = count_wal_lines(wal_dir.path());
    assert!(
        wal_lines >= 3,
        "WAL should have at least 3 delete mutations for noise edges"
    );
}

// ── Test 4: residual edges marked UNCLASSIFIED ───────────────────────────────

/// SC-001 residual path: unmatched edges get UNCLASSIFIED, fact unchanged.
#[tokio::test]
async fn test_residual_marked_unclassified() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let onto = test_ontology();

    let src = make_entity("Alice");
    let dst = make_entity("Bob");
    let edge_uuids: Vec<String>;
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();

        let edges: Vec<RelatesToEdge> = (0..5)
            .map(|i| {
                make_edge_with_rt(
                    &src.uuid,
                    &dst.uuid,
                    &format!("IS_THE_NTH_RELATION_{i}"),
                    None,
                    &format!("unique fact sentence {i}"),
                )
            })
            .collect();
        edge_uuids = edges.iter().map(|e| e.uuid.clone()).collect();
        for e in &edges {
            conn.insert_relates_to_edge(e).unwrap();
        }
    }

    // MockEmbedder returns zero vectors → cosine_similarity is 0 → no embedding promotion
    let state = make_state(db.clone(), Some(onto));
    let result = dispatch(
        "knowledge_canonicalize_relations",
        json!({ "dry_run": false }),
        state,
    )
    .await;

    assert_eq!(result["residual_count"], json!(5));
    assert_eq!(result["mapped_count"], json!(0));
    assert_eq!(result["noise_count"], json!(0));

    // All 5 edges marked UNCLASSIFIED, facts preserved
    let conn = db.connect().unwrap();
    let edges = conn
        .get_edges_by_uuids(&edge_uuids.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .unwrap();
    for (i, edge) in edges.iter().enumerate() {
        assert_eq!(
            edge.relation_type.as_deref(),
            Some("UNCLASSIFIED"),
            "edge {i} should be UNCLASSIFIED"
        );
        assert!(
            edge.fact.contains("unique fact sentence"),
            "fact should be preserved on edge {i}"
        );
    }
}

// ── Test 5: embedding fallback promotes residual edges ────────────────────────

/// SC-001 embedding path: facts close to canonical gloss get promoted.
#[tokio::test]
async fn test_embedding_fallback_promotes_residual() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let onto = test_ontology();

    let src = make_entity("Alice");
    let dst = make_entity("Bob");
    let manages_fact = "Alice oversees Bob's annual performance review";
    let edge_uuid;
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();
        // An edge with a unique name that won't lexically match (no keywords), but whose fact
        // sentence semantically expresses MANAGES via embedding similarity
        let e = make_edge_with_rt(
            &src.uuid,
            &dst.uuid,
            "RELATES_TO_CONCEPT_DELTA_SEVENTEEN",
            None,
            manages_fact,
        );
        edge_uuid = e.uuid.clone();
        conn.insert_relates_to_edge(&e).unwrap();
    }

    // Set up NameMapEmbedder: both the canonical gloss and the fact sentence map to
    // the same unit vector → cosine similarity = 1.0 ≥ 0.7 threshold
    let gloss = "one entity manages or oversees another";
    let matching_vec = vec![1.0, 0.0, 0.0, 0.0];
    let map: HashMap<String, Vec<f32>> = [
        (gloss.to_string(), matching_vec.clone()),
        (manages_fact.to_string(), matching_vec.clone()),
    ]
    .into();
    let state = make_state_with_name_map_embedder(db.clone(), wal_dir.path(), Some(onto), map);

    let result = dispatch(
        "knowledge_canonicalize_relations",
        json!({ "dry_run": false, "embedding_threshold": 0.7 }),
        state,
    )
    .await;

    assert_eq!(
        result["embedding_fallback_promoted"],
        json!(1),
        "one edge should be promoted by embedding fallback"
    );
    assert_eq!(result["residual_count"], json!(0));

    let conn = db.connect().unwrap();
    let edges = conn.get_edges_by_uuids(&[edge_uuid.as_str()]).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(
        edges[0].relation_type.as_deref(),
        Some("MANAGES"),
        "embedding fallback should set relation_type to MANAGES"
    );
}

// ── Test 6: WAL round-trip fidelity ──────────────────────────────────────────

/// SC-003: WAL replay after canonicalize pass reproduces same relation_types.
#[tokio::test]
async fn test_wal_round_trip_fidelity() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let onto = test_ontology();

    let src = make_entity("Alice");
    let dst = make_entity("Bob");

    // Create AppState first so seed mutations and canonicalize mutations are written
    // through the same WalWriter session — guarantees file-sequence ordering on replay.
    // (Two separate WalWriter instances for the same wal_dir share a second-precision
    // timestamp prefix but have different random session_ids, making their alphabetical
    // sort order — and therefore replay order — non-deterministic.)
    let state = make_state_with_wal(db.clone(), wal_dir.path(), Some(onto));

    // Insert seed data and write its mutations through state's WalWriter
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();
        conn.insert_relates_to_edge(&make_edge(&src.uuid, &dst.uuid, "WROTE"))
            .unwrap();
        conn.insert_relates_to_edge(&make_edge(&src.uuid, &dst.uuid, "ALICE → BOB"))
            .unwrap();
        conn.insert_relates_to_edge(&make_edge_with_rt(
            &src.uuid,
            &dst.uuid,
            "IS_UNIQUE_RESIDUAL",
            None,
            "some unique fact",
        ))
        .unwrap();
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

    // Run canonicalize pass (appends mutations to same WAL session)
    dispatch(
        "knowledge_canonicalize_relations",
        json!({ "dry_run": false }),
        Arc::clone(&state),
    )
    .await;

    // Snapshot post-pass edge states from live DB
    let post_pass: Vec<(String, Option<String>)> = {
        let conn = db.connect().unwrap();
        let mut edges = conn.list_relationships(None, 100).unwrap();
        edges.sort_by(|a, b| a.uuid.cmp(&b.uuid));
        edges
            .into_iter()
            .map(|e| (e.uuid, e.relation_type))
            .collect()
    };

    // Create fresh DB and replay the full WAL
    let dir2 = TempDir::new().unwrap();
    let db2 = Arc::new(Db::open(dir2.path().join("replay.db").to_str().unwrap()).unwrap());
    {
        let conn2 = db2.connect().unwrap();
        conn2.init_schema(DIM).unwrap();
        let stats = WalReplayer::new(wal_dir.path()).replay(&conn2).unwrap();
        assert!(stats.lines_replayed > 0, "WAL replay should replay lines");
    }

    // Snapshot replayed state
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
        post_pass, replayed,
        "WAL replay must reproduce exact post-pass edge UUIDs and relation_types"
    );
}

// ── Test 7: idempotency — second run adds zero WAL mutations ──────────────────

/// SC-006: running the pass twice emits zero new WAL mutations on the second run.
#[tokio::test]
async fn test_idempotency_no_second_mutations() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let onto = test_ontology();

    let src = make_entity("Alice");
    let dst = make_entity("Bob");
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&src).unwrap();
        conn.insert_entity(&dst).unwrap();
        conn.insert_relates_to_edge(&make_edge(&src.uuid, &dst.uuid, "WROTE"))
            .unwrap();
        conn.insert_relates_to_edge(&make_edge(&src.uuid, &dst.uuid, "SPECIFIC_UNIQUE_REL"))
            .unwrap();
    }

    // First run
    let state = make_state_with_wal(db.clone(), wal_dir.path(), Some(Arc::clone(&onto)));
    dispatch(
        "knowledge_canonicalize_relations",
        json!({ "dry_run": false }),
        state,
    )
    .await;
    let wal_after_first = count_wal_lines(wal_dir.path());
    assert!(wal_after_first > 0, "first run must write WAL mutations");

    // Second run (same state, same DB)
    let state2 = make_state_with_wal(db.clone(), wal_dir.path(), Some(onto));
    dispatch(
        "knowledge_canonicalize_relations",
        json!({ "dry_run": false }),
        state2,
    )
    .await;
    let wal_after_second = count_wal_lines(wal_dir.path());

    assert_eq!(
        wal_after_first, wal_after_second,
        "second canonicalize run must emit zero new WAL mutations (idempotent)"
    );
}

// ── Test 8: fails fast without ontology ──────────────────────────────────────

/// FR-013: returns -32000 error if no ontology is loaded.
#[tokio::test]
async fn test_fails_fast_without_ontology() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    // No ontology
    let state = make_state(db, None);

    let v = dispatch_raw(
        "knowledge_canonicalize_relations",
        json!({ "dry_run": true }),
        state,
    )
    .await;

    // Should get a -32000 error response, not a result
    assert!(
        v.get("error").is_some(),
        "response without ontology must be an error, got: {v}"
    );
    assert_eq!(v["error"]["code"], json!(-32000));
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("relation_type") || msg.contains("ontology"),
        "error message should mention 'relation_types' or 'ontology', got: {msg}"
    );
}
