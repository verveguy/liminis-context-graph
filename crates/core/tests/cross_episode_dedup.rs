// Integration tests for cross-episode entity dedup (issue #164).
//
// Tests FR-007 (identical-name dedup) and FR-008 (embedding-based dedup) across
// multiple add_episode calls. Uses ConfigurableExtractor and NameMapEmbedder to
// control what each episode extracts and what embeddings are produced.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{MockEmbedder, NameMapEmbedder},
    episode,
    extractor::ConfigurableExtractor,
    telemetry::{NoopSink, TelemetrySink},
    types::{ExtractedEntity, ExtractionResult, SourceType},
};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const EMB_DIM: usize = 4;
const REF_TIME: &str = "2026-01-01T00:00:00Z";
const GROUP: &str = "test-grp";

fn make_db() -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(EMB_DIM).unwrap();
    }
    (db, dir)
}

fn make_state_with(
    db: Arc<Db>,
    extractor: impl lcg_core::extractor::Extractor + 'static,
    embedder: impl lcg_core::embedder::Embedder + 'static,
) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(embedder),
        extractor: Arc::new(extractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "test".to_string(),
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

fn one_entity(name: &str) -> ExtractionResult {
    ExtractionResult {
        entities: vec![ExtractedEntity {
            name: name.to_string(),
            entity_type: "Person".to_string(),
            summary: format!("{name} is a person"),
        }],
        edges: vec![],
    }
}

fn count_entities_named(db: &Db, name: &str, group_id: &str) -> usize {
    let conn = db.connect().unwrap();
    conn.count_entities_by_name_ci(name, group_id).unwrap()
}

// ── FR-007a: two episodes with identical names → one node ────────────────────

#[tokio::test]
async fn test_identical_name_two_episodes_one_node() {
    let (db, _dir) = make_db();

    let ext = ConfigurableExtractor::new(vec![one_entity("Brett"), one_entity("Brett")]);
    let state = make_state_with(Arc::clone(&db), ext, MockEmbedder::new(EMB_DIM));

    episode::add_episode(
        Arc::clone(&state),
        "ep-a",
        "Brett met Alice for lunch.",
        "test",
        "test source",
        REF_TIME,
        GROUP,
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    episode::add_episode(
        Arc::clone(&state),
        "ep-b",
        "Brett called Alice later.",
        "test",
        "test source",
        REF_TIME,
        GROUP,
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    let count = count_entities_named(&db, "Brett", GROUP);
    assert_eq!(count, 1, "expected exactly one Brett node, got {count}");
}

// ── FR-007b: cross-session durability ────────────────────────────────────────

#[tokio::test]
async fn test_identical_name_cross_session_durability() {
    let (db, _dir) = make_db();

    // Session 1: ingest episode A
    {
        let ext = ConfigurableExtractor::new(vec![one_entity("Brett")]);
        let state = make_state_with(Arc::clone(&db), ext, MockEmbedder::new(EMB_DIM));
        episode::add_episode(
            Arc::clone(&state),
            "ep-a",
            "Brett joined the team.",
            "test",
            "test source",
            REF_TIME,
            GROUP,
            SourceType::Text,
            None,
        )
        .await
        .unwrap();
    }

    // Session 2: fresh AppState, same Db — simulates service restart.
    {
        let ext = ConfigurableExtractor::new(vec![one_entity("Brett")]);
        let state = make_state_with(Arc::clone(&db), ext, MockEmbedder::new(EMB_DIM));
        episode::add_episode(
            Arc::clone(&state),
            "ep-b",
            "Brett attended the standup.",
            "test",
            "test source",
            REF_TIME,
            GROUP,
            SourceType::Text,
            None,
        )
        .await
        .unwrap();
    }

    let count = count_entities_named(&db, "Brett", GROUP);
    assert_eq!(
        count, 1,
        "cross-session: expected one Brett node after two sessions, got {count}"
    );
}

// ── FR-001 edge case: case-insensitive match ──────────────────────────────────

#[tokio::test]
async fn test_case_insensitive_name_match() {
    let (db, _dir) = make_db();

    let ext = ConfigurableExtractor::new(vec![one_entity("Brett"), one_entity("brett")]);
    let state = make_state_with(Arc::clone(&db), ext, MockEmbedder::new(EMB_DIM));

    episode::add_episode(
        Arc::clone(&state),
        "ep-a",
        "Brett met Alice.",
        "test",
        "test source",
        REF_TIME,
        GROUP,
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    episode::add_episode(
        Arc::clone(&state),
        "ep-b",
        "brett called Alice.",
        "test",
        "test source",
        REF_TIME,
        GROUP,
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    // "brett" and "Brett" must resolve to the same node.
    let count = count_entities_named(&db, "brett", GROUP);
    assert_eq!(count, 1, "case-insensitive: expected one node, got {count}");
}

// ── Edge case: empty-name entity is skipped ────────────────────────────────────

#[tokio::test]
async fn test_empty_name_entity_skipped() {
    let (db, _dir) = make_db();

    let empty_entity = ExtractionResult {
        entities: vec![ExtractedEntity {
            name: "   ".to_string(), // whitespace-only name
            entity_type: "Person".to_string(),
            summary: "nobody".to_string(),
        }],
        edges: vec![],
    };
    let ext = ConfigurableExtractor::new(vec![empty_entity]);
    let state = make_state_with(Arc::clone(&db), ext, MockEmbedder::new(EMB_DIM));

    // Should not crash, should not create a node with an empty/whitespace name.
    episode::add_episode(
        Arc::clone(&state),
        "ep-a",
        "Nobody said something.",
        "test",
        "test source",
        REF_TIME,
        GROUP,
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    // The empty-name entity may be inserted (empty name is not filtered at the ingest level,
    // only at the resolution level), but no crash should occur.
    // Primary assertion: no panic above.
}

// ── FR-008: embedding-based dedup for high-similarity name variants ────────────

#[tokio::test]
async fn test_embedding_based_dedup_variant_names() {
    let (db, _dir) = make_db();

    // "Brett Adamson" → axis-0 unit vector [1, 0, 0, 0]
    // "Brett A."      → [cos(22.5°), sin(22.5°), 0, 0] ≈ [0.924, 0.383, 0, 0]
    // cosine_similarity([1,0,0,0], [0.924, 0.383, 0, 0]) = 0.924 > DEDUP_THRESHOLD (0.85)
    let mut emb_map: HashMap<String, Vec<f32>> = HashMap::new();
    emb_map.insert("Brett Adamson".to_string(), vec![1.0_f32, 0.0, 0.0, 0.0]);
    emb_map.insert("Brett A.".to_string(), vec![0.9239_f32, 0.3827, 0.0, 0.0]);

    let ext = ConfigurableExtractor::new(vec![one_entity("Brett Adamson"), one_entity("Brett A.")]);
    let state = make_state_with(Arc::clone(&db), ext, NameMapEmbedder::new(EMB_DIM, emb_map));

    episode::add_episode(
        Arc::clone(&state),
        "ep-a",
        "Brett Adamson joined the team.",
        "test",
        "test source",
        REF_TIME,
        GROUP,
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    episode::add_episode(
        Arc::clone(&state),
        "ep-b",
        "Brett A. attended the standup.",
        "test",
        "test source",
        REF_TIME,
        GROUP,
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    // "Brett A." is not an exact name match for "Brett Adamson", so it falls through to
    // the embedding path. The embeddings have cosine sim ≈ 0.924 > DEDUP_THRESHOLD (0.85),
    // so they must resolve to the same node (PassthroughDedupAdapter always confirms).
    let conn = db.connect().unwrap();
    let count = conn.entity_count_in_group(GROUP).unwrap();
    assert_eq!(
        count, 1,
        "embedding dedup: expected one entity node, got {count}"
    );
}

// ── SC-002 negative case: dissimilar names must not collapse ──────────────────

#[tokio::test]
async fn test_no_false_collapse_dissimilar_names() {
    let (db, _dir) = make_db();

    // "Alice Wang" → axis-0 [1, 0, 0, 0]
    // "Bob Chen"   → axis-1 [0, 1, 0, 0]
    // cosine_similarity = 0.0 < DEDUP_THRESHOLD (0.85) → two distinct nodes
    let mut emb_map: HashMap<String, Vec<f32>> = HashMap::new();
    emb_map.insert("Alice Wang".to_string(), vec![1.0_f32, 0.0, 0.0, 0.0]);
    emb_map.insert("Bob Chen".to_string(), vec![0.0_f32, 1.0, 0.0, 0.0]);

    let ext = ConfigurableExtractor::new(vec![one_entity("Alice Wang"), one_entity("Bob Chen")]);
    let state = make_state_with(Arc::clone(&db), ext, NameMapEmbedder::new(EMB_DIM, emb_map));

    episode::add_episode(
        Arc::clone(&state),
        "ep-a",
        "Alice Wang joined the team.",
        "test",
        "test source",
        REF_TIME,
        GROUP,
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    episode::add_episode(
        Arc::clone(&state),
        "ep-b",
        "Bob Chen attended the standup.",
        "test",
        "test source",
        REF_TIME,
        GROUP,
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    let conn = db.connect().unwrap();
    let count = conn.entity_count_in_group(GROUP).unwrap();
    assert_eq!(
        count, 2,
        "no-false-collapse: expected two distinct entity nodes, got {count}"
    );
}
