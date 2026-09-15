//! Hybrid search must return results in fused-rank order.
//!
//! `hybrid_entity_search` and `hybrid_edge_search` rank candidates with RRF, then fetch the rows
//! with a `WHERE uuid IN $uuids` lookup that has no `ORDER BY` — which returned the right top-k
//! set in storage (insertion) order, so every query came back ranked identically no matter what
//! was asked. These tests pin the ranking end to end through the IPC handlers, for both
//! `knowledge_find_entities` and `knowledge_find_relationships`.
//!
//! Both use `NameMapEmbedder` so the ranking is fully determined by chosen vectors: four items are
//! inserted alpha → delta, and the query's vector is most similar to delta, then charlie, bravo,
//! alpha — the exact reverse of insertion order. The query text shares no words with any item, so
//! BM25 contributes nothing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{Embedder, NameMapEmbedder},
    extractor::ConfigurableExtractor,
    handlers,
    ipc::IpcRequest,
    telemetry::NoopSink,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const DIM: usize = 4;
const GRP: &str = "test-group";
const QUERY: &str = "zulu unrelated query";
/// Query similarity rises with the one-hot axis index, so axis 3 (the last inserted) ranks first.
const QUERY_VECTOR: [f32; DIM] = [0.1, 0.3, 0.6, 1.0];
const EXPECTED: [&str; 4] = ["delta", "charlie", "bravo", "alpha"];

fn one_hot(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0; DIM];
    v[axis] = 1.0;
    v
}

fn state_with(dir: &TempDir, embedder: Arc<dyn Embedder>) -> Arc<AppState> {
    let db = Arc::new(Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
        conn.build_indices_and_constraints().unwrap();
    }
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder,
        extractor: Arc::new(ConfigurableExtractor::new(vec![])),
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

async fn dispatch(method: &str, params: Value, state: Arc<AppState>) -> Value {
    let request = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    };
    let response = serde_json::to_value(handlers::dispatch(request, state, None).await).unwrap();
    assert!(
        response.get("error").is_none(),
        "{method} failed: {}",
        response["error"]
    );
    response["result"].clone()
}

#[tokio::test]
async fn find_entities_returns_results_in_ranked_order_not_insertion_order() {
    let dir = TempDir::new().unwrap();

    // Every entity gets a summary embedded to the same vector as its name, so the name-vector and
    // summary-vector RRF inputs agree. (An entity with *no* summary stores a zero-vector
    // `summary_embedding` sentinel, which the summary index still returns in arbitrary order —
    // that would add a second, conflicting ranking and make the expected order meaningless.)
    let entities = [
        ("alpha", "first placeholder note"),
        ("bravo", "second placeholder note"),
        ("charlie", "third placeholder note"),
        ("delta", "fourth placeholder note"),
    ];
    let mut vectors = HashMap::new();
    for (axis, (name, summary)) in entities.iter().enumerate() {
        vectors.insert(name.to_string(), one_hot(axis));
        vectors.insert(summary.to_string(), one_hot(axis));
    }
    vectors.insert(QUERY.to_string(), QUERY_VECTOR.to_vec());
    let state = state_with(&dir, Arc::new(NameMapEmbedder::new(DIM, vectors)));

    for (name, summary) in entities {
        dispatch(
            "knowledge_assert_entity",
            json!({ "name": name, "summary": summary, "group_id": GRP }),
            Arc::clone(&state),
        )
        .await;
    }

    let result = dispatch(
        "knowledge_find_entities",
        json!({ "query": QUERY, "group_ids": [GRP], "num_results": 4 }),
        Arc::clone(&state),
    )
    .await;
    let ranked: Vec<&str> = result["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();

    assert_eq!(
        ranked, EXPECTED,
        "find_entities must return fused-rank order, not storage order"
    );
}

#[tokio::test]
async fn find_relationships_returns_results_in_ranked_order_not_insertion_order() {
    let dir = TempDir::new().unwrap();

    // Four relationships from one hub entity, inserted alpha → delta. Each fact is embedded to a
    // one-hot vector; edge search fuses only BM25 (empty here) and the fact vector, so the fact
    // vectors alone decide the order. Endpoint entity names get distinct vectors of their own.
    let edges = [
        ("alpha", "first placeholder statement"),
        ("bravo", "second placeholder statement"),
        ("charlie", "third placeholder statement"),
        ("delta", "fourth placeholder statement"),
    ];
    let mut vectors = HashMap::new();
    vectors.insert("hub".to_string(), vec![0.5, 0.5, 0.5, 0.5]);
    for (axis, (target, fact)) in edges.iter().enumerate() {
        vectors.insert(format!("{target} node"), one_hot(axis));
        vectors.insert(fact.to_string(), one_hot(axis));
    }
    vectors.insert(QUERY.to_string(), QUERY_VECTOR.to_vec());
    let state = state_with(&dir, Arc::new(NameMapEmbedder::new(DIM, vectors)));

    // knowledge_assert_relationship only resolves existing endpoints within its group.
    let endpoints = std::iter::once("hub".to_string())
        .chain(edges.iter().map(|(target, _)| format!("{target} node")));
    for name in endpoints {
        dispatch(
            "knowledge_assert_entity",
            json!({ "name": name, "group_id": GRP }),
            Arc::clone(&state),
        )
        .await;
    }

    for (target, fact) in edges {
        dispatch(
            "knowledge_assert_relationship",
            json!({
                "source_name": "hub",
                "target_name": format!("{target} node"),
                "predicate": "RELATES_TO",
                "fact": fact,
                "group_id": GRP,
            }),
            Arc::clone(&state),
        )
        .await;
    }

    let result = dispatch(
        "knowledge_find_relationships",
        json!({ "query": QUERY, "group_ids": [GRP], "num_results": 4 }),
        Arc::clone(&state),
    )
    .await;
    let fact_to_target: HashMap<&str, &str> = edges.iter().map(|(t, f)| (*f, *t)).collect();
    let ranked: Vec<&str> = result["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .map(|e| fact_to_target[e["fact"].as_str().unwrap()])
        .collect();

    assert_eq!(
        ranked, EXPECTED,
        "find_relationships must return fused-rank order, not storage order"
    );
}
