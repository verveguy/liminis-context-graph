// Baseline wall-time nanoseconds from benches/python_baseline_ns.json
// Measured on Apple M2 (2026-05-19) using graphiti_service.py brute-force cosine path
// against a deterministic 8-dim synthetic corpus.
const PYTHON_BASELINE_NS_1K: u128 = 2_000_000;
const PYTHON_BASELINE_NS_10K: u128 = 20_000_000;
const PYTHON_BASELINE_NS_50K: u128 = 100_000_000;

use criterion::{criterion_group, criterion_main, Criterion};
use liminis_graph_core::{Db, EntityRow};
use std::sync::Arc;

/// Seeds exactly `n` entities with deterministic 8-dim unit-vector embeddings and
/// builds both HNSW vector and BM25 full-text indexes.
fn setup_bench_db_n(n: usize, dim: usize) -> (Arc<Db>, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("bench.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        let ts = "2026-01-01 00:00:00";
        for i in 0..n {
            let axis = i % dim;
            let emb: Vec<f32> = (0..dim)
                .map(|j| if j == axis { 1.0 } else { 0.0 })
                .collect();
            conn.insert_entity(&EntityRow {
                uuid: format!("e{i:06}"),
                name: format!("Entity {i}"),
                group_id: "bench".to_string(),
                labels: vec!["Entity".to_string()],
                created_at: ts.to_string(),
                name_embedding: emb,
                summary: format!("Summary for entity {i}"),
                attributes: "{}".to_string(),
            })
            .unwrap();
        }
        conn.build_indices_and_constraints().unwrap();
    }
    (db, dir)
}

fn bench_hybrid_entity_search(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(100, dim);

    let query_vec: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("hybrid_entity_search_fts_fallback", |b| {
        b.iter(|| {
            let conn = db.connect().unwrap();
            let _ = conn.fts_search_entities("Entity", &["bench"], 10);
            let _ = conn.vector_search_entities(&query_vec, &["bench"], 10);
        });
    });
}

fn bench_hybrid_edge_search(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(100, dim);

    let query_vec: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("hybrid_edge_search_fts_fallback", |b| {
        b.iter(|| {
            let conn = db.connect().unwrap();
            let _ = conn.fts_search_edges("fact", &["bench"], 10);
            let _ = conn.vector_search_edges(&query_vec, &["bench"], 10);
        });
    });
}

fn bench_dedup_brute_force_1k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(1000, dim);
    let query_emb: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("bench_dedup_brute_force_1k", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let conn = db.connect().unwrap();
                let start = std::time::Instant::now();
                let _ = conn
                    .brute_force_similar_entity(&query_emb, "bench", 0.85)
                    .unwrap();
                total += start.elapsed();
            }
            total
        });
    });
}

fn bench_dedup_hybrid_1k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(1000, dim);
    let query_emb: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("bench_dedup_hybrid_1k", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let conn = db.connect().unwrap();
                let start = std::time::Instant::now();
                let _ = conn
                    .hybrid_dedup_similar_entity(&query_emb, "Entity 0", "bench", 0.85)
                    .unwrap();
                total += start.elapsed();
            }
            // Assert ≤ 30% of Python baseline at 1k
            let rust_ns = total.as_nanos() / iters as u128;
            assert!(
                rust_ns <= PYTHON_BASELINE_NS_1K * 30 / 100,
                "hybrid dedup 1k: {}ns > 30% of Python baseline {}ns",
                rust_ns,
                PYTHON_BASELINE_NS_1K
            );
            total
        });
    });
}

fn bench_dedup_brute_force_10k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(10_000, dim);
    let query_emb: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("bench_dedup_brute_force_10k", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let conn = db.connect().unwrap();
                let start = std::time::Instant::now();
                let _ = conn
                    .brute_force_similar_entity(&query_emb, "bench", 0.85)
                    .unwrap();
                total += start.elapsed();
            }
            total
        });
    });
}

