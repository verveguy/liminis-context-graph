// T014 integration tests: LlmRouter fallback, PassthroughDedupAdapter default, write serialization.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use arc_swap::ArcSwapOption;
use futures::future::BoxFuture;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::{DedupAdapter, PassthroughDedupAdapter},
    embedder::{MockEmbedder, OaiEmbedder},
    episode,
    error::Error,
    extractor::{AnthropicExtractor, ExtractOptions, Extractor, MockExtractor},
    handlers,
    ipc::IpcRequest,
    llm_router::LlmRouter,
    telemetry::{CaptureSink, NoopSink, TelemetryEvent, TelemetrySink},
    types::{EntityRow, ExtractedEntity, SourceType},
    Embedder,
};
use serde_json::{json, Value};
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
        Arc::new(primary) as Arc<dyn Extractor>,
        "claude-haiku-4-5-20251001".to_string(),
        Some(Arc::new(fallback) as Arc<dyn Extractor>),
        "claude-haiku-4-5-fallback".to_string(),
        Arc::clone(&sink) as Arc<dyn TelemetrySink>,
    );

    // Both calls will fail (connection refused) — we only care about the LlmFallback event count.
    let _ = router
        .extract(ExtractOptions {
            episode_body: "episode 1",
            group_id: "grp",
            source_type: SourceType::Text,
            custom_instructions: None,
            reference_time: "2026-01-01T00:00:00Z",
            ontology: None,
            chunk_key: None,
        })
        .await;
    let _ = router
        .extract(ExtractOptions {
            episode_body: "episode 2",
            group_id: "grp",
            source_type: SourceType::Text,
            custom_instructions: None,
            reference_time: "2026-01-01T00:00:00Z",
            ontology: None,
            chunk_key: None,
        })
        .await;

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
        original_entity_type: None,
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
            SourceType::Text,
            None,
            "",
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
            SourceType::Text,
            None,
            "",
        )
        .await
    });

    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();
    assert!(r1.is_ok(), "first add_episode failed: {:?}", r1);
    assert!(r2.is_ok(), "second add_episode failed: {:?}", r2);
}

// ── Test 4: a hung embedder doesn't hold state.write_lock indefinitely (#510) ─────

/// Builds `AppState` identically to `concurrent_add_episode_no_write_conflict`'s but with a
/// caller-supplied embedder, so this test can swap in an `OaiEmbedder` pointed at a stub that
/// never responds.
fn make_state_with_embedder(db: Arc<Db>, embedder: Arc<dyn lcg_core::Embedder>) -> Arc<AppState> {
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder,
        extractor: Arc::new(MockExtractor),
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
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
        embedding_cache: std::sync::Arc::new(lcg_core::EmbeddingCache::new()),
    })
}

/// Spawns a stub HTTP server that accepts the TCP connection and never reads or writes anything
/// — a backend that hangs while still holding its connection open (#510's core scenario).
async fn spawn_stub_http_hang_server() -> std::net::SocketAddr {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _stream = stream;
                std::future::pending::<()>().await;
            });
        }
    });

    addr
}

/// UDS counterpart of `spawn_stub_http_hang_server` (#541): accepts a connection over a Unix
/// domain socket and never reads or writes anything.
#[cfg(unix)]
async fn spawn_stub_uds_hang_server(path: &std::path::Path) -> tokio::task::JoinHandle<()> {
    use tokio::net::UnixListener;

    let listener = UnixListener::bind(path).unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _stream = stream;
                std::future::pending::<()>().await;
            });
        }
    })
}

fn ipc_req(id: i64, method: &str, params: Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    }
}

