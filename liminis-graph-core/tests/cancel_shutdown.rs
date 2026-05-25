// In-process cancellation tests for the fast clean shutdown feature (issue #78).
//
// Tests verify that add_episode responds to CancellationToken at phase boundaries:
//   - cancelled during Phase A (HTTP call) → Err(Error::Cancelled) within ~500ms
//   - cancelled before add_episode begins → immediate Err(Error::Cancelled)
//   - no cancellation → add_episode completes normally

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use futures::future::BoxFuture;
use liminis_graph_core::{
    app_state::AppState, db::Db, dedup_adapter::PassthroughDedupAdapter, embedder::MockEmbedder,
    episode, error::Error, extractor::MockExtractor, telemetry::NoopSink, types::ExtractionResult,
    Extractor,
};
use std::sync::atomic::AtomicBool;
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

// ── SlowExtractor ─────────────────────────────────────────────────────────────

/// Test extractor that sleeps for 60 s — dropped by select! when token is cancelled.
struct SlowExtractor;

impl Extractor for SlowExtractor {
    fn extract<'a>(
        &'a self,
        _episode_body: &'a str,
        _group_id: &'a str,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(ExtractionResult {
                entities: vec![],
                edges: vec![],
            })
        })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let count = entities.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("cancel_test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

fn make_state_with_slow_extractor(db: Arc<Db>, token: CancellationToken) -> Arc<AppState> {
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(SlowExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink: Arc::new(NoopSink),
        db_path: "test.db".to_string(),
        wal_dir: None,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: token,
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
    })
}

fn make_state_with_fast_extractor(db: Arc<Db>) -> Arc<AppState> {
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink: Arc::new(NoopSink),
        db_path: "test.db".to_string(),
        wal_dir: None,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// Cancellation during Phase A (slow HTTP call) returns Error::Cancelled within ~500 ms.
#[tokio::test]
async fn cancel_during_phase_a_returns_cancelled() {
    let (db, _dir) = make_db(4);
    let token = CancellationToken::new();
    let state = make_state_with_slow_extractor(db, token.clone());
    let cancelled_chunks = Arc::clone(&state.cancelled_chunks);

    // Spawn add_episode; it will block in SlowExtractor::extract for 60 s.
    let s = Arc::clone(&state);
    let handle = tokio::spawn(async move {
        episode::add_episode(s, "ep", "body", "src", "desc", "2026-01-01 00:00:00", "grp").await
    });

    // Give the task a moment to enter the Phase A select!, then cancel.
    tokio::time::sleep(Duration::from_millis(10)).await;
    token.cancel();

    // The task should complete quickly (< 500 ms) with Error::Cancelled.
    let result = tokio::time::timeout(Duration::from_millis(500), handle)
        .await
        .expect("add_episode should exit within 500 ms of cancellation")
        .expect("task should not panic");

    assert!(
        matches!(result, Err(Error::Cancelled)),
        "expected Error::Cancelled, got: {result:?}"
    );
    assert_eq!(
        cancelled_chunks.load(Ordering::Relaxed),
        1,
        "cancelled_chunks counter should be 1"
    );
}

/// Cancellation before add_episode begins → immediate Err(Error::Cancelled).
#[tokio::test]
async fn cancel_before_episode_returns_cancelled() {
    let (db, _dir) = make_db(4);
    let token = CancellationToken::new();
    // Cancel the token BEFORE calling add_episode.
    token.cancel();
    let state = make_state_with_slow_extractor(db, token);
    let cancelled_chunks = Arc::clone(&state.cancelled_chunks);

    let result = episode::add_episode(
        state,
        "ep",
        "body",
        "src",
        "desc",
        "2026-01-01 00:00:00",
        "grp",
    )
    .await;

    assert!(
        matches!(result, Err(Error::Cancelled)),
        "expected Error::Cancelled, got: {result:?}"
    );
    assert_eq!(
        cancelled_chunks.load(Ordering::Relaxed),
        1,
        "cancelled_chunks counter should be 1"
    );
}

/// Without cancellation, add_episode completes normally with cancelled_chunks == 0.
#[tokio::test]
async fn no_cancel_completes_normally() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_fast_extractor(db);
    let cancelled_chunks = Arc::clone(&state.cancelled_chunks);

    let result = episode::add_episode(
        state,
        "ep",
        "body",
        "src",
        "desc",
        "2026-01-01 00:00:00",
        "grp",
    )
    .await;

    assert!(result.is_ok(), "expected success, got: {result:?}");
    assert_eq!(
        cancelled_chunks.load(Ordering::Relaxed),
        0,
        "cancelled_chunks should be 0 when no cancellation"
    );
}