fn bench_dedup_hybrid_10k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(10_000, dim);
    let query_emb: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("bench_dedup_hybrid_10k", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let conn = db.connect().unwrap();
                let start = std::time::Instant::now();
                let _ = conn
                    .hybrid_dedup_similar_entity(&query_emb, "Entity 0", "bench", 0.85)
                    .unwrap();
                total += start.elapsed();
            }
            // Assert ≤ 30% of Python baseline at 10k
            let rust_ns = total.as_nanos() / iters as u128;
            assert!(
                rust_ns <= PYTHON_BASELINE_NS_10K * 30 / 100,
                "hybrid dedup 10k: {}ns > 30% of Python baseline {}ns",
                rust_ns,
                PYTHON_BASELINE_NS_10K
            );
            total
        });
    });
}

fn bench_dedup_brute_force_50k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(50_000, dim);
    let query_emb: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("bench_dedup_brute_force_50k", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let conn = db.connect().unwrap();
                let start = std::time::Instant::now();
                let _ = conn
                    .brute_force_similar_entity(&query_emb, "bench", 0.85)
                    .unwrap();
                total += start.elapsed();
            }
            total
        });
    });
}

fn bench_dedup_hybrid_50k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(50_000, dim);
    let query_emb: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("bench_dedup_hybrid_50k", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let conn = db.connect().unwrap();
                let start = std::time::Instant::now();
                let _ = conn
                    .hybrid_dedup_similar_entity(&query_emb, "Entity 0", "bench", 0.85)
                    .unwrap();
                total += start.elapsed();
            }
            // Assert ≤ 30% of Python baseline at 50k (R-007 performance gate)
            let rust_ns = total.as_nanos() / iters as u128;
            assert!(
                rust_ns <= PYTHON_BASELINE_NS_50K * 30 / 100,
                "hybrid dedup 50k: {}ns > 30% of Python baseline {}ns",
                rust_ns,
                PYTHON_BASELINE_NS_50K
            );
            total
        });
    });
}

/// Runs 100 probe queries against a 1k-entity corpus with both brute-force and hybrid dedup,
/// then asserts decision overlap ≥ 95%. Registered as a non-timed Criterion bench so it
/// integrates with the CI bench runner (`cargo bench -- dedup_overlap_check`).
fn bench_dedup_overlap_check(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(1000, dim);
    let n_probes = 100;

    let conn = db.connect().unwrap();
    let mut brute_decisions: Vec<Option<String>> = Vec::with_capacity(n_probes);
    let mut hybrid_decisions: Vec<Option<String>> = Vec::with_capacity(n_probes);

    for i in 0..n_probes {
        let axis = i % dim;
        let query_emb: Vec<f32> = (0..dim)
            .map(|j| if j == axis { 1.0f32 } else { 0.0 })
            .collect();
        let query_name = format!("Entity {i}");

        let brute = conn
            .brute_force_similar_entity(&query_emb, "bench", 0.85)
            .unwrap();
        let hybrid = conn
            .hybrid_dedup_similar_entity(&query_emb, &query_name, "bench", 0.85)
            .unwrap();

        brute_decisions.push(brute.map(|e| e.uuid));
        hybrid_decisions.push(hybrid.map(|e| e.uuid));
    }

    let matching = brute_decisions
        .iter()
        .zip(hybrid_decisions.iter())
        .filter(|(b, h)| b == h)
        .count();
    let overlap = matching as f64 / n_probes as f64;
    assert!(
        overlap >= 0.95,
        "decision overlap {:.1}% < 95% required (R-003/acceptance scenario 2)",
        overlap * 100.0
    );

    // Register trivial timed bench so Criterion is satisfied
    c.bench_function("dedup_overlap_check", |b| b.iter(|| overlap));
}

criterion_group!(benches, bench_hybrid_entity_search, bench_hybrid_edge_search);
criterion_group!(
    dedup,
    bench_dedup_brute_force_1k,
    bench_dedup_hybrid_1k,
    bench_dedup_brute_force_10k,
    bench_dedup_hybrid_10k,
    bench_dedup_overlap_check
);
criterion_group!(dedup_50k, bench_dedup_brute_force_50k, bench_dedup_hybrid_50k);
criterion_main!(benches, dedup, dedup_50k);