/// Acceptance Scenario 1 / SC-001: originally (#487) `handle_assert_entity`'s create path called
/// `embed()` while holding `state.write_lock`, so an embedder that accepts a connection and
/// never responds would hold that lock until the embed's configured timeout fired (#510) —
/// bounding, but not eliminating, the stall. Issue #543 removed the hold entirely: the create
/// path never holds `write_lock` across the embedder round trip at all (it re-acquires the lock
/// and re-resolves immediately before the insert instead), so a concurrent write against a
/// *different*, pre-existing entity on the same `AppState` now completes essentially immediately
/// — not merely "within the ~300ms embed timeout bound" as it did pre-#543.
///
/// This test and its UDS counterpart below
/// (`hung_uds_embedder_on_create_path_releases_write_lock_for_concurrent_write`, #541) are the
/// only two tests in this binary that set `LCG_EMBEDDING_TIMEOUT_MS`/
/// `LCG_EMBEDDING_CONNECT_TIMEOUT_MS`; `EMBEDDING_TIMEOUT_ENV_LOCK` below serializes them against
/// each other so cargo's default parallel test execution can't interleave one test's env-var
/// cleanup with the other's read of it — unlike `embedder_transport.rs`, which has many more
/// concurrent `OaiEmbedder` constructors in the same binary and needs a heavier-weight `RwLock`
/// for that reason. Cleanup is still RAII (`EnvVarGuard`, below), not a manual `remove_var` at the
/// end of the test, so a panic on any assertion in between doesn't leak the override into the
/// rest of this binary's process.
static EMBEDDING_TIMEOUT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvVarGuard {
    keys: &'static [&'static str],
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for key in self.keys {
            std::env::remove_var(key);
        }
    }
}

// Held for this whole test's duration (a local binding, not a scoped block) so the two
// env-var-setting tests in this file never interleave — see `EMBEDDING_TIMEOUT_ENV_LOCK`'s doc
// comment above.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn hung_embedder_on_create_path_releases_write_lock_for_concurrent_write() {
    let _env_lock = EMBEDDING_TIMEOUT_ENV_LOCK.lock().unwrap();
    std::env::set_var("LCG_EMBEDDING_TIMEOUT_MS", "300");
    std::env::set_var("LCG_EMBEDDING_CONNECT_TIMEOUT_MS", "300");
    let _env_guard = EnvVarGuard {
        keys: &[
            "LCG_EMBEDDING_TIMEOUT_MS",
            "LCG_EMBEDDING_CONNECT_TIMEOUT_MS",
        ],
    };

    let dim = 4;
    let (db, _dir) = make_db(dim);
    // Pre-seed an existing entity so the second, concurrent call resolves it via the
    // update path — which never touches the embedder (issue #444) — proving the lock release
    // rather than merely proving the hung embedder eventually times out on its own call.
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "existing-uuid".to_string(),
            name: "Existing Entity".to_string(),
            group_id: "grp".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0, 0.0, 0.0, 0.0],
            summary: "seed".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    let hang_addr = spawn_stub_http_hang_server().await;
    let embedder: Arc<dyn lcg_core::Embedder> = Arc::new(
        OaiEmbedder::new_http(
            format!("http://{hang_addr}/v1/embeddings"),
            "test-model",
            dim,
        )
        .expect("valid embedder config"),
    );
    let state = make_state_with_embedder(db, embedder);

    let state_create = Arc::clone(&state);
    let create_task = tokio::spawn(async move {
        handlers::dispatch(
            ipc_req(
                1,
                "knowledge_assert_entity",
                json!({
                    "name": "Brand New Entity",
                    "group_id": "grp",
                    "summary": "created while embedder is hung",
                }),
            ),
            state_create,
            None,
        )
        .await
    });

    // Give the create call a brief head start so it acquires `write_lock` first — otherwise the
    // update below could race ahead and this test would prove nothing about lock contention.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let state_update = Arc::clone(&state);
    let start_update = std::time::Instant::now();
    let update_task = tokio::spawn(async move {
        handlers::dispatch(
            ipc_req(
                2,
                "knowledge_assert_entity",
                json!({
                    "name": "Existing Entity",
                    "group_id": "grp",
                    "summary": "updated concurrently with the hung create",
                }),
            ),
            state_update,
            None,
        )
        .await
    });

    // Await the concurrent update *first* and measure its elapsed time immediately — both tasks
    // run independently in the background regardless of await order, but awaiting `create_task`
    // first (which, post-#543, spends ~600ms sequentially timing out both the name and summary
    // embeds) before measuring `update_elapsed` would fold that unrelated wait into the
    // measurement and defeat the point of this assertion, even though the update itself finished
    // long before the create call did.
    let update_resp = tokio::time::timeout(std::time::Duration::from_secs(10), update_task)
        .await
        .expect("concurrent update must not hang indefinitely — write_lock was not released")
        .unwrap();
    let update_elapsed = start_update.elapsed();

    // Neither call should ever hang indefinitely; this outer bound only guards the test itself
    // against a true regression (the whole point being disproven), not the feature's own bound.
    let create_resp = tokio::time::timeout(std::time::Duration::from_secs(10), create_task)
        .await
        .expect("create call must not hang indefinitely — write_lock was not released")
        .unwrap();

    let create_val = serde_json::to_value(create_resp).unwrap();
    assert!(
        create_val.get("result").is_some(),
        "create call should still succeed via the zero-vector embedder-unavailable fallback: \
         {create_val}"
    );
    assert_eq!(create_val["result"]["created"], true);
    assert!(
        create_val["result"]["embedding_warning"].is_string(),
        "expected embedding_warning to be populated by the timed-out embed call: {create_val}"
    );

    let update_val = serde_json::to_value(update_resp).unwrap();
    assert!(
        update_val.get("result").is_some(),
        "concurrent update call should succeed: {update_val}"
    );
    assert_eq!(update_val["result"]["created"], false);
    assert_eq!(update_val["result"]["entity_uuid"], "existing-uuid");

    // The decisive assertion (tightened by issue #543): since the create path no longer holds
    // `write_lock` across the embed call at all, the concurrent update must complete well before
    // the hung embedder's sequential name+summary timeouts (~600ms total) even finish — not
    // merely "bounded by" them, which was the best pre-#543 code could promise. 500ms leaves CI
    // scheduling headroom while still clearly separating "lock never held across embed" from the
    // pre-#543 regression this test would otherwise fail to catch (review feedback: 250ms was
    // too tight relative to CI variance).
    assert!(
        update_elapsed < std::time::Duration::from_millis(500),
        "concurrent update took {update_elapsed:?} — expected it to complete well before the \
         hung embedder's sequential ~600ms of timeouts, since write_lock is never held across \
         the embed call after issue #543"
    );
}

