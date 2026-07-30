// Integration tests for global (cross-batch) edge endpoint resolution (issue #209).
//
// Covers:
//   SC-001/SC-003 (User Story 1): an edge referencing an entity created in an earlier
//     ingest batch is persisted rather than silently dropped.
//   SC-002 (User Story 2): global fallback resolution stays scoped to group_id — an
//     edge in one group must never resolve against another group's same-named entity.
//   Edge case: each endpoint is resolved independently; an edge is dropped if either
//     side (batch-local or global) fails to resolve.

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
    types::{ExtractedEdge, ExtractedEntity, ExtractionResult, SourceType},
};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const EMB_DIM: usize = 4;
const REF_TIME: &str = "2026-01-01T00:00:00Z";
const GROUP_A: &str = "group-a";
const GROUP_B: &str = "group-b";

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

/// Builds an ExtractionResult from bare entity names and (source, target, fact) edge tuples.
fn batch(entities: &[&str], edges: &[(&str, &str, &str)]) -> ExtractionResult {
    ExtractionResult {
        entities: entities
            .iter()
            .map(|n| ExtractedEntity {
                name: n.to_string(),
                entity_type: "Entity".to_string(),
                summary: format!("{n} summary"),
            })
            .collect(),
        edges: edges
            .iter()
            .map(|(src, dst, fact)| ExtractedEdge {
                source_name: src.to_string(),
                target_name: dst.to_string(),
                fact: fact.to_string(),
                relation_type: None,
                valid_at: None,
                invalid_at: None,
            })
            .collect(),
    }
}

async fn run_episode(
    state: &Arc<AppState>,
    ep_name: &str,
    body: &str,
    group_id: &str,
) -> episode::AddEpisodeResult {
    episode::add_episode(
        Arc::clone(state),
        ep_name,
        body,
        "test",
        "test source",
        REF_TIME,
        group_id,
        SourceType::Text,
        None,
    )
    .await
    .unwrap()
}

// ── User Story 1 / SC-001 / SC-003: cross-batch edge endpoint resolution ──────

#[tokio::test]
async fn test_cross_batch_edge_endpoint_is_resolved_and_persisted() {
    let (db, _dir) = make_db();

    // Batch 1 creates 'Apple' with no edges. Batch 2 extracts an edge Apple -> Creator Studio
    // where only 'Creator Studio' is present in that batch's entity list — the exact scenario
    // reported in #202.
    let ext = ConfigurableExtractor::new(vec![
        batch(&["Apple"], &[]),
        batch(
            &["Creator Studio"],
            &[("Apple", "Creator Studio", "Apple owns Creator Studio")],
        ),
    ]);
    let state = make_state_with(Arc::clone(&db), ext, MockEmbedder::new(EMB_DIM));

    run_episode(&state, "ep-a", "Apple is a company.", GROUP_A).await;
    let result = run_episode(&state, "ep-b", "Apple owns Creator Studio.", GROUP_A).await;

    assert_eq!(
        result.edges_extracted, 1,
        "edge to a previously-ingested entity must survive validation, got {}",
        result.edges_extracted
    );

    let conn = db.connect().unwrap();
    assert_eq!(
        conn.count_entities_by_name_ci("Apple", GROUP_A).unwrap(),
        1,
        "resolving the edge must not create a duplicate 'Apple' entity"
    );

    let apple = conn
        .get_entity_by_name_ci("Apple", GROUP_A)
        .unwrap()
        .expect("Apple entity must exist");
    let creator_studio = conn
        .get_entity_by_name_ci("Creator Studio", GROUP_A)
        .unwrap()
        .expect("Creator Studio entity must exist");

    let rels = conn.list_relationships(Some(&[GROUP_A]), 10).unwrap();
    assert_eq!(rels.len(), 1, "expected exactly one persisted edge");
    assert_eq!(rels[0].source_node_uuid, apple.uuid);
    assert_eq!(rels[0].target_node_uuid, creator_studio.uuid);
}

// ── User Story 2 / FR-003 / SC-002: group isolation during global fallback ────

