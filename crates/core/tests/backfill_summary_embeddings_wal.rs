/// Integration tests for knowledge_backfill_summary_embeddings — issue #470, FR-005.
///
/// These tests require a real WAL writer (unlike parity tests which set wal_writer: None) to
/// verify that backfill mutations are WAL-durable, mirroring backfill_wal.rs's coverage for
/// knowledge_backfill_relation_types.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{Embedder, NameMapEmbedder},
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    telemetry::{NoopSink, TelemetrySink},
    EntityRow, WalWriter,
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

fn make_state_with_wal(
    db: Arc<Db>,
    wal_dir: &std::path::Path,
    embedder: Arc<dyn Embedder>,
) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = WalWriter::new(wal_dir, 10_000, 0).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder,
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_root: Some(wal_dir.to_path_buf()),
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writers: Arc::new(Mutex::new(
            wal_writer
                .into_iter()
                .map(|w| ("liminis".to_string(), w))
                .collect(),
        )),
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

/// Simulates a pre-#470 entity: `summary_embedding` is the migration's zero-vector placeholder
/// (`insert_entity`'s fallback for an unset field), never a real embedding — exactly what a
/// row migrated from a pre-#470 database looks like before backfill runs.
fn make_entity(name: &str, summary: &str) -> EntityRow {
    EntityRow {
        uuid: Uuid::new_v4().to_string(),
        name: name.to_string(),
        group_id: GRP.to_string(),
        labels: vec!["Entity".to_string()],
        created_at: TS.to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: summary.to_string(),
        attributes: "{}".to_string(),
        ..Default::default()
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

// ── Tests ─────────────────────────────────────────────────────────────────────

/// FR-005, SC-004: backfill counts every entity with a non-empty summary as a candidate, embeds
/// each one, and the mutation is WAL-durable.
#[tokio::test]
async fn test_backfill_summary_embeddings_counts_and_persists() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    let embedder: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(
        DIM,
        [
            ("a pump manufacturer".to_string(), vec![1.0, 0.0, 0.0, 0.0]),
            ("a box factory".to_string(), vec![0.0, 1.0, 0.0, 0.0]),
        ]
        .into_iter()
        .collect(),
    ));
    let state = make_state_with_wal(db.clone(), wal_dir.path(), embedder);

    let e1 = make_entity("widget-1", "a pump manufacturer");
    let e2 = make_entity("widget-2", "a box factory");
    let e3 = make_entity("widget-3", ""); // empty summary — not a candidate

    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&e1).unwrap();
        conn.insert_entity(&e2).unwrap();
        conn.insert_entity(&e3).unwrap();
        let seed_mutations = conn.drain_mutations();
        let mut wal_guard = state.wal_writers.lock().unwrap();
        if let Some(writer) = wal_guard.get_mut("liminis") {
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

    let before_wal = count_wal_lines(wal_dir.path());

    let result = dispatch(
        "knowledge_backfill_summary_embeddings",
        json!({ "group_id": GRP, "dry_run": false }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(
        result["backfilled"], 2,
        "must backfill the 2 entities with a non-empty summary: {result}"
    );
    assert_eq!(
        result["total_entities"], 3,
        "must count all 3 entities scanned: {result}"
    );

    let after_wal = count_wal_lines(wal_dir.path());
    assert!(
        after_wal > before_wal,
        "backfill mutations must be WAL-durable: before={before_wal}, after={after_wal}"
    );

    // The embedded vectors are now retrievable via the summary vector index — proves the
    // drop/rebuild cycle left the index usable and pointing at the real embeddings, not stale.
    let conn = db.connect().unwrap();
    let hits = conn
        .vector_search_entities_by_summary(&[1.0, 0.0, 0.0, 0.0], Some(&[GRP]), 5)
        .unwrap();
    assert!(
        hits.iter().any(|(uuid, _)| *uuid == e1.uuid),
        "e1 must be findable via its newly-backfilled summary_embedding: {hits:?}"
    );
}

/// FR-005: dry_run reports candidate counts without embedding or mutating anything.
#[tokio::test]
async fn test_backfill_summary_embeddings_dry_run_does_not_mutate() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let embedder: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, HashMap::new()));
    let state = make_state_with_wal(db.clone(), wal_dir.path(), embedder);

    let e1 = make_entity("widget-1", "a pump manufacturer");
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&e1).unwrap();
        conn.drain_mutations();
    }

    let before_wal = count_wal_lines(wal_dir.path());

    let result = dispatch(
        "knowledge_backfill_summary_embeddings",
        json!({ "group_id": GRP, "dry_run": true }),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(
        result["backfilled"], 1,
        "dry_run must still report the candidate count: {result}"
    );
    assert_eq!(result["dry_run"], true);

    let after_wal = count_wal_lines(wal_dir.path());
    assert_eq!(
        before_wal, after_wal,
        "dry_run must not write any WAL mutation"
    );
}