/// SC-003 (#541): UDS counterpart of
/// `hung_embedder_on_create_path_releases_write_lock_for_concurrent_write` — the same
/// write-lock-release guarantee must hold when the hung embedder is reached over the UDS
/// transport (the *default* deployment path, per #541's issue body), not only over HTTP as #510
/// verified.
// See `hung_embedder_on_create_path_releases_write_lock_for_concurrent_write`'s comment on
// `EMBEDDING_TIMEOUT_ENV_LOCK`: this is the second (and only other) test in this binary that sets
// these env vars, and holding the lock for the whole test serializes the two against each other.
#[allow(clippy::await_holding_lock)]
#[cfg(unix)]
#[tokio::test]
async fn hung_uds_embedder_on_create_path_releases_write_lock_for_concurrent_write() {
    let _env_lock = EMBEDDING_TIMEOUT_ENV_LOCK.lock().unwrap();
    std::env::set_var("LCG_EMBEDDING_TIMEOUT_MS", "300");
    std::env::set_var("LCG_EMBEDDING_CONNECT_TIMEOUT_MS", "300");
    let _env_guard = EnvVarGuard {
        keys: &[
            "LCG_EMBEDDING_TIMEOUT_MS",
            "LCG_EMBEDDING_CONNECT_TIMEOUT_MS",
        ],
    };

    let dim = 4;
    let (db, _dir) = make_db(dim);
    // Pre-seed an existing entity so the second, concurrent call resolves it via the update path
    // — which never touches the embedder (issue #444) — proving the lock release rather than
    // merely proving the hung embedder eventually times out on its own call.
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "existing-uuid".to_string(),
            name: "Existing Entity".to_string(),
            group_id: "grp".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0, 0.0, 0.0, 0.0],
            summary: "seed".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    let sock_dir = TempDir::new().unwrap();
    let sock_path = sock_dir.path().join("hang.sock");
    let _server = spawn_stub_uds_hang_server(&sock_path).await;
    let embedder: Arc<dyn lcg_core::Embedder> = Arc::new(
        OaiEmbedder::new_uds(sock_path.to_str().unwrap(), "test-model", dim)
            .expect("valid embedder config"),
    );
    let state = make_state_with_embedder(db, embedder);

    let state_create = Arc::clone(&state);
    let create_task = tokio::spawn(async move {
        handlers::dispatch(
            ipc_req(
                1,
                "knowledge_assert_entity",
                json!({
                    "name": "Brand New Entity",
                    "group_id": "grp",
                    "summary": "created while embedder is hung",
                }),
            ),
            state_create,
            None,
        )
        .await
    });

    // Give the create call a brief head start so it acquires `write_lock` first — otherwise the
    // update below could race ahead and this test would prove nothing about lock contention.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let state_update = Arc::clone(&state);
    let start_update = std::time::Instant::now();
    let update_task = tokio::spawn(async move {
        handlers::dispatch(
            ipc_req(
                2,
                "knowledge_assert_entity",
                json!({
                    "name": "Existing Entity",
                    "group_id": "grp",
                    "summary": "updated concurrently with the hung create",
                }),
            ),
            state_update,
            None,
        )
        .await
    });

    // Neither call should ever hang indefinitely; this outer bound only guards the test itself
    // against a true regression, not the feature's own bound.
    let create_resp = tokio::time::timeout(std::time::Duration::from_secs(10), create_task)
        .await
        .expect("create call must not hang indefinitely — write_lock was not released")
        .unwrap();
    let update_resp = tokio::time::timeout(std::time::Duration::from_secs(10), update_task)
        .await
        .expect("concurrent update must not hang indefinitely — write_lock was not released")
        .unwrap();
    let update_elapsed = start_update.elapsed();

    let create_val = serde_json::to_value(create_resp).unwrap();
    assert!(
        create_val.get("result").is_some(),
        "create call should still succeed via the zero-vector embedder-unavailable fallback: \
         {create_val}"
    );
    assert_eq!(create_val["result"]["created"], true);
    assert!(
        create_val["result"]["embedding_warning"].is_string(),
        "expected embedding_warning to be populated by the timed-out embed call: {create_val}"
    );

    let update_val = serde_json::to_value(update_resp).unwrap();
    assert!(
        update_val.get("result").is_some(),
        "concurrent update call should succeed: {update_val}"
    );
    assert_eq!(update_val["result"]["created"], false);
    assert_eq!(update_val["result"]["entity_uuid"], "existing-uuid");

    // The decisive assertion: the update, which never touches the embedder, must complete
    // shortly after the create releases the lock (~300ms embed timeout + margin) — not after
    // some unbounded wait, which is what this test (and #510's HTTP counterpart) exists to catch.
    assert!(
        update_elapsed < std::time::Duration::from_secs(5),
        "concurrent update took {update_elapsed:?} — write_lock was held far longer than the \
         ~300ms embed timeout bound, suggesting it was not released promptly"
    );
}