#[tokio::test]
async fn test_global_fallback_resolution_is_scoped_to_group_id() {
    let (db, _dir) = make_db();

    // 'Apple' is created independently under group A and group B. A later batch under group A
    // extracts an edge referencing 'Apple' (absent from that batch) — it must resolve only to
    // group A's 'Apple', never group B's.
    let ext = ConfigurableExtractor::new(vec![
        batch(&["Apple"], &[]),
        batch(&["Apple"], &[]),
        batch(&["X"], &[("Apple", "X", "Apple relates to X")]),
    ]);
    let state = make_state_with(Arc::clone(&db), ext, MockEmbedder::new(EMB_DIM));

    run_episode(&state, "ep-a", "Apple exists in group A.", GROUP_A).await;
    run_episode(&state, "ep-b", "Apple exists in group B.", GROUP_B).await;
    let result = run_episode(&state, "ep-c", "Apple relates to X.", GROUP_A).await;

    assert_eq!(
        result.edges_extracted, 1,
        "edge must resolve via group A's own 'Apple', got {}",
        result.edges_extracted
    );

    let conn = db.connect().unwrap();
    let apple_a = conn
        .get_entity_by_name_ci("Apple", GROUP_A)
        .unwrap()
        .expect("group A's Apple must exist");
    let apple_b = conn
        .get_entity_by_name_ci("Apple", GROUP_B)
        .unwrap()
        .expect("group B's Apple must exist");
    assert_ne!(
        apple_a.uuid, apple_b.uuid,
        "the two groups must have distinct Apple entities"
    );

    let rels_a = conn.list_relationships(Some(&[GROUP_A]), 10).unwrap();
    assert_eq!(rels_a.len(), 1, "expected one edge in group A");
    assert_eq!(rels_a[0].source_node_uuid, apple_a.uuid);

    let rels_b = conn.list_relationships(Some(&[GROUP_B]), 10).unwrap();
    assert!(
        rels_b.is_empty(),
        "group B must not gain an edge as a side effect of group A's resolution"
    );
}

// ── Edge case: endpoints resolve independently; drop if either fails ──────────

#[tokio::test]
async fn test_edge_dropped_when_one_endpoint_unresolvable_even_if_other_resolves_globally() {
    let (db, _dir) = make_db();

    // 'Apple' is created in an earlier batch (resolves via global fallback), but
    // 'Nonexistent' is absent from both the batch and the persisted graph — the edge must
    // still be dropped, per FR-004, even though one side resolved successfully.
    let ext = ConfigurableExtractor::new(vec![
        batch(&["Apple"], &[]),
        batch(
            &["Y"],
            &[("Apple", "Nonexistent", "Apple relates to Nonexistent")],
        ),
    ]);
    let state = make_state_with(Arc::clone(&db), ext, MockEmbedder::new(EMB_DIM));

    run_episode(&state, "ep-a", "Apple is a company.", GROUP_A).await;
    let result = run_episode(&state, "ep-b", "Apple relates to Nonexistent.", GROUP_A).await;

    assert_eq!(
        result.edges_extracted, 0,
        "edge with one genuinely-unresolvable endpoint must be dropped, got {}",
        result.edges_extracted
    );

    let conn = db.connect().unwrap();
    let rels = conn.list_relationships(Some(&[GROUP_A]), 10).unwrap();
    assert!(
        rels.is_empty(),
        "no relationship should be persisted when an endpoint is unresolvable"
    );
}

// ── User Story 3 / FR-003 / SC-003: salvage an off-list endpoint by name-embedding
//    similarity against the batch's own entities, instead of dropping it ────────

