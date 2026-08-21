// #208 integration tests: Phase B's hybrid dedup query auto-heals a missing HNSW/FTS index
// instead of failing the chunk, mirroring the search handlers' existing auto-heal pattern
// (ADR-0025). Companion to `auto_heal_index_integration.rs` (search-handler path) and
// `dedup_integration.rs` (threshold/overlap correctness, no auto-heal).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwapOption;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::Embedder,
    episode,
    error::Error,
    extractor::{ExtractOptions, Extractor},
    handlers,
    ipc::IpcRequest,
    telemetry::NoopSink,
    types::{ExtractedEntity, ExtractionOutcome, ExtractionResult, SourceType},
    EntityRow,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::RwLock;

// Must exceed the largest entity count any single test in this file generates via
// UniqueEntityExtractor — AxisEmbedder is one-hot on axis `n % DIM`, so two entities land on
// the same axis (identical embedding, cosine similarity 1.0) whenever n wraps around DIM,
// and PassthroughDedupAdapter merges any embedding-similarity match unconditionally.
const DIM: usize = 64;

// lbug installs vector/fts extensions into a global directory (~/.lbdb/extension/) on the
// first Db::open call. Concurrent opens race on directory creation. Serialize them here
// (same pattern as auto_heal_index_integration.rs).
static DB_OPEN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// All tests in this file share one process, so `episode::hybrid_threshold()`'s `OnceLock`
/// caches whichever value is read first — every test must request the same threshold.
const HYBRID_THRESHOLD: &str = "5";

fn set_hybrid_threshold() {
    std::env::set_var("LIMINIS_DEDUP_HYBRID_THRESHOLD", HYBRID_THRESHOLD);
}

/// Extracts exactly one entity per call, with a name unique across the whole process
/// (`Entity-{n}`), so concurrent `add_episode` calls reliably grow `entity_count_in_group`
/// without colliding on the name-match short-circuit. No edges (keeps Phase A/C simple).
struct UniqueEntityExtractor {
    counter: Arc<AtomicUsize>,
}

impl UniqueEntityExtractor {
    fn new() -> Self {
        Self {
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Extractor for UniqueEntityExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, Error>> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            Ok(ExtractionResult {
                entities: vec![ExtractedEntity {
                    name: format!("Entity-{n}"),
                    entity_type: "Person".to_string(),
                    summary: format!("Auto-generated test entity #{n}"),
                    original_entity_type: None,
                }],
                edges: vec![],
            }
            .into())
        })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(async move { Ok(vec![String::new(); entities.len()]) })
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        _allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        Box::pin(async move { Ok(vec![String::new(); edges.len()]) })
    }
}

/// Deterministic, non-degenerate embedder: entity names of the form `Entity-{n}` map to a
/// one-hot vector on axis `n % DIM` (avoids the all-identical-zero-vector edge case a plain
/// `MockEmbedder` would produce for every name, which is uninteresting for exercising the
/// HNSW candidate path). Anything else (episode body, etc.) gets the zero vector — its value
/// doesn't matter for this test, only its presence/dimension does.
struct AxisEmbedder;

impl Embedder for AxisEmbedder {
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        let v = if let Some(suffix) = text.strip_prefix("Entity-") {
            let n: usize = suffix.parse().unwrap_or(0);
            let axis = n % DIM;
            (0..DIM)
                .map(|j| if j == axis { 1.0 } else { 0.0 })
                .collect()
        } else {
            vec![0.0f32; DIM]
        };
        Box::pin(async move { Ok(v) })
    }

    fn dim(&self) -> usize {
        DIM
    }
}

