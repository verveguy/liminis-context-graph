// Golden real-corpus WAL fixture: rebuild -> assert e2e harness (#217).
//
// Replays the committed real-data WAL fixture (crates/core/tests/fixtures/real_corpus_wal/)
// through the production knowledge_rebuild_from_wal IPC path with ZERO LLM/extractor/embedder
// calls, then asserts against expected_results.json (recorded at capture time from a real
// AnthropicExtractor + real OaiEmbedder ingest run). See FR-001..FR-015 in
// specs/217-golden-real-corpus-wal/spec.md.
//
// Query-time embedding uses MockEmbedder (a zero vector, dim matched to the fixture) rather
// than a live embedder, per FR-011's zero-network requirement — this makes the vector half of
// RRF-fused search deterministic-but-uninformative, while the BM25/FTS half still carries a
// real signal from the actual captured content. Golden-query assertions are therefore written
// as top-N set-membership, not exact top-1/ordering (see module docstring in
// crates/core/scripts/capture_real_corpus.py and the fixture's expected_results.json).
//
// Graph traversal (knowledge_get_entity_neighbors) and the raw relationship listing
// (knowledge_list_relationships) do not depend on embeddings at all, so those assertions are
// exact-set-equality, not tolerant set-membership.
//
// "Zero LLM/extractor/embedder calls during replay" (FR-004) is checked explicitly, not just
// assumed from wiring in Mock{Extractor,Embedder}: CountingExtractor/CountingEmbedder below
// wrap the mocks with atomic call counters, and every test asserts those counters are zero
// immediately after knowledge_rebuild_from_wal returns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use futures::future::BoxFuture;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{Embedder, MockEmbedder},
    error::Error as LcgError,
    extractor::{ExtractOptions, Extractor, MockExtractor},
    handlers,
    ipc::IpcRequest,
    telemetry::{NoopSink, TelemetrySink},
    types::ExtractionOutcome,
    wal_group,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

// ── call-counting Extractor/Embedder ─────────────────────────────────────────
//
// Wrap Mock{Extractor,Embedder} with atomic call counters so "zero LLM/embedder calls
// during replay" (FR-004) is an explicit, checked assertion rather than an assumption that
// happens to hold because the wired-in implementation is a mock. Delegates entirely to the
// Mock types for behavior; only adds counting.

struct CountingExtractor {
    inner: MockExtractor,
    calls: Arc<AtomicUsize>,
}

impl Extractor for CountingExtractor {
    fn extract<'a>(
        &'a self,
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, LcgError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.extract(opts)
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, LcgError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.classify_entities(entities, allowed_types)
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, LcgError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.classify_relations(edges, allowed_types)
    }
}

struct CountingEmbedder {
    inner: MockEmbedder,
    calls: Arc<AtomicUsize>,
}

impl Embedder for CountingEmbedder {
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, LcgError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.embed(text)
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }
}

// ── fixture ─────────────────────────────────────────────────────────────────────

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_corpus_wal")
}

fn wal_dir() -> PathBuf {
    fixture_dir().join("wal")
}

/// Recursively copies `src` (a directory) into `dst`, creating `dst` and any intermediate
/// directories as needed (`dst` may already exist, e.g. as a fresh empty `TempDir`) — mirrors
/// `crates/service/tests/common/real_corpus.rs`'s helper of the same name.
fn copy_dir_recursive(src: &Path, dst: &Path) {
    if src.is_dir() {
        std::fs::create_dir_all(dst)
            .unwrap_or_else(|e| panic!("create dir {}: {e}", dst.display()));
        for entry in
            std::fs::read_dir(src).unwrap_or_else(|e| panic!("read dir {}: {e}", src.display()))
        {
            let entry = entry.expect("read dir entry");
            copy_dir_recursive(&entry.path(), &dst.join(entry.file_name()));
        }
    } else {
        std::fs::copy(src, dst)
            .unwrap_or_else(|e| panic!("copy {} to {}: {e}", src.display(), dst.display()));
    }
}