#[tokio::test]
async fn test_off_list_endpoint_salvaged_via_name_embedding_similarity() {
    let (db, _dir) = make_db();

    // 'Global Warming' is not in this batch's entity list, but its name embedding is
    // deliberately made identical to 'Climate Change' (a batch entity) — well above
    // DEDUP_THRESHOLD — so the salvage step must rewrite the edge to point at it instead of
    // dropping it.
    let mut map = HashMap::new();
    map.insert("Climate Change".to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    map.insert("Ocean Acidification".to_string(), vec![0.0, 1.0, 0.0, 0.0]);
    map.insert("Global Warming".to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    let embedder = NameMapEmbedder::new(EMB_DIM, map);

    let ext = ConfigurableExtractor::new(vec![batch(
        &["Climate Change", "Ocean Acidification"],
        &[(
            "Global Warming",
            "Ocean Acidification",
            "Global warming causes ocean acidification",
        )],
    )]);
    let state = make_state_with(Arc::clone(&db), ext, embedder);

    let result = run_episode(
        &state,
        "ep-a",
        "Global warming causes ocean acidification.",
        GROUP_A,
    )
    .await;

    assert_eq!(
        result.edges_extracted, 1,
        "salvaged edge must be inserted, not dropped, got {}",
        result.edges_extracted
    );
    assert_eq!(
        result.edges_dropped_unresolvable, 0,
        "a successfully salvaged edge must not be counted as dropped"
    );

    let conn = db.connect().unwrap();
    assert!(
        conn.get_entity_by_name_ci("Global Warming", GROUP_A)
            .unwrap()
            .is_none(),
        "salvage must not create a new entity for the off-list name"
    );
    let climate_change = conn
        .get_entity_by_name_ci("Climate Change", GROUP_A)
        .unwrap()
        .expect("Climate Change entity must exist");
    let ocean_acidification = conn
        .get_entity_by_name_ci("Ocean Acidification", GROUP_A)
        .unwrap()
        .expect("Ocean Acidification entity must exist");

    let rels = conn.list_relationships(Some(&[GROUP_A]), 10).unwrap();
    assert_eq!(rels.len(), 1, "expected exactly one persisted edge");
    assert_eq!(
        rels[0].source_node_uuid, climate_change.uuid,
        "the salvaged edge must attach to 'Climate Change', not a new entity"
    );
    assert_eq!(rels[0].target_node_uuid, ocean_acidification.uuid);
}

// ── Edge case: adversarial pairs must not cross-resolve below the similarity threshold ──

#[tokio::test]
async fn test_salvage_does_not_collapse_adversarial_pairs_below_threshold() {
    let (db, _dir) = make_db();

    // 'Carbon Dioxide' and 'Carbon Monoxide' are textually similar but semantically distinct;
    // their embeddings are deliberately orthogonal (cosine 0). The off-list endpoint 'CO' sits
    // roughly equidistant between them — similar enough to be tempting, but below
    // DEDUP_THRESHOLD (0.85) against either — so salvage must not force a match to whichever is
    // nominally closer.
    let mut map = HashMap::new();
    map.insert("Carbon Dioxide".to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    map.insert("Carbon Monoxide".to_string(), vec![0.0, 1.0, 0.0, 0.0]);
    map.insert("CO".to_string(), vec![0.5, 0.5, 0.0, 0.0]);
    let embedder = NameMapEmbedder::new(EMB_DIM, map);

    let ext = ConfigurableExtractor::new(vec![batch(
        &["Carbon Dioxide", "Carbon Monoxide"],
        &[("CO", "Carbon Dioxide", "CO is related to carbon dioxide")],
    )]);
    let state = make_state_with(Arc::clone(&db), ext, embedder);

    let result = run_episode(&state, "ep-a", "CO is related to carbon dioxide.", GROUP_A).await;

    assert_eq!(
        result.edges_extracted, 0,
        "a below-threshold near-miss must not be salvaged, got {}",
        result.edges_extracted
    );
    assert_eq!(
        result.edges_dropped_unresolvable, 1,
        "the unsalvaged edge must be counted as dropped"
    );

    let conn = db.connect().unwrap();
    let rels = conn.list_relationships(Some(&[GROUP_A]), 10).unwrap();
    assert!(
        rels.is_empty(),
        "no relationship should be persisted for a below-threshold adversarial pair"
    );
}

// ── FR-004: a genuinely-unresolvable edge is dropped and counted, not just logged ──────

#[tokio::test]
async fn test_genuinely_unresolvable_edge_is_counted_in_result() {
    let (db, _dir) = make_db();

    // 'Nonexistent' is absent from the batch's entities, has no persisted match, and (under
    // MockEmbedder's zero vectors) no salvage match either — it must be dropped and counted.
    let ext = ConfigurableExtractor::new(vec![batch(
        &["Alice"],
        &[("Alice", "Nonexistent", "Alice relates to Nonexistent")],
    )]);
    let state = make_state_with(Arc::clone(&db), ext, MockEmbedder::new(EMB_DIM));

    let result = run_episode(&state, "ep-a", "Alice relates to Nonexistent.", GROUP_A).await;

    assert_eq!(result.edges_extracted, 0);
    assert_eq!(
        result.edges_dropped_unresolvable, 1,
        "a genuinely unresolvable edge must be reported in the process_chunk result, not only on stderr"
    );
}
