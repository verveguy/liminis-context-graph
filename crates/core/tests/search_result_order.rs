//! `knowledge_find_entities` must return results in fused-rank order.
//!
//! The hybrid search ranks candidates with RRF, then fetches the rows with a
//! `WHERE uuid IN $uuids` lookup that has no `ORDER BY` — which returned the right top-k set in
//! storage (insertion) order, so every query came back ranked identically no matter what was
//! asked. This pins the ranking end to end through the IPC handler.
//!
//! Uses `NameMapEmbedder` so the ranking is fully determined by chosen vectors: four entities are
//! inserted alpha → delta, and the query's vector is most similar to delta, then charlie, bravo,
//! alpha — the exact reverse of insertion order. The query text shares no words with any entity
//! name or summary, so BM25 contributes nothing, and each summary is embedded to the same vector
//! as its name, so the two vector lists agree: the fused ranking is fully determined.

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

    // Insertion order alpha → delta; similarity to the query rises in that same order, so the
    // correct ranking is the exact reverse of insertion.
    //
    // Every entity gets a summary embedded to the same vector as its name, so the name-vector and
    // summary-vector RRF inputs agree. (An entity with *no* summary stores a zero-vector
    // `summary_embedding` sentinel, which the summary index still returns in arbitrary order —
    // that would add a second, conflicting ranking and make the expected order meaningless.)
    let entities = [
        ("alpha", "first placeholder note", [1.0, 0.0, 0.0, 0.0]),
        ("bravo", "second placeholder note", [0.0, 1.0, 0.0, 0.0]),
        ("charlie", "third placeholder note", [0.0, 0.0, 1.0, 0.0]),
        ("delta", "fourth placeholder note", [0.0, 0.0, 0.0, 1.0]),
    ];
    let mut vectors = HashMap::new();
    for (name, summary, vector) in entities {
        vectors.insert(name.to_string(), vector.to_vec());
        vectors.insert(summary.to_string(), vector.to_vec());
    }
    vectors.insert(QUERY.to_string(), vec![0.1, 0.3, 0.6, 1.0]);
    let state = state_with(&dir, Arc::new(NameMapEmbedder::new(DIM, vectors)));

    for (name, summary, _) in entities {
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
        ranked,
        vec!["delta", "charlie", "bravo", "alpha"],
        "find_entities must return fused-rank order, not storage order"
    );
}