fn expected_results() -> Value {
    let path = fixture_dir().join("expected_results.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("expected_results.json must be valid JSON")
}

// The default LIMINIS_DEDUP_HYBRID_THRESHOLD (crates/core/src/episode.rs's private
// hybrid_threshold(), not exposed publicly — this mirrors the documented default rather than
// importing it, since exporting it would be a production-code change outside this issue's scope).
const DEFAULT_HYBRID_DEDUP_THRESHOLD: i64 = 1000;

// ── harness ───────────────────────────────────────────────────────────────────

/// Call counters for the extractor/embedder wired into a `make_state` `AppState`, so tests
/// can assert "zero calls" explicitly instead of relying on the mock's inherent safety.
struct CallCounts {
    extractor: Arc<AtomicUsize>,
    embedder: Arc<AtomicUsize>,
}

/// Builds a fresh, empty lbug DB + AppState wired at a copy of the fixture's WAL that has been
/// migrated to the current post-ADR-0378 per-group layout, with call-counting
/// Extractor/Embedder wrappers so that "nothing in this test file makes an LLM, extractor, or
/// embedder network call" (FR-004, FR-011) is an explicit, asserted count rather than an
/// assumption that happens to hold because the wired-in implementation is a mock. The
/// fixture's real embeddings are already baked into the WAL from capture time; only
/// query-time embedding at test time is mocked (and counted).
///
/// The checked-in fixture at `wal_dir()` predates ADR-0378 (flat `.jsonl` files, no per-group
/// subdirectory) and must never be written into. #429: rather than pointing `wal_root` at it
/// directly (which would fail `knowledge_rebuild_from_wal`'s per-group directory resolution
/// with "No WAL files found") or physically restructuring the checked-in fixture, this copies
/// the fixture into a fresh `TempDir` and runs the same `wal_group::migrate_wal_root_if_needed`
/// upgrade a real binary would run at startup against a legacy workspace — mirroring
/// `wal_root_migration.rs`'s existing precedent — so the suite continues to exercise the real
/// migration path rather than sidestepping it.
fn make_state(dim: usize) -> (Arc<AppState>, TempDir, TempDir, CallCounts) {
    let db_dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(db_dir.path().join("real_corpus.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        // Deliberately no create_vector_indexes() here: knowledge_rebuild_from_wal drops and
        // rebuilds all indexes (FTS + HNSW) itself at the end of a non-dry-run replay, so
        // pre-creating them here would just be redundant work this harness doesn't need.
    }

    let wal_dir_tempdir = TempDir::new().unwrap();
    let wal_root = wal_dir_tempdir.path().to_path_buf();
    copy_dir_recursive(&wal_dir(), &wal_root);
    wal_group::migrate_wal_root_if_needed(&wal_root)
        .expect("migrate the copied fixture WAL to the per-group layout");

    let extractor_calls = Arc::new(AtomicUsize::new(0));
    let embedder_calls = Arc::new(AtomicUsize::new(0));

    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(CountingEmbedder {
            inner: MockEmbedder::new(dim),
            calls: Arc::clone(&embedder_calls),
        }),
        extractor: Arc::new(CountingExtractor {
            inner: MockExtractor,
            calls: Arc::clone(&extractor_calls),
        }),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_dir.path().join("real_corpus.db").display().to_string(),
        wal_root: Some(wal_root),
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
    });
    (
        state,
        db_dir,
        wal_dir_tempdir,
        CallCounts {
            extractor: extractor_calls,
            embedder: embedder_calls,
        },
    )
}

fn req(id: i64, method: &str, params: Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    }
}

async fn dispatch(id: i64, method: &str, params: Value, state: &Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), Arc::clone(state), None).await;
    let v = serde_json::to_value(resp).unwrap();
    assert!(v.get("error").is_none(), "{method} returned an error: {v}");
    v["result"].clone()
}

/// Rebuilds the fixture WAL into `state`'s (fresh, empty) DB via the production
/// knowledge_rebuild_from_wal path — zero LLM/extractor/embedder calls (FR-004).
///
/// A `progress_tx` must be supplied to get the synchronous path: with `progress_tx: None`,
/// `handle_rebuild_from_wal` (handlers.rs) always spawns a background job and returns
/// immediately with `{"success": true, "job_id": ..., "status": "running"}`, regardless of
/// `dry_run` — there is no separate "blocking, no progress" mode. Progress messages are
/// drained and discarded; this harness only needs the final result.
async fn rebuild(state: &Arc<AppState>) -> Value {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let resp = handlers::dispatch(
        req(1, "knowledge_rebuild_from_wal", json!({})),
        Arc::clone(state),
        Some(tx),
    )
    .await;
    let v = serde_json::to_value(resp).unwrap();
    assert!(
        v.get("error").is_none(),
        "knowledge_rebuild_from_wal returned an error: {v}"
    );
    v["result"].clone()
}

fn as_str_vec(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("expected {key} to be an array: {v}"))
        .iter()
        .map(|n| {
            n["name"]
                .as_str()
                .unwrap_or_else(|| panic!("expected {key} element to have a string `name`: {n}"))
                .to_string()
        })
        .collect()
}