// ── Test 5: narrowed write_lock around the embedder round trip (issue #543) ───────

/// Test embedder whose `embed()` blocks on a shared `tokio::sync::Barrier` until exactly
/// `parties` concurrent callers have all called it — forcing a deterministic interleaving where
/// every racing caller's Pass 1 (pre-embed resolution) has definitely completed before any of
/// them proceeds to Pass 2 (post-embed re-resolution/insert), rather than relying on
/// `sleep`-based timing that could flake under load (per issue #543 Research's flagged risk).
struct BarrierEmbedder {
    dim: usize,
    barrier: Arc<tokio::sync::Barrier>,
}

impl Embedder for BarrierEmbedder {
    fn embed<'a>(&'a self, _text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        let barrier = Arc::clone(&self.barrier);
        let dim = self.dim;
        Box::pin(async move {
            barrier.wait().await;
            Ok(vec![0.0f32; dim])
        })
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

/// Test embedder whose `embed()` sleeps for a fixed, configurable delay before succeeding —
/// unlike the hung-forever stub server above, this always completes; it models a slow but
/// healthy embedder (FR-012/SC-001), not a degraded one. Signals `started` the instant it's
/// entered so a test can deterministically wait for the create call to have reached its
/// lock-free embed phase (Pass 1 already resolved and dropped the guard) instead of guessing
/// with a fixed sleep, which review feedback flagged as a source of CI flakiness under load.
struct DelayEmbedder {
    dim: usize,
    delay: Duration,
    started: Arc<tokio::sync::Notify>,
}

impl Embedder for DelayEmbedder {
    fn embed<'a>(&'a self, _text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        let dim = self.dim;
        let delay = self.delay;
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.notify_one();
            tokio::time::sleep(delay).await;
            Ok(vec![0.0f32; dim])
        })
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

