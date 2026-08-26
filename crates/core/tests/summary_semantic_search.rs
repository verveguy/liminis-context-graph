//! Integration tests for issue #470: semantic search over Entity summaries.
//!
//! Covers SC-001 (direct-assert paraphrase retrieval), SC-002 (extraction-path paraphrase
//! retrieval), SC-003 (no regression to existing name-based retrieval), SC-004 (pre-existing DB
//! opens normally and becomes retrievable via backfill — the backfill mechanism itself is
//! covered end-to-end in `backfill_summary_embeddings_wal.rs`; this file adds the
//! `knowledge_find_entities` round-trip proof), and the spec's Edge Cases (empty summary skips
//! the embedder entirely; a partially-completed backfill doesn't error).
//!
//! Uses `NameMapEmbedder` throughout: since it's a pure string->vector lookup (not a real
//! semantic model), "paraphrase" here means two different, non-overlapping strings deliberately
//! mapped to the same vector — simulating what a real embedder would do for a genuine paraphrase,
//! while keeping the test fully deterministic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{CountingEmbedder, Embedder, MockEmbedder, NameMapEmbedder},
    extractor::{ConfigurableExtractor, Extractor},
    handlers,
    ipc::IpcRequest,
    telemetry::NoopSink,
    types::{ExtractedEntity, ExtractionResult},
    EntityRow,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const DIM: usize = 4;
const GRP: &str = "test-group";