fn as_uuid_set(v: &Value, key: &str) -> std::collections::BTreeSet<String> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("expected {key} to be an array: {v}"))
        .iter()
        .map(|n| {
            n["uuid"]
                .as_str()
                .unwrap_or_else(|| panic!("expected {key} element to have a string `uuid`: {n}"))
                .to_string()
        })
        .collect()
}

// ── Tasks 7-11: one rebuild, all non-determinism assertions ──────────────────
//
// Each of FR-004..FR-009 is its own Acceptance Scenario in the spec, but none of them
// requires an *independent* rebuild from the others — only Task 12 (determinism) genuinely
// needs two separate rebuilds by design. A full replay + HNSW/FTS index build over this
// fixture takes roughly a minute (see real_corpus_wal/README.md for the measured figure), so
// consolidating these five scenarios into one rebuilt graph instead of five independent ones
// cuts this file's CI contribution by roughly 5x with no loss of coverage (SC-005).

// #[ignore]: this rebuild + HNSW/FTS index build over the fixture's 1,506 entities takes
// roughly a minute, and this file runs three such rebuilds total — measured at 190-240s
// (~3-4 min) for the whole file (see real_corpus_wal/README.md, SC-005). Running that on
// every PR would add ~5.7 min to the existing ~16-17 min `cargo test --release` CI job,
// so it's excluded from the default `cargo test --release` run and instead run explicitly:
// on push to main (see .github/workflows/real-corpus-e2e.yml) and via
// `cargo test -p lcg-core --test real_corpus_e2e --release -- --ignored` locally.
#[tokio::test]
#[ignore]
async fn rebuild_and_assert_all_non_determinism_expectations() {
    let expected = expected_results();
    let dim = expected["embedding_dim"].as_u64().unwrap() as usize;
    let (state, _db_dir, _wal_dir, calls) = make_state(dim);
    let group_id = expected["group_id"].clone();

    // Task 7 (FR-004, FR-005): rebuild with zero LLM/extractor/embedder calls; counts match.
    let rebuild_result = rebuild(&state).await;
    assert_eq!(rebuild_result["success"], true, "{rebuild_result}");
    // Explicit assertion, not an assumption: replay mechanically re-executes the WAL's
    // recorded Cypher and must never call Extractor::extract/classify_* or Embedder::embed —
    // that's the entire point of the fixture (FR-004). CountingExtractor/CountingEmbedder
    // (see above) make this a checked invariant instead of something that merely happens to
    // be true because the wired-in implementation is a mock.
    assert_eq!(
        calls.extractor.load(Ordering::SeqCst),
        0,
        "replay must never call the extractor"
    );
    assert_eq!(
        calls.embedder.load(Ordering::SeqCst),
        0,
        "replay must never call the embedder"
    );
    assert!(
        rebuild_result["mutations_replayed"].as_u64().unwrap() > 0,
        "expected a non-trivial replay: {rebuild_result}"
    );

    let status = dispatch(2, "knowledge_status", json!({}), &state).await;
    assert_eq!(
        status["entity_count"], expected["entity_count"],
        "entity_count mismatch: {status}"
    );
    assert_eq!(
        status["relationship_count"], expected["relationship_count"],
        "relationship_count mismatch: {status}"
    );
    assert_eq!(
        status["episode_count"], expected["episode_count"],
        "episode_count mismatch: {status}"
    );

    // The fixed MockExtractor stub (Alice/Acme Corp/WORKS_AT) must never appear — confirms
    // this graph came entirely from real captured data, not from a stray live extraction.
    let alice_search = dispatch(
        3,
        "knowledge_find_entities",
        json!({"query": "Alice", "group_ids": [group_id.clone()], "num_results": 50}),
        &state,
    )
    .await;
    for name in as_str_vec(&alice_search, "nodes") {
        assert_ne!(
            name, "Alice",
            "MockExtractor's fixed stub entity leaked into the graph"
        );
    }

    // Task 8 (FR-006): indices built, entity_count exceeds the hybrid-dedup threshold.
    assert_eq!(
        rebuild_result["indices_built"], true,
        "index build must succeed over the fixture's full entity count: {rebuild_result}"
    );
    assert_eq!(status["indices_built"], true, "{status}");
    let entity_count = status["entity_count"].as_i64().unwrap();
    assert!(
        entity_count > DEFAULT_HYBRID_DEDUP_THRESHOLD,
        "fixture entity_count ({entity_count}) must exceed the hybrid-dedup threshold \
         ({DEFAULT_HYBRID_DEDUP_THRESHOLD}) with margin (SC-002) — the fixture was captured \
         with --target-entities 1500 specifically to guarantee this"
    );

    // Task 9 (FR-007): golden entity + relationship queries surface expected hub data.
    let entity_queries = expected["golden_entity_queries"].as_array().unwrap();
    assert!(
        !entity_queries.is_empty(),
        "fixture must record golden entity queries"
    );
    for q in entity_queries {
        let query = q["query"].as_str().unwrap();
        let top_n = q["expected_top_n"].as_u64().unwrap();
        let result = dispatch(
            10,
            "knowledge_find_entities",
            json!({"query": query, "group_ids": [group_id.clone()], "num_results": top_n}),
            &state,
        )
        .await;
        let actual_names: std::collections::HashSet<String> =
            as_str_vec(&result, "nodes").into_iter().collect();
        let expected_names: Vec<String> = q["expected_entity_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap_or_default().to_string())
            .collect();

        // Query-time embedding is a mocked zero vector here (FR-011), so the fused top-N
        // will not exactly reproduce capture time's real-embedder ranking — assert
        // set-membership (at least one recorded hub entity still surfaces), not exact
        // top-1/ordering (see module docstring, Edge Cases in the spec).
        let overlap = expected_names
            .iter()
            .filter(|n| actual_names.contains(*n))
            .count();
        assert!(
            overlap > 0,
            "query {query:?}: expected at least one of {expected_names:?} in actual top-{top_n} \
             {actual_names:?}"
        );
    }

    let relationship_queries = expected["golden_relationship_queries"].as_array().unwrap();
    assert!(
        !relationship_queries.is_empty(),
        "fixture must record golden relationship queries"
    );
    for q in relationship_queries {
        let query = q["query"].as_str().unwrap();
        let top_n = q["expected_top_n"].as_u64().unwrap();
        let result = dispatch(
            11,
            "knowledge_find_relationships",
            json!({"query": query, "group_ids": [group_id.clone()], "num_results": top_n}),
            &state,
        )
        .await;
        let facts = result["facts"].as_array().unwrap();
        assert!(
            !facts.is_empty(),
            "query {query:?} returned no relationships: {result}"
        );

        let actual_relation_types: std::collections::HashSet<String> = facts
            .iter()
            .filter_map(|f| f["relation_type"].as_str().map(str::to_string))
            .collect();
        let expected_relation_types: std::collections::HashSet<String> = q
            ["expected_relation_types"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect();

        // Same tolerance rationale as the entity-query assertions above: relation_type is a
        // coarser signal than exact fact/uuid identity, so it survives the MockEmbedder-induced
        // re-ranking more reliably while still confirming real relation-typed data (FR-009's
        // spirit) shows up for a real natural-language query.
        let overlap = expected_relation_types
            .intersection(&actual_relation_types)
            .count();
        assert!(
            overlap > 0,
            "query {query:?}: expected relation_type overlap between {expected_relation_types:?} \
             and actual {actual_relation_types:?}"
        );
    }

    // Task 10 (FR-008): multi-hop traversal matches the recorded hop1/hop2 node sets exactly.
    // Graph traversal (get_entity_neighbors) never touches embeddings/BM25 — it's a plain
    // structural read through the RELATES_TO shadow-node pattern (ADR-0007) — so this is
    // exact-set equality, not the query-search tolerance used above.
    let traversal = &expected["traversal"];
    assert!(!traversal.is_null(), "fixture must record a traversal path");

    let hop1 = dispatch(
        20,
        "knowledge_get_entity_neighbors",
        json!({
            "entity_uuid": traversal["start_entity_uuid"],
            "group_ids": [group_id.clone()],
            "num_results": 25,
        }),
        &state,
    )
    .await;
    let expected_hop1: std::collections::BTreeSet<String> = traversal["expected_hop1_node_uuids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        as_uuid_set(&hop1, "nodes"),
        expected_hop1,
        "hop1 node set mismatch from {}",
        traversal["start_entity_name"]
    );

    let hop2 = dispatch(
        21,
        "knowledge_get_entity_neighbors",
        json!({
            "entity_uuid": traversal["hop1_entity_uuid"],
            "group_ids": [group_id.clone()],
            "num_results": 25,
        }),
        &state,
    )
    .await;
    let expected_hop2: std::collections::BTreeSet<String> = traversal["expected_hop2_node_uuids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        as_uuid_set(&hop2, "nodes"),
        expected_hop2,
        "hop2 node set mismatch from {}",
        traversal["hop1_entity_name"]
    );

    // Task 11 (FR-009): sampled real fact/relation_type pairs survive replay unchanged.
    let samples = expected["relation_type_samples"].as_array().unwrap();
    assert!(
        !samples.is_empty(),
        "fixture must record relation_type_samples"
    );

    // list_relationships (db.rs) orders by rn.uuid DESC with no ranking/embedding involved —
    // a plain deterministic listing — so requesting a limit comfortably above the fixture's
    // total relationship_count recovers every edge, independent of any query-search tolerance.
    let total = expected["relationship_count"].as_i64().unwrap();
    let all = dispatch(
        30,
        "knowledge_list_relationships",
        json!({"group_ids": [group_id], "num_results": total + 100}),
        &state,
    )
    .await;
    let by_uuid: HashMap<String, Value> = all["facts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["uuid"].as_str().unwrap().to_string(), f.clone()))
        .collect();

    for sample in samples {
        let uuid = sample["uuid"].as_str().unwrap();
        let actual = by_uuid
            .get(uuid)
            .unwrap_or_else(|| panic!("relationship {uuid} from the fixture's recorded samples was not found after replay"));
        assert_eq!(
            actual["relation_type"], sample["relation_type"],
            "relation_type mismatch for {uuid} ({:?}): {actual}",
            sample["fact"]
        );
        assert_eq!(
            actual["fact"], sample["fact"],
            "fact text mismatch for {uuid}: {actual}"
        );
    }

    // Final explicit check (FR-004/FR-011): the extractor must never be called anywhere in
    // this test, at any point — none of the IPC methods exercised above (knowledge_status,
    // knowledge_find_entities, knowledge_find_relationships, knowledge_get_entity_neighbors,
    // knowledge_list_relationships) touch extraction, only replay and read paths. The embedder
    // count is intentionally not re-asserted here — it is expected to be > 0 by now, since the
    // golden-query assertions above legitimately call embed() (with the mock) for query-time
    // embedding.
    assert_eq!(
        calls.extractor.load(Ordering::SeqCst),
        0,
        "the extractor must never be called anywhere in this test"
    );
}

// ── Task 12: determinism (FR-010, Acceptance Scenario 6) ─────────────────────

// #[ignore]: same rationale as the primary test above — two more full rebuilds, excluded
// from the default `cargo test --release` run for the same CI-time reason.
#[tokio::test]
#[ignore]
async fn replay_is_deterministic_across_independent_processes() {
    let expected = expected_results();
    let dim = expected["embedding_dim"].as_u64().unwrap() as usize;

    let (state_a, _db_dir_a, _wal_dir_a, calls_a) = make_state(dim);
    let (state_b, _db_dir_b, _wal_dir_b, calls_b) = make_state(dim);

    let rebuild_a = rebuild(&state_a).await;
    let rebuild_b = rebuild(&state_b).await;
    assert_eq!(
        rebuild_a["mutations_replayed"], rebuild_b["mutations_replayed"],
        "two independent replays of the same fixture must replay the same number of mutations"
    );
    // Explicit assertion (FR-004), not an assumption — see the primary test's comment for why.
    for (label, counts) in [("a", &calls_a), ("b", &calls_b)] {
        assert_eq!(
            counts.extractor.load(Ordering::SeqCst),
            0,
            "replay {label} must never call the extractor"
        );
        assert_eq!(
            counts.embedder.load(Ordering::SeqCst),
            0,
            "replay {label} must never call the embedder"
        );
    }

    let status_a = dispatch(2, "knowledge_status", json!({}), &state_a).await;
    let status_b = dispatch(2, "knowledge_status", json!({}), &state_b).await;
    for key in [
        "entity_count",
        "relationship_count",
        "episode_count",
        "indices_built",
    ] {
        assert_eq!(
            status_a[key], status_b[key],
            "status.{key} differs between replays"
        );
    }

    // Traversal is a pure structural read with no ranking involved, so it's the sharpest
    // determinism signal available here — assert exact equality between the two independent
    // rebuilds, not just against the fixture (already covered by the traversal test above).
    let traversal = &expected["traversal"];
    let group_id = expected["group_id"].clone();
    let hop1_a = dispatch(
        20,
        "knowledge_get_entity_neighbors",
        json!({"entity_uuid": traversal["start_entity_uuid"], "group_ids": [group_id.clone()], "num_results": 25}),
        &state_a,
    )
    .await;
    let hop1_b = dispatch(
        20,
        "knowledge_get_entity_neighbors",
        json!({"entity_uuid": traversal["start_entity_uuid"], "group_ids": [group_id], "num_results": 25}),
        &state_b,
    )
    .await;
    assert_eq!(
        as_uuid_set(&hop1_a, "nodes"),
        as_uuid_set(&hop1_b, "nodes"),
        "traversal hop1 node set must be identical across independent rebuilds"
    );
}