/// Test embedder whose `embed()` signals `started` the instant it's entered, then blocks until
/// `proceed` is signaled — lets a test inject a concurrent write into the exact window between
/// Pass 1 dropping `write_lock` and Pass 2 re-acquiring it (issue #543's Acceptance Scenario 2,
/// third bullet).
struct PausableEmbedder {
    dim: usize,
    started: Arc<tokio::sync::Notify>,
    proceed: Arc<tokio::sync::Notify>,
}

impl Embedder for PausableEmbedder {
    fn embed<'a>(&'a self, _text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        let dim = self.dim;
        let started = Arc::clone(&self.started);
        let proceed = Arc::clone(&self.proceed);
        Box::pin(async move {
            started.notify_one();
            proceed.notified().await;
            Ok(vec![0.0f32; dim])
        })
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

/// FR-010/SC-002: two concurrent `knowledge_assert_entity` calls racing to create the identical
/// not-yet-existing `(name, group_id)` must yield exactly one created row — the loser resolves
/// via the update path against the winner's row, with no error and no duplicate. Uses
/// `BarrierEmbedder` to force both calls' embed round trips to overlap deterministically.
#[tokio::test]
async fn assert_entity_concurrent_creates_of_same_identity_yield_one_row() {
    let dim = 4;
    let (db, _dir) = make_db(dim);
    let db_check = Arc::clone(&db);

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let embedder: Arc<dyn Embedder> = Arc::new(BarrierEmbedder { dim, barrier });
    let state = make_state_with_embedder(db, embedder);

    let mut tasks = Vec::new();
    for id in 1..=2 {
        let state = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            handlers::dispatch(
                ipc_req(
                    id,
                    "knowledge_assert_entity",
                    json!({
                        "name": "Racing Entity",
                        "group_id": "grp",
                        "summary": "created by a racing caller",
                    }),
                ),
                state,
                None,
            )
            .await
        }));
    }

    let mut results = Vec::new();
    for t in tasks {
        results.push(serde_json::to_value(t.await.unwrap()).unwrap());
    }
    for r in &results {
        assert!(r.get("result").is_some(), "call should succeed: {r}");
    }

    let created_count = results
        .iter()
        .filter(|r| r["result"]["created"].as_bool().unwrap())
        .count();
    assert_eq!(
        created_count, 1,
        "exactly one of the two racing creates should report created=true: {results:?}"
    );

    let uuids: Vec<&str> = results
        .iter()
        .map(|r| r["result"]["entity_uuid"].as_str().unwrap())
        .collect();
    assert_eq!(
        uuids[0], uuids[1],
        "the losing call must resolve to the same row the winner created: {results:?}"
    );

    let conn = db_check.connect().unwrap();
    let count = conn
        .count_active_entities_by_name_ci("Racing Entity", "grp")
        .unwrap();
    assert_eq!(count, 1, "expected exactly one row, found a duplicate");
}