// ── Helpers ───────────────────────────────────────────────────────────────────

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
    extractor: Arc<dyn Extractor>,
) -> Arc<AppState> {
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder,
        extractor,
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

async fn dispatch_raw(method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

async fn dispatch(method: &str, params: Value, state: Arc<AppState>) -> Value {
    let v = dispatch_raw(method, params, state).await;
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

// ── SC-001: direct-assert entity retrievable by summary paraphrase ────────────

#[tokio::test]
async fn sc001_direct_assert_entity_retrievable_by_summary_paraphrase() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let summary = "a hydraulic pump manufactured in Ohio";
    let paraphrase = "industrial machinery producing pressurized fluid flow";
    let mut map = HashMap::new();
    map.insert(summary.to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    // Deliberately the SAME vector for a completely different string — simulating a real
    // embedder recognizing these as semantically equivalent despite zero shared vocabulary.
    map.insert(paraphrase.to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    let embedder: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, map));
    let state = make_state(db, embedder, Arc::new(ConfigurableExtractor::new(vec![])));

    let assert_result = dispatch(
        "knowledge_assert_entity",
        json!({
            "name": "widget-42",
            "summary": summary,
            "group_id": GRP,
        }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(assert_result["created"], true);

    // Query with the paraphrase: shares no vocabulary with either "widget-42" (the name) or the
    // summary text itself, so neither FTS nor the name-vector can find it — only the summary
    // vector this issue adds can.
    let find_result = dispatch(
        "knowledge_find_entities",
        json!({ "query": paraphrase, "group_ids": [GRP], "num_results": 5 }),
        Arc::clone(&state),
    )
    .await;
    let names = find_entities_names(&find_result["nodes"]);
    assert!(
        names.contains(&"widget-42".to_string()),
        "entity must be retrievable by a paraphrase of its summary sharing no vocabulary with \
         it: {names:?}"
    );
}

// ── SC-002: extraction-created entity retrievable by summary paraphrase ───────

#[tokio::test]
async fn sc002_extraction_created_entity_retrievable_by_summary_paraphrase() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let summary = "a decentralized ledger for recording transactions";
    let paraphrase = "distributed database tracking financial exchanges";
    let mut map = HashMap::new();
    map.insert(summary.to_string(), vec![0.0, 1.0, 0.0, 0.0]);
    map.insert(paraphrase.to_string(), vec![0.0, 1.0, 0.0, 0.0]);
    let embedder: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, map));

    let extractor = ConfigurableExtractor::new(vec![ExtractionResult {
        entities: vec![ExtractedEntity {
            name: "proj-x9".to_string(),
            entity_type: "Entity".to_string(),
            summary: summary.to_string(),
            original_entity_type: None,
        }],
        edges: vec![],
    }]);
    let state = make_state(db, embedder, Arc::new(extractor));

    let add_result = dispatch(
        "knowledge_add_episode",
        json!({
            "name": "episode-1",
            "episode_body": "irrelevant body text — MockExtractor-style test extractor ignores it",
            "source": "text",
            "reference_time": "2026-01-01T00:00:00Z",
            "group_id": GRP,
        }),
        Arc::clone(&state),
    )
    .await;
    assert!(add_result["episode_uuid"].as_str().is_some());

    let find_result = dispatch(
        "knowledge_find_entities",
        json!({ "query": paraphrase, "group_ids": [GRP], "num_results": 5 }),
        Arc::clone(&state),
    )
    .await;
    let names = find_entities_names(&find_result["nodes"]);
    assert!(
        names.contains(&"proj-x9".to_string()),
        "extraction-created entity must be retrievable by summary paraphrase on equal footing \
         with a directly-asserted one: {names:?}"
    );
}

// ── SC-003: existing name-based retrieval is not regressed ────────────────────

#[tokio::test]
async fn sc003_name_based_query_order_unregressed_by_summary_vector() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    // Alice's name embeds to exactly the query vector (dominant BM25 + name-vector match at
    // rank 0 in both lists). Bob shares no vocabulary and maps to an unrelated name vector.
    // Both have an EMPTY summary, so both get the identical zero-vector summary_embedding — the
    // third (summary-vector) list is a pure, uninformative tie between them, contributing at
    // most a single rank's worth of RRF score (~0.0003) to whichever ties first, versus Alice's
    // full two-list rank-0 dominance (~0.033) — nowhere near enough to flip the order.
    let mut map = HashMap::new();
    map.insert("Alice".to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    map.insert("Bob".to_string(), vec![0.0, 1.0, 0.0, 0.0]);
    let embedder: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, map));
    let state = make_state(
        db.clone(),
        embedder,
        Arc::new(ConfigurableExtractor::new(vec![])),
    );

    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "alice-uuid".to_string(),
            name: "Alice".to_string(),
            group_id: GRP.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "bob-uuid".to_string(),
            name: "Bob".to_string(),
            group_id: GRP.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:01".to_string(),
            name_embedding: vec![0.0, 1.0, 0.0, 0.0],
            summary: "".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    let find_result = dispatch(
        "knowledge_find_entities",
        json!({ "query": "Alice", "group_ids": [GRP], "num_results": 2 }),
        Arc::clone(&state),
    )
    .await;
    let names = find_entities_names(&find_result["nodes"]);
    assert_eq!(
        names.first().map(String::as_str),
        Some("Alice"),
        "an exact name+vector match at rank 0 in both existing lists must still rank first, \
         unperturbed by the third (tied, uninformative) summary-vector list: {names:?}"
    );
}

// ── SC-004: pre-existing DB opens normally; backfill makes it retrievable ─────

