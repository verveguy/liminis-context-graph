//! Issue #445, User Story 2: bulk-extraction ingest batches its four Phase A embed sites (entity
//! names, per-entity summaries, salvage-endpoint lookups, edge facts) into one `embed_batch` call
//! each per chunk, instead of one `embed()` call per item.
//!
//! Uses `NameMapEmbedder` (a pure string->vector lookup) wrapped in `CountingEmbedder`, so a test
//! can assert both the number of round-trips issued (`batch_call_count()`/`call_count()`) and —
//! via retrieval — that each vector landed on the correct entity/edge, which a wrong batch/index
//! offset would silently scramble.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{CountingEmbedder, Embedder, NameMapEmbedder},
    extractor::ConfigurableExtractor,
    handlers,
    ipc::IpcRequest,
    telemetry::NoopSink,
    types::{ExtractedEdge, ExtractedEntity, ExtractionResult},
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const DIM: usize = 4;
const GRP: &str = "test-group";

fn open_db(dir: &TempDir) -> Arc<Db> {
    let db = Arc::new(Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
        conn.build_indices_and_constraints().unwrap();
    }
    db
}

fn make_state(
    db: Arc<Db>,
    embedder: Arc<dyn Embedder>,
    extractor: ConfigurableExtractor,
) -> Arc<AppState> {
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder,
        extractor: Arc::new(extractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink: Arc::new(NoopSink),
        db_path: "test.db".to_string(),
        wal_root: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writers: Arc::new(Mutex::new(HashMap::new())),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(true)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
        embedding_cache: Arc::new(lcg_core::EmbeddingCache::new()),
    })
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

fn find_entities_names(nodes: &Value) -> Vec<String> {
    nodes
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap().to_string())
        .collect()
}