fn make_state_without_indices(extractor: Arc<dyn Extractor>) -> (Arc<AppState>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("dedup_auto_heal_test.db");
    let db_path_str = db_path.to_str().unwrap().to_string();
    let db = {
        let _open_guard = DB_OPEN_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        Arc::new(Db::open(&db_path_str).unwrap())
    };
    {
        let conn = db.connect().unwrap();
        // init_schema creates tables and FTS indexes but intentionally NOT HNSW vector
        // indexes — matches the bug scenario: a workspace where nothing has ever built them.
        conn.init_schema(DIM).unwrap();
    }
    let state = Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(AxisEmbedder),
        extractor,
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink: Arc::new(NoopSink),
        db_path: db_path_str,
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
    });
    (state, dir)
}

/// User Story 1 (SC-001/SC-004): on a fresh, never-indexed DB with several concurrent
/// writers, crossing the hybrid-dedup threshold must not fail any chunk with the
/// `entity_name_embedding_idx` missing-index Binder exception.
#[tokio::test]
async fn concurrent_ingest_past_hybrid_threshold_auto_heals() {
    set_hybrid_threshold();
    let (state, _dir) = make_state_without_indices(Arc::new(UniqueEntityExtractor::new()));
    let threshold: usize = HYBRID_THRESHOLD.parse().unwrap();

    // Seed sequentially up to the threshold first. Phase A/B run without a lock and Phase C
    // (the only serialization point) is what actually grows entity_count_in_group, so firing
    // every writer concurrently from an empty group would let them all read count=0 before any
    // commit lands — none would ever reach the hybrid path. Committing the first `threshold`
    // entities one at a time (as real sustained-but-not-yet-parallel ingest would) guarantees
    // the concurrent batch below genuinely observes count >= threshold.
    for i in 0..threshold {
        episode::add_episode(
            Arc::clone(&state),
            &format!("seed-ep-{i}"),
            &format!("seed-body-{i}"),
            "src",
            "desc",
            "2026-01-01 00:00:00",
            "load-test-group",
            SourceType::Text,
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("sequential seed ingest {i} failed: {e}"));
    }
    assert!(
        !state.indices_built.load(Ordering::Acquire),
        "seeding stays below the >= threshold hybrid gate and must not touch the HNSW index"
    );

    // Now fire the concurrent writers (mirrors #207's field reproduction: several parallel
    // writers pushing a group_id that has already reached the threshold). Every one of them
    // must see count >= threshold and take the hybrid path — the first to hit the missing
    // index triggers build_indices_once; the rest either race harmlessly into the same guarded
    // build or see indices_built already true.
    const WRITERS: usize = 20;
    let mut handles = Vec::with_capacity(WRITERS);
    for i in 0..WRITERS {
        let state = Arc::clone(&state);
        handles.push(tokio::spawn(async move {
            episode::add_episode(
                state,
                &format!("ep-{i}"),
                &format!("body-{i}"),
                "src",
                "desc",
                "2026-01-01 00:00:00",
                "load-test-group",
                SourceType::Text,
                None,
            )
            .await
        }));
    }

    for (i, h) in handles.into_iter().enumerate() {
        let result = h.await.expect("task panicked");
        assert!(
            result.is_ok(),
            "writer {i} failed: {:?} (should never fail with a missing-index error)",
            result.err()
        );
    }

    assert!(
        state.indices_built.load(Ordering::Acquire),
        "indices_built should be true after the hybrid path auto-healed"
    );

    let db = state.db.load_full().unwrap();
    let conn = db.connect().unwrap();
    let count = conn.entity_count_in_group("load-test-group").unwrap();
    assert_eq!(
        count,
        threshold + WRITERS,
        "all seeded + concurrently-ingested entities should be present"
    );
}