#[tokio::test]
async fn sc004_preexisting_entity_becomes_retrievable_via_find_entities_after_backfill() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let summary = "a vintage typewriter restoration service";
    let paraphrase = "antique writing machine repair business";
    let mut map = HashMap::new();
    map.insert(summary.to_string(), vec![0.0, 0.0, 1.0, 0.0]);
    map.insert(paraphrase.to_string(), vec![0.0, 0.0, 1.0, 0.0]);
    let embedder: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, map));
    let state = make_state(
        db.clone(),
        embedder,
        Arc::new(ConfigurableExtractor::new(vec![])),
    );

    // Simulates a pre-#470 entity: inserted with insert_entity's zero-vector summary_embedding
    // fallback (never embedded), exactly what a migrated pre-existing row looks like.
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "old-shop-uuid".to_string(),
            name: "shop-77".to_string(),
            group_id: GRP.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0, 0.0, 0.0, 1.0],
            summary: summary.to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        // A dominant decoy that genuinely matches the paraphrase both lexically (identical
        // summary text) and by vector, so it wins the sole num_results=1 slot unless shop-77 is
        // also a real contender. With only one entity in the group, HNSW's unthresholded
        // top-K would trivially return it regardless of true relevance — the decoy is what
        // makes "shop-77 is absent from the top-1 result" a meaningful (not vacuous) assertion.
        conn.insert_entity(&EntityRow {
            uuid: "decoy-uuid".to_string(),
            name: "decoy-unrelated".to_string(),
            group_id: GRP.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:01".to_string(),
            name_embedding: vec![0.0, 0.0, 1.0, 0.0],
            summary: paraphrase.to_string(),
            attributes: "{}".to_string(),
            summary_embedding: vec![0.0, 0.0, 1.0, 0.0],
            ..Default::default()
        })
        .unwrap();
    }

    // Before backfill: with only 1 result slot, the dominant decoy must win it — shop-77 must
    // not be competitive yet, proving the zero-vector placeholder genuinely contributes nothing
    // (not that this test is accidentally passing for free).
    let before = dispatch(
        "knowledge_find_entities",
        json!({ "query": paraphrase, "group_ids": [GRP], "num_results": 1 }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(
        find_entities_names(&before["nodes"]),
        vec!["decoy-unrelated".to_string()],
        "precondition: a not-yet-backfilled entity must not be paraphrase-retrievable yet"
    );

    let backfill_result = dispatch(
        "knowledge_backfill_summary_embeddings",
        json!({ "group_id": GRP, "dry_run": false }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(backfill_result["backfilled"], 2);

    let after = dispatch(
        "knowledge_find_entities",
        json!({ "query": paraphrase, "group_ids": [GRP], "num_results": 2 }),
        Arc::clone(&state),
    )
    .await;
    assert!(
        find_entities_names(&after["nodes"]).contains(&"shop-77".to_string()),
        "a pre-existing entity must become paraphrase-retrievable after the documented backfill \
         path runs, the same way a newly-created entity would be: {:?}",
        after["nodes"]
    );
}

// ── Edge cases ──────────────────────────────────────────────────────────────

/// Edge Cases: an entity with an empty-string summary never triggers an embedder round-trip for
/// the summary — it goes straight to the zero-vector sentinel, matching name/lexical-only
/// fallback behavior, unaffected by this change.
#[tokio::test]
async fn empty_summary_entity_skips_embedder_call_for_summary() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let inner: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, HashMap::new()));
    let counting = Arc::new(CountingEmbedder::new(inner));
    let embedder: Arc<dyn Embedder> = counting.clone();
    let state = make_state(db, embedder, Arc::new(ConfigurableExtractor::new(vec![])));

    let assert_result = dispatch(
        "knowledge_assert_entity",
        json!({
            "name": "slug-empty-summary",
            "summary": "",
            "group_id": GRP,
        }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(assert_result["created"], true);

    // Exactly one embed() call — for `name` only. A second call (for the empty summary) would
    // mean the empty-summary skip regressed.
    assert_eq!(
        counting.call_count(),
        1,
        "an empty summary must not trigger a second embedder call"
    );
}

/// Issue #445, FR-009/SC-004: Phase C issues one batch embed call per `WRITE_BATCH_SIZE`
/// (100) candidates, not one call per candidate. 101 candidates should issue ceil(101/100) = 2
/// batch calls, and — since Phase C is the only embedding done by a non-dry-run backfill — zero
/// single-item `embed()` calls.
#[tokio::test]
async fn backfill_batches_embed_calls_by_write_chunk() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let inner: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(DIM));
    let counting = Arc::new(CountingEmbedder::new(inner));
    let embedder: Arc<dyn Embedder> = counting.clone();
    let state = make_state(
        db.clone(),
        embedder,
        Arc::new(ConfigurableExtractor::new(vec![])),
    );

    const N: usize = 101;
    {
        let conn = db.connect().unwrap();
        for i in 0..N {
            conn.insert_entity(&EntityRow {
                uuid: format!("uuid-{i}"),
                name: format!("entity-{i}"),
                group_id: GRP.to_string(),
                labels: vec!["Entity".to_string()],
                created_at: "2026-01-01 00:00:00".to_string(),
                name_embedding: vec![0.0; DIM],
                summary: format!("summary text {i}"),
                attributes: "{}".to_string(),
                ..Default::default()
            })
            .unwrap();
        }
    }

    let backfill_result = dispatch(
        "knowledge_backfill_summary_embeddings",
        json!({ "group_id": GRP, "dry_run": false }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(backfill_result["backfilled"], N);

    assert_eq!(
        counting.batch_call_count(),
        2,
        "101 candidates at WRITE_BATCH_SIZE=100 should issue ceil(101/100) = 2 batch embed calls"
    );
    assert_eq!(
        counting.call_count(),
        0,
        "backfill's Phase C should exclusively use embed_batch, never single-item embed()"
    );
}

/// Acceptance Scenario 3: a `dry_run` invocation never reaches Phase C, so it issues zero batch
/// embed calls — batching must not change that.
#[tokio::test]
async fn backfill_dry_run_issues_zero_batch_calls() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let inner: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(DIM));
    let counting = Arc::new(CountingEmbedder::new(inner));
    let embedder: Arc<dyn Embedder> = counting.clone();
    let state = make_state(
        db.clone(),
        embedder,
        Arc::new(ConfigurableExtractor::new(vec![])),
    );

    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "dry-run-uuid".to_string(),
            name: "dry-run-entity".to_string(),
            group_id: GRP.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0; DIM],
            summary: "a summary that should not be embedded".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    let backfill_result = dispatch(
        "knowledge_backfill_summary_embeddings",
        json!({ "group_id": GRP, "dry_run": true }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(backfill_result["backfilled"], 1);
    assert_eq!(counting.batch_call_count(), 0);
    assert_eq!(counting.call_count(), 0);
}

/// Edge Cases: a partially-completed backfill (some entities embedded, others not yet) must not
/// be an error condition or block queries — not-yet-processed entities simply retrieve via
/// existing name/lexical behavior only, exactly as they do today.
#[tokio::test]
async fn partial_backfill_does_not_error_and_unprocessed_entities_still_queryable_by_name() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let mut map = HashMap::new();
    map.insert("Backfilled".to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    map.insert("NotYetBackfilled".to_string(), vec![0.0, 1.0, 0.0, 0.0]);
    let embedder: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, map));
    let state = make_state(
        db.clone(),
        embedder,
        Arc::new(ConfigurableExtractor::new(vec![])),
    );

    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "backfilled-uuid".to_string(),
            name: "Backfilled".to_string(),
            group_id: GRP.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "a summary that was already embedded".to_string(),
            attributes: "{}".to_string(),
            summary_embedding: vec![0.5, 0.5, 0.0, 0.0],
            ..Default::default()
        })
        .unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "pending-uuid".to_string(),
            name: "NotYetBackfilled".to_string(),
            group_id: GRP.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:01".to_string(),
            name_embedding: vec![0.0, 1.0, 0.0, 0.0],
            summary: "a summary not yet embedded".to_string(),
            attributes: "{}".to_string(),
            // Left as the migration's zero-vector placeholder — simulates "not yet backfilled".
            ..Default::default()
        })
        .unwrap();
    }

    // A name-based query for the not-yet-backfilled entity must still succeed via name/lexical
    // matching — this must not be an error condition.
    let find_result = dispatch(
        "knowledge_find_entities",
        json!({ "query": "NotYetBackfilled", "group_ids": [GRP], "num_results": 5 }),
        Arc::clone(&state),
    )
    .await;
    let names = find_entities_names(&find_result["nodes"]);
    assert!(
        names.contains(&"NotYetBackfilled".to_string()),
        "an entity not yet backfilled must still be retrievable via existing name/lexical \
         behavior, not blocked or erroring: {names:?}"
    );
}

