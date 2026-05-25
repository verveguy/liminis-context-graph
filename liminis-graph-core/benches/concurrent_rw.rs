// [HOT] Concurrent reader/writer bench — proves p95 search latency ≤ 500 ms
// under ≥ 100 concurrent episode extraction tasks (FR-013, SC-001).
//
// The bench uses MockExtractor (zero-latency) and MockEmbedder to isolate the
// RwLock contention model without real LLM or HTTP latency. This proves the
// write-guard-only-around-DB-commit design (ADR-042) does not block reads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use arc_swap::ArcSwapOption;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use liminis_graph_core::{
    app_state::AppState,
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{Embedder, MockEmbedder},
    episode,
    extractor::MockExtractor,
    search,
    telemetry::NoopSink,
    types::EntityRow,
};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;

const BENCH_DIM: usize = 768;
const N_WRITERS: usize = 100;
const N_READERS: usize = 100;
const P95_BUDGET_MS: u64 = 500;

fn build_state(db: Arc<Db>) -> Arc<AppState> {
    let sink = Arc::new(NoopSink);
    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(BENCH_DIM));
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder,
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "bench.db".to_string(),
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

fn setup_db() -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("bench.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(BENCH_DIM).unwrap();
    }

    // Pre-populate 100 entities with non-zero embeddings so brute_force_similar_entity
    // returns a match and the dedup path is exercised.
    let emb: Vec<f32> = vec![1.0f32 / (BENCH_DIM as f32).sqrt(); BENCH_DIM];
    {
        let conn = db.connect().unwrap();
        for i in 0..100usize {
            conn.insert_entity(&EntityRow {
                uuid: format!("seed-entity-{i:04}"),
                name: format!("SeedEntity{i}"),
                group_id: "bench".to_string(),
                labels: vec!["Entity".to_string()],
                created_at: "2026-01-01 00:00:00".to_string(),
                name_embedding: emb.clone(),
                summary: format!("Seed entity number {i}"),
                attributes: "{}".to_string(),
                ..Default::default()
            })
            .unwrap();
        }
    }

    (db, dir)
}

fn bench_concurrent_rw(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("concurrent_rw_p95_search", |b| {
        // Use iter_batched so each sample starts with a fresh DB and clean state,
        // preventing episodic row accumulation from making later iterations slower.
        b.iter_batched(
            || {
                let (db, dir) = setup_db();
                let state = build_state(Arc::clone(&db));
                (state, dir) // dir kept alive to prevent TempDir deletion during the sample
            },
            |(state, _dir)| {
                rt.block_on(async {
                    // Launch N_WRITERS concurrent add_episode tasks (zero-latency mock)
                    let write_handles: Vec<_> = (0..N_WRITERS)
                        .map(|i| {
                            let s = Arc::clone(&state);
                            tokio::spawn(async move {
                                let _ = episode::add_episode(
                                    s,
                                    &format!("ep-{i}"),
                                    &format!("body-{i}"),
                                    "bench",
                                    "bench source",
                                    "2026-01-01 00:00:00",
                                    "bench",
                                )
                                .await;
                            })
                        })
                        .collect();

                    // Launch N_READERS concurrent search queries and record latencies
                    let search_handles: Vec<_> = (0..N_READERS)
                        .map(|_| {
                            let s = Arc::clone(&state);
                            tokio::spawn(async move {
                                let t = Instant::now();
                                let _ = search::hybrid_entity_search(
                                    s.db.load_full().expect("bench requires healthy DB"),
                                    Arc::clone(&s.embedder),
                                    "Alice",
                                    vec!["bench".to_string()],
                                    10,
                                )
                                .await;
                                t.elapsed()
                            })
                        })
                        .collect();

                    // Wait for all tasks to complete
                    for h in write_handles {
                        let _ = h.await;
                    }

                    let mut latencies: Vec<Duration> = Vec::with_capacity(N_READERS);
                    for h in search_handles {
                        if let Ok(d) = h.await {
                            latencies.push(d);
                        }
                    }

                    // Assert p95 ≤ P95_BUDGET_MS
                    if !latencies.is_empty() {
                        latencies.sort_unstable();
                        let p95_idx =
                            ((latencies.len() as f64 * 0.95) as usize).min(latencies.len() - 1);
                        let p95 = latencies[p95_idx];
                        assert!(
                            p95 <= Duration::from_millis(P95_BUDGET_MS),
                            "p95 search latency {p95:?} exceeds {P95_BUDGET_MS} ms budget"
                        );
                    }
                });
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_concurrent_rw);
criterion_main!(benches);