/// User Story 2 (SC-002): after `knowledge_clear_all`, the very next ingest that crosses the
/// hybrid-dedup threshold must succeed with no intervening search call having built the index.
#[tokio::test]
async fn post_clear_all_ingest_past_threshold_succeeds() {
    set_hybrid_threshold();
    let (state, _dir) = make_state_without_indices(Arc::new(UniqueEntityExtractor::new()));

    // First pass: ingest past the threshold so indices get built and populated.
    const FIRST_PASS: usize = 8;
    for i in 0..FIRST_PASS {
        episode::add_episode(
            Arc::clone(&state),
            &format!("pre-ep-{i}"),
            &format!("pre-body-{i}"),
            "src",
            "desc",
            "2026-01-01 00:00:00",
            "clear-test-group",
            SourceType::Text,
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("first-pass ingest {i} failed: {e}"));
    }
    assert!(state.indices_built.load(Ordering::Acquire));

    // knowledge_clear_all: deletes and reinitializes the DB (init_schema only — no HNSW
    // indexes) and resets indices_built to false.
    let clear_request = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: "knowledge_clear_all".to_string(),
        params: json!({"confirm": true}),
    };
    let response = handlers::dispatch(clear_request, Arc::clone(&state), None).await;
    let v: serde_json::Value = serde_json::to_value(&response).unwrap();
    assert!(
        v.get("result").is_some(),
        "knowledge_clear_all should succeed, got: {v}"
    );
    assert!(!state.indices_built.load(Ordering::Acquire));

    // Second pass: resume ingest past the threshold immediately, with no search call in
    // between. Every chunk must succeed via the dedup-path auto-heal (FR-005).
    const SECOND_PASS: usize = 8;
    for i in 0..SECOND_PASS {
        episode::add_episode(
            Arc::clone(&state),
            &format!("post-ep-{i}"),
            &format!("post-body-{i}"),
            "src",
            "desc",
            "2026-01-01 00:00:00",
            "clear-test-group",
            SourceType::Text,
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("post-clear ingest {i} failed: {e}"));
    }
    assert!(
        state.indices_built.load(Ordering::Acquire),
        "indices_built should be true again after the post-clear auto-heal"
    );
}

/// FR-006 (the "already true" quirk mirrored from the search handlers): if `indices_built` is
/// already `true` but the hybrid dedup query still fails with a missing-index error (a stale
/// flag — e.g. this DB snapshot never actually got indexed), the chunk fails with
/// `MISSING_INDEX_USER_MSG` rather than attempting a redundant rebuild.
#[tokio::test]
async fn stale_indices_built_flag_maps_to_user_message_without_rebuilding() {
    set_hybrid_threshold();
    let (state, _dir) = make_state_without_indices(Arc::new(UniqueEntityExtractor::new()));

    // Seed entities directly past the threshold (bypassing add_episode) so the very next
    // add_episode call already sees entity_count_in_group >= threshold and takes the hybrid
    // path immediately.
    {
        let db = state.db.load_full().unwrap();
        let conn = db.connect().unwrap();
        let threshold: usize = HYBRID_THRESHOLD.parse().unwrap();
        for i in 0..=threshold {
            conn.insert_entity(&EntityRow {
                uuid: format!("seed-{i:04}"),
                name: format!("Seed-{i}"),
                group_id: "stale-flag-group".to_string(),
                labels: vec!["Entity".to_string()],
                created_at: "2026-01-01 00:00:00".to_string(),
                name_embedding: vec![0.0f32; DIM],
                summary: format!("Seed entity {i}"),
                attributes: "{}".to_string(),
                ..Default::default()
            })
            .unwrap();
        }
    }

    // Simulate a stale flag: the session believes indices are built, but this particular DB
    // (never had build_indices_and_constraints called on it) genuinely has none.
    state.indices_built.store(true, Ordering::Release);

    let result = episode::add_episode(
        Arc::clone(&state),
        "ep-stale",
        "body-stale",
        "src",
        "desc",
        "2026-01-01 00:00:00",
        "stale-flag-group",
        SourceType::Text,
        None,
    )
    .await;

    let err = result.expect_err("expected a missing-index user error, got success");
    let msg = err.to_string();
    assert!(
        !msg.contains("Binder exception:"),
        "raw binder error must not be surfaced (FR-007-adjacent), got: {msg}"
    );
    assert!(
        msg.contains("knowledge_build_indices"),
        "error must name the recovery step, got: {msg}"
    );
}
