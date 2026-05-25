// T014 integration tests: LlmRouter fallback, PassthroughDedupAdapter default, write serialization.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use arc_swap::ArcSwapOption;
use liminis_graph_core::{
    app_state::AppState,
    db::Db,
    dedup_adapter::{DedupAdapter, PassthroughDedupAdapter},
    embedder::MockEmbedder,
    episode,
    extractor::{AnthropicExtractor, Extractor, MockExtractor},
    llm_router::LlmRouter,
    telemetry::{CaptureSink, NoopSink, TelemetryEvent, TelemetrySink},
    types::{EntityRow, ExtractedEntity},
};
use tempfile::TempDir;
use tokio::sync::RwLock;

// ── Test 1: LlmFallback emitted exactly once per session ──────────────────────

#[tokio::test]
async fn llm_router_fallback_emitted_once_per_session() {
    let sink = Arc::new(CaptureSink::new());
    let primary = AnthropicExtractor::with_url(
        "claude-haiku-4-5-20251001".to_string(),
        "invalid-key".to_string(),
        "http://127.0.0.1:1/unreachable".to_string(),
        Arc::clone(&sink) as Arc<dyn TelemetrySink>,
    );
    let fallback = AnthropicExtractor::with_url(
        "claude-haiku-4-5-fallback".to_string(),
        "invalid-key".to_string(),
        "http://127.0.0.1:1/unreachable".to_string(),
        Arc::clone(&sink) as Arc<dyn TelemetrySink>,
    );
    let router = LlmRouter::new(
        primary,
        Some(fallback),
        Arc::clone(&sink) as Arc<dyn TelemetrySink>,
    );

    // Both calls will fail (connection refused) — we only care about the LlmFallback event count.
    let _ = router.extract("episode 1", "grp").await;
    let _ = router.extract("episode 2", "grp").await;

    let events = sink.events();
    let fallback_count = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::LlmFallback { .. }))
        .count();
    assert_eq!(
        fallback_count, 1,
        "expected exactly one LlmFallback event across two calls, got: {events:?}"
    );
}

// ── Test 2: PassthroughDedupAdapter always returns true ───────────────────────

#[tokio::test]
async fn passthrough_dedup_adapter_always_returns_true() {
    let adapter = PassthroughDedupAdapter;
    let candidate = EntityRow {
        uuid: "uuid-1".to_string(),
        name: "Alice".to_string(),
        group_id: "g".to_string(),
        labels: vec![],
        created_at: "2026-01-01 00:00:00".to_string(),
        name_embedding: vec![],
        summary: "Alice is a person".to_string(),
        attributes: "{}".to_string(),
        ..Default::default()
    };
    let incoming = ExtractedEntity {
        name: "Alice".to_string(),
        entity_type: "Person".to_string(),
        summary: "Alice is a software engineer".to_string(),
    };
    let result = adapter.is_duplicate(&candidate, &incoming).await.unwrap();
    assert!(result, "PassthroughDedupAdapter should always return true");
}

// ── Test 3: Two concurrent add_episode calls complete without error ───────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("conc_test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }
    (db, dir)
}

#[tokio::test]
async fn concurrent_add_episode_no_write_conflict() {
    let (db, _dir) = make_db(4);
    let state = Arc::new(AppState {
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
        ontology: None,
    });

    let s1 = Arc::clone(&state);
    let s2 = Arc::clone(&state);

    let h1 = tokio::spawn(async move {
        episode::add_episode(
            s1,
            "ep-a",
            "body-a",
            "src",
            "desc",
            "2026-01-01 00:00:00",
            "grp",
        )
        .await
    });
    let h2 = tokio::spawn(async move {
        episode::add_episode(
            s2,
            "ep-b",
            "body-b",
            "src",
            "desc",
            "2026-01-01 00:00:00",
            "grp",
        )
        .await
    });

    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();
    assert!(r1.is_ok(), "first add_episode failed: {:?}", r1);
    assert!(r2.is_ok(), "second add_episode failed: {:?}", r2);
}