/// Wraps an `Embedder` with a fixed per-call delay, so a real backfill run's Phase C spans
/// enough wall-clock time for a genuinely concurrent `knowledge_find_entities` call to land while
/// `entity_summary_embedding_idx` is dropped — without this, `MockEmbedder`/`NameMapEmbedder`
/// resolve near-instantly and the race window is too narrow to hit deterministically.
struct SlowEmbedder {
    inner: Arc<dyn Embedder>,
    delay: std::time::Duration,
}

impl Embedder for SlowEmbedder {
    fn embed<'a>(
        &'a self,
        text: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<Vec<f32>, lcg_core::error::Error>> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            self.inner.embed(text).await
        })
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }
}

/// Issue #470 concurrency fix: a `knowledge_find_entities` call that lands while
/// `knowledge_backfill_summary_embeddings` has `entity_summary_embedding_idx` dropped (Phase C)
/// must auto-heal rather than hard-fail. Real concurrent timing, not a simulated state, so this
/// fails if the `indices_built` bookkeeping backfill relies on ever regresses.
#[tokio::test]
async fn test_backfill_summary_embeddings_concurrent_find_entities_does_not_hard_fail() {
    let dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    {
        let conn = db.connect().unwrap();
        conn.create_vector_indexes().unwrap();
    }

    let mut map = HashMap::new();
    map.insert("a pump manufacturer".to_string(), vec![1.0, 0.0, 0.0, 0.0]);
    let inner: Arc<dyn Embedder> = Arc::new(NameMapEmbedder::new(DIM, map));
    let embedder: Arc<dyn Embedder> = Arc::new(SlowEmbedder {
        inner,
        delay: std::time::Duration::from_millis(20),
    });
    let state = make_state_with_wal(db.clone(), wal_dir.path(), embedder);
    state
        .indices_built
        .store(true, std::sync::atomic::Ordering::Release);

    {
        let conn = db.connect().unwrap();
        // 40 candidates x 20ms/embed = ~800ms of Phase C runtime — comfortably enough for the
        // concurrent find_entities call below to land mid-window.
        for i in 0..40 {
            conn.insert_entity(&make_entity(&format!("widget-{i}"), "a pump manufacturer"))
                .unwrap();
        }
        conn.drain_mutations();
    }

    let backfill_state = Arc::clone(&state);
    let backfill_handle = tokio::spawn(async move {
        dispatch(
            "knowledge_backfill_summary_embeddings",
            json!({ "group_id": GRP, "dry_run": false }),
            backfill_state,
        )
        .await
    });

    // Give the backfill task a head start into Phase C before racing the search in.
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;

    let find_state = Arc::clone(&state);
    let find_result = dispatch_raw(
        "knowledge_find_entities",
        json!({ "query": "widget-0", "group_ids": [GRP], "num_results": 5 }),
        find_state,
    )
    .await;
    assert!(
        find_result.get("error").is_none(),
        "a concurrent search racing into the dropped summary index must auto-heal, not \
         hard-fail: {find_result}"
    );

    let backfill_result = backfill_handle.await.unwrap();
    assert_eq!(
        backfill_result["backfilled"], 40,
        "backfill itself must still complete successfully: {backfill_result}"
    );
}