/// FR-011/SC-003: the edge equivalent of the entity race above — two concurrent
/// `knowledge_assert_relationship` calls racing to create the identical not-yet-existing edge
/// `(source, target, predicate, group_id)` must yield exactly one created edge.
#[tokio::test]
async fn assert_relationship_concurrent_creates_of_same_identity_yield_one_row() {
    let dim = 4;
    let (db, _dir) = make_db(dim);
    let db_check = Arc::clone(&db);
    {
        let conn = db.connect().unwrap();
        for (uuid, name) in [
            ("source-uuid", "Source Entity"),
            ("target-uuid", "Target Entity"),
        ] {
            conn.insert_entity(&EntityRow {
                uuid: uuid.to_string(),
                name: name.to_string(),
                group_id: "grp".to_string(),
                labels: vec!["Entity".to_string()],
                created_at: "2026-01-01 00:00:00".to_string(),
                name_embedding: vec![0.0; dim],
                summary: "seed".to_string(),
                attributes: "{}".to_string(),
                ..Default::default()
            })
            .unwrap();
        }
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let embedder: Arc<dyn Embedder> = Arc::new(BarrierEmbedder { dim, barrier });
    let state = make_state_with_embedder(db, embedder);

    let mut tasks = Vec::new();
    for id in 1..=2 {
        let state = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            handlers::dispatch(
                ipc_req(
                    id,
                    "knowledge_assert_relationship",
                    json!({
                        "source_name": "Source Entity",
                        "target_name": "Target Entity",
                        "predicate": "RACES_WITH",
                        "group_id": "grp",
                    }),
                ),
                state,
                None,
            )
            .await
        }));
    }

    let mut results = Vec::new();
    for t in tasks {
        results.push(serde_json::to_value(t.await.unwrap()).unwrap());
    }
    for r in &results {
        assert!(r.get("result").is_some(), "call should succeed: {r}");
    }

    let created_count = results
        .iter()
        .filter(|r| r["result"]["created"].as_bool().unwrap())
        .count();
    assert_eq!(
        created_count, 1,
        "exactly one of the two racing creates should report created=true: {results:?}"
    );

    let uuids: Vec<&str> = results
        .iter()
        .map(|r| r["result"]["edge_uuid"].as_str().unwrap())
        .collect();
    assert_eq!(
        uuids[0], uuids[1],
        "the losing call must resolve to the same edge the winner created: {results:?}"
    );

    let conn = db_check.connect().unwrap();
    let matching = conn
        .get_edges_for_entity("source-uuid")
        .unwrap()
        .into_iter()
        .filter(|e| {
            e.name == "RACES_WITH" && e.target_node_uuid == "target-uuid" && e.invalid_at.is_none()
        })
        .count();
    assert_eq!(matching, 1, "expected exactly one edge, found a duplicate");
}

/// FR-012/SC-001: a concurrent unrelated write (an update, which never touches the embedder)
/// must complete without waiting for another call's in-flight, slow-but-healthy embedder round
/// trip. Demonstrates the throughput independence directly, rather than by inference from the
/// hung-embedder test above.
#[tokio::test]
async fn assert_entity_unrelated_write_not_gated_by_slow_embedder() {
    let dim = 4;
    let (db, _dir) = make_db(dim);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "existing-uuid".to_string(),
            name: "Existing Entity".to_string(),
            group_id: "grp".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0; dim],
            summary: "seed".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let embedder: Arc<dyn Embedder> = Arc::new(DelayEmbedder {
        dim,
        delay: Duration::from_millis(500),
        started: Arc::clone(&started),
    });
    let state = make_state_with_embedder(db, embedder);

    let state_create = Arc::clone(&state);
    let create_task = tokio::spawn(async move {
        handlers::dispatch(
            ipc_req(
                1,
                "knowledge_assert_entity",
                json!({
                    "name": "Brand New Entity",
                    "group_id": "grp",
                    "summary": "",
                }),
            ),
            state_create,
            None,
        )
        .await
    });

    // Wait for the create call to have acquired (and dropped) the lock for Pass 1 and entered
    // its slow embed call, rather than guessing with a fixed sleep (review feedback: a fixed
    // delay can still race under CI load).
    started.notified().await;

    let state_update = Arc::clone(&state);
    let start_update = std::time::Instant::now();
    let update_task = tokio::spawn(async move {
        handlers::dispatch(
            ipc_req(
                2,
                "knowledge_assert_entity",
                json!({
                    "name": "Existing Entity",
                    "group_id": "grp",
                    "summary": "updated while create's embedder is slow",
                }),
            ),
            state_update,
            None,
        )
        .await
    });

    // Measure the update's own completion time first — see the matching comment in the
    // hung-embedder test above for why await order matters here.
    let update_resp = tokio::time::timeout(Duration::from_secs(10), update_task)
        .await
        .expect("update must not hang indefinitely")
        .unwrap();
    let update_elapsed = start_update.elapsed();

    let create_resp = tokio::time::timeout(Duration::from_secs(10), create_task)
        .await
        .expect("create must not hang indefinitely")
        .unwrap();

    let update_val = serde_json::to_value(update_resp).unwrap();
    assert!(
        update_val.get("result").is_some(),
        "concurrent update should succeed: {update_val}"
    );
    assert_eq!(update_val["result"]["created"], false);

    let create_val = serde_json::to_value(create_resp).unwrap();
    assert!(
        create_val.get("result").is_some(),
        "create should succeed: {create_val}"
    );
    assert_eq!(create_val["result"]["created"], true);

    // 350ms leaves CI scheduling headroom (review feedback: 200ms was too tight) while still
    // clearly separating "not gated at all" from the 500ms-embed regression this test guards
    // against.
    assert!(
        update_elapsed < Duration::from_millis(350),
        "concurrent update took {update_elapsed:?} — expected it to complete well before the \
         delayed embedder call (500ms) returns, demonstrating it is not ordered after it"
    );
}