/// Acceptance Scenarios 1-4: a chunk with multiple entities (non-empty summaries), an edge whose
/// endpoint is absent from the entity list (triggering salvage lookup), and multiple edge facts
/// should issue exactly one batch call per site — 4 total (name, summary, salvage, fact) — not
/// one call per item, and zero single-item `embed()` calls.
#[tokio::test]
async fn extraction_batches_all_four_phase_a_sites() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let alice_summary = "runs a neighborhood bakery";
    let bob_summary = "leads a distributed systems team";
    let collab_fact = "Alice and Bob collaborate on a project";
    let mentors_fact = "Dave mentors Carol";

    let mut map = HashMap::new();
    map.insert("Alice".to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    map.insert("Bob".to_string(), vec![0.0, 1.0, 0.0, 0.0]);
    map.insert("Carol".to_string(), vec![0.0, 0.0, 1.0, 0.0]);
    map.insert(alice_summary.to_string(), vec![0.9, 0.1, 0.0, 0.0]);
    map.insert(bob_summary.to_string(), vec![0.0, 0.9, 0.1, 0.0]);
    // "Dave" is the off-list edge endpoint; deliberately orthogonal to every entity name vector
    // above so it never salvage-matches — the test only cares that exactly one batch call is
    // issued for the lookup, not that it succeeds.
    map.insert("Dave".to_string(), vec![0.0, 0.0, 0.0, 1.0]);
    map.insert(collab_fact.to_string(), vec![0.2, 0.2, 0.2, 0.2]);
    map.insert(mentors_fact.to_string(), vec![0.3, 0.1, 0.1, 0.1]);

    let inner: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, map));
    let counting = Arc::new(CountingEmbedder::new(inner));
    let embedder: Arc<dyn Embedder> = counting.clone();

    let extractor = ConfigurableExtractor::new(vec![ExtractionResult {
        entities: vec![
            ExtractedEntity {
                name: "Alice".to_string(),
                entity_type: "Entity".to_string(),
                summary: alice_summary.to_string(),
                original_entity_type: None,
            },
            ExtractedEntity {
                name: "Bob".to_string(),
                entity_type: "Entity".to_string(),
                summary: bob_summary.to_string(),
                original_entity_type: None,
            },
            // Empty summary: excluded from the summary batch, gets a zero-vector sentinel
            // instead — proves the summary batch's non-empty filter survives batching.
            ExtractedEntity {
                name: "Carol".to_string(),
                entity_type: "Entity".to_string(),
                summary: "".to_string(),
                original_entity_type: None,
            },
        ],
        edges: vec![
            ExtractedEdge {
                source_name: "Alice".to_string(),
                target_name: "Bob".to_string(),
                fact: collab_fact.to_string(),
                ..Default::default()
            },
            ExtractedEdge {
                source_name: "Dave".to_string(),
                target_name: "Carol".to_string(),
                fact: mentors_fact.to_string(),
                ..Default::default()
            },
        ],
    }]);
    let state = make_state(db.clone(), embedder, extractor);

    let add_result = dispatch(
        "knowledge_add_episode",
        json!({
            "name": "episode-1",
            "episode_body": "irrelevant body text — ConfigurableExtractor ignores it",
            "source": "text",
            "reference_time": "2026-01-01T00:00:00Z",
            "group_id": GRP,
        }),
        Arc::clone(&state),
    )
    .await;
    assert!(add_result["episode_uuid"].as_str().is_some());

    assert_eq!(
        counting.batch_call_count(),
        4,
        "expected one batch call each for entity names, summaries, salvage lookup, and edge \
         facts — got {}",
        counting.batch_call_count()
    );
    assert_eq!(
        counting.call_count(),
        1,
        "the only single-item embed() call left should be the episode body's content_embedding \
         (out of scope for #445 — everything else routes through embed_batch now)"
    );

    // Correctness, not just call count: a wrong batch/index offset would scramble which vector
    // lands on which entity. Query by each entity's exact summary text (NameMapEmbedder maps
    // literal strings) and confirm it retrieves that specific entity, not a sibling.
    let alice_find = dispatch(
        "knowledge_find_entities",
        json!({ "query": alice_summary, "group_ids": [GRP], "num_results": 1 }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(find_entities_names(&alice_find["nodes"]), vec!["Alice"]);

    let bob_find = dispatch(
        "knowledge_find_entities",
        json!({ "query": bob_summary, "group_ids": [GRP], "num_results": 1 }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(find_entities_names(&bob_find["nodes"]), vec!["Bob"]);
}

/// A chunk with no edges never triggers the salvage batch (its containing block is guarded by
/// `!extraction.edges.is_empty()`). The edge-fact batch call still fires with an empty slice —
/// harmless per FR-006 (no network call for empty input) — so 3 batch calls are expected: name,
/// summary, and a no-op empty fact batch.
#[tokio::test]
async fn extraction_with_no_edges_batches_only_name_and_summary() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let mut map = HashMap::new();
    map.insert("Solo".to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    map.insert("a lone entity".to_string(), vec![0.0, 1.0, 0.0, 0.0]);

    let inner: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, map));
    let counting = Arc::new(CountingEmbedder::new(inner));
    let embedder: Arc<dyn Embedder> = counting.clone();

    let extractor = ConfigurableExtractor::new(vec![ExtractionResult {
        entities: vec![ExtractedEntity {
            name: "Solo".to_string(),
            entity_type: "Entity".to_string(),
            summary: "a lone entity".to_string(),
            original_entity_type: None,
        }],
        edges: vec![],
    }]);
    let state = make_state(db.clone(), embedder, extractor);

    dispatch(
        "knowledge_add_episode",
        json!({
            "name": "episode-1",
            "episode_body": "irrelevant",
            "source": "text",
            "reference_time": "2026-01-01T00:00:00Z",
            "group_id": GRP,
        }),
        Arc::clone(&state),
    )
    .await;

    assert_eq!(
        counting.batch_call_count(),
        3,
        "no edges: name batch + summary batch + a harmless empty (no-op) fact batch"
    );
}