/// Edge Cases: re-asserting an entity with a changed summary leaves `summary_embedding` at its
/// original (write-once, now-documented-as-stale) value — see the Plan's Key Decisions for why
/// this deliberately diverges from the spec's literal Edge Cases text (an HNSW-indexed column
/// cannot be refreshed via plain SET, matching `name_embedding`'s existing precedent). Asserted
/// explicitly so this doesn't regress silently later.
#[tokio::test]
async fn reassert_with_changed_summary_leaves_summary_embedding_stale() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let original_summary = "a bakery specializing in sourdough bread";
    let updated_summary = "a completely unrelated software consultancy";
    let original_paraphrase = "artisan yeast-leavened flour goods shop";
    let updated_paraphrase = "technology advisory firm for enterprise clients";

    let mut map = HashMap::new();
    map.insert(original_summary.to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    map.insert(original_paraphrase.to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    map.insert(updated_summary.to_string(), vec![0.0, 1.0, 0.0, 0.0]);
    map.insert(updated_paraphrase.to_string(), vec![0.0, 1.0, 0.0, 0.0]);
    let embedder: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, map));
    let state = make_state(
        db.clone(),
        embedder,
        Arc::new(ConfigurableExtractor::new(vec![])),
    );

    dispatch(
        "knowledge_assert_entity",
        json!({ "name": "biz-1", "summary": original_summary, "group_id": GRP }),
        Arc::clone(&state),
    )
    .await;

    // A dominant decoy that genuinely matches the UPDATED paraphrase both lexically and by
    // vector. With only "biz-1" otherwise in the group, HNSW's unthresholded top-K would
    // trivially return it regardless of true relevance — the decoy plus num_results=1 below is
    // what makes "biz-1 is absent from the top-1 result" a meaningful, non-vacuous assertion.
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "decoy-uuid".to_string(),
            name: "decoy-unrelated".to_string(),
            group_id: GRP.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:01".to_string(),
            name_embedding: vec![0.0, 1.0, 0.0, 0.0],
            summary: updated_paraphrase.to_string(),
            attributes: "{}".to_string(),
            summary_embedding: vec![0.0, 1.0, 0.0, 0.0],
            ..Default::default()
        })
        .unwrap();
    }

    // Re-assert with a materially different summary.
    let reassert_result = dispatch(
        "knowledge_assert_entity",
        json!({ "name": "biz-1", "summary": updated_summary, "group_id": GRP }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(
        reassert_result["created"], false,
        "second assert with the same name must update, not create"
    );

    // The ORIGINAL paraphrase still finds it (summary_embedding is stale, from the first embed).
    let via_original = dispatch(
        "knowledge_find_entities",
        json!({ "query": original_paraphrase, "group_ids": [GRP], "num_results": 5 }),
        Arc::clone(&state),
    )
    .await;
    assert!(
        find_entities_names(&via_original["nodes"]).contains(&"biz-1".to_string()),
        "summary_embedding is write-once: the ORIGINAL summary's paraphrase must still match \
         after a re-assert, documenting the staleness rather than silently losing it: {:?}",
        via_original["nodes"]
    );

    // The UPDATED paraphrase does NOT find it via the vector path (summary_embedding was never
    // refreshed) — it would only be found via lexical (FTS) overlap with the literal updated
    // `summary` text, which this paraphrase deliberately shares no vocabulary with.
    let via_updated = dispatch(
        "knowledge_find_entities",
        json!({ "query": updated_paraphrase, "group_ids": [GRP], "num_results": 1 }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(
        find_entities_names(&via_updated["nodes"]),
        vec!["decoy-unrelated".to_string()],
        "the UPDATED summary's paraphrase must NOT match biz-1 via the stale summary_embedding \
         (the dominant decoy must win the sole result slot instead) — this is the documented \
         write-once staleness, not a bug: {:?}",
        via_updated["nodes"]
    );
}