/// Edge case from issue #543's Acceptance Scenario 2, third bullet: a create call resolves
/// "not found" and is paused mid-embed; before it re-acquires the lock, a concurrent writer
/// creates a *different* active entity under the exact name the paused call is trying to
/// create. The paused call's Pass 2 re-resolution must find that entity and take the update
/// path against it, rather than inserting a second, colliding row (FR-008).
#[tokio::test]
async fn assert_entity_during_embed_collision_takes_update_path_not_duplicate_insert() {
    let dim = 4;
    let (db, _dir) = make_db(dim);
    let db_check = Arc::clone(&db);

    let started = Arc::new(tokio::sync::Notify::new());
    let proceed = Arc::new(tokio::sync::Notify::new());
    let embedder: Arc<dyn Embedder> = Arc::new(PausableEmbedder {
        dim,
        started: Arc::clone(&started),
        proceed: Arc::clone(&proceed),
    });
    let state = make_state_with_embedder(db, embedder);

    let state_create = Arc::clone(&state);
    let create_task = tokio::spawn(async move {
        handlers::dispatch(
            ipc_req(
                1,
                "knowledge_assert_entity",
                json!({
                    "name": "Contested Name",
                    "group_id": "grp",
                    "summary": "",
                }),
            ),
            state_create,
            None,
        )
        .await
    });

    // Wait until the create call's Pass 1 has resolved "not found", dropped the lock, and
    // entered the embed call — exactly the window this edge case targets.
    started.notified().await;

    // A concurrent writer creates a *different* active entity under the exact name the paused
    // call is trying to create — simulated with a direct DB insert (rather than a second
    // `knowledge_assert_entity` call) since the point under test is the paused call's Pass 2
    // re-resolution, not how the collider itself came to exist.
    {
        let conn = db_check.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "collider-uuid".to_string(),
            name: "Contested Name".to_string(),
            group_id: "grp".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![0.0; dim],
            summary: "created by a concurrent writer during the dropped-lock window".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    proceed.notify_one();

    let create_resp = tokio::time::timeout(Duration::from_secs(10), create_task)
        .await
        .expect("create call must not hang indefinitely")
        .unwrap();
    let create_val = serde_json::to_value(create_resp).unwrap();
    assert!(
        create_val.get("result").is_some(),
        "call should succeed: {create_val}"
    );
    assert_eq!(
        create_val["result"]["created"], false,
        "Pass 2's re-resolution should find the collider and take the update path, not insert \
         a duplicate: {create_val}"
    );
    assert_eq!(create_val["result"]["entity_uuid"], "collider-uuid");

    let conn = db_check.connect().unwrap();
    let count = conn
        .count_active_entities_by_name_ci("Contested Name", "grp")
        .unwrap();
    assert_eq!(
        count, 1,
        "expected no duplicate row from the paused call's insert"
    );
}
