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
                ..Default::default()
            })
            .unwrap();
        }
        conn.build_indices_and_constraints().unwrap();
    }
    (db, dir)
}

/// Measures average brute-force dedup wall-time over `samples` iterations.
/// Used as the in-CI baseline for hybrid dedup ratio assertions.
fn measure_brute_force_ns(db: &Arc<Db>, query_emb: &[f32], samples: usize) -> u128 {
    let total: u128 = (0..samples)
        .map(|_| {
            let conn = db.connect().unwrap();
            let t = std::time::Instant::now();
            let _ = conn
                .brute_force_similar_entity(query_emb, "bench", 0.85)
                .unwrap();
            t.elapsed().as_nanos()
        })
        .sum();
    total / samples as u128
}

fn bench_hybrid_entity_search(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(100, dim);

    let query_vec: Vec<f32> = (0..dim)
        .map(|i| if i == 0 { 1.0f32 } else { 0.0 })
        .collect();

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

    let query_vec: Vec<f32> = (0..dim)
        .map(|i| if i == 0 { 1.0f32 } else { 0.0 })
        .collect();

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
    let query_emb: Vec<f32> = (0..dim)
        .map(|i| if i == 0 { 1.0f32 } else { 0.0 })
        .collect();

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
    let query_emb: Vec<f32> = (0..dim)
        .map(|i| if i == 0 { 1.0f32 } else { 0.0 })
        .collect();

    // No performance-ratio assertion at 1k: with CANDIDATE_K=200 the HNSW+BM25 overhead is
    // non-trivial relative to a 1k brute-force scan. The constitution's ≤30% gate applies
    // at 50k entities (FR-003, SC-003); see bench_dedup_hybrid_10k and bench_dedup_hybrid_50k.
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
            total
        });
    });
}

fn bench_dedup_brute_force_10k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(10_000, dim);
    let query_emb: Vec<f32> = (0..dim)
        .map(|i| if i == 0 { 1.0f32 } else { 0.0 })
        .collect();

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
    let query_emb: Vec<f32> = (0..dim)
        .map(|i| if i == 0 { 1.0f32 } else { 0.0 })
        .collect();

    // No performance-ratio assertion at 10k: with CANDIDATE_K=200 the HNSW+BM25 overhead is
    // non-trivial relative to a 10k brute-force scan. The constitution's ≤30% gate applies
    // at 50k entities (FR-003, SC-003); see bench_dedup_hybrid_50k.
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
            total
        });
    });
}

fn bench_dedup_brute_force_50k(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(50_000, dim);
    let query_emb: Vec<f32> = (0..dim)
        .map(|i| if i == 0 { 1.0f32 } else { 0.0 })
        .collect();

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
    let query_emb: Vec<f32> = (0..dim)
        .map(|i| if i == 0 { 1.0f32 } else { 0.0 })
        .collect();

    // 3 samples for 50k to keep setup time reasonable.
    let brute_ns = measure_brute_force_ns(&db, &query_emb, 3);

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
            // R-007 performance gate: hybrid must be ≤ 30% of brute-force wall time.
            let rust_ns = total.as_nanos() / iters as u128;
            assert!(
                rust_ns <= brute_ns * 30 / 100,
                "hybrid dedup 50k: {}ns > 30% of Rust brute-force {}ns",
                rust_ns,
                brute_ns
            );
            total
        });
    });
}

/// Runs 100 probe queries against a 1k-entity corpus with both brute-force and hybrid dedup,
/// then asserts decision overlap ≥ 95%. Registered as a Criterion bench so it
/// integrates with the CI bench runner (`cargo bench -- dedup_overlap_check`).
///
/// The probe loop is inside the bench_function closure so it only executes when this
/// specific bench is selected — not when other benches in the same group are run.
fn bench_dedup_overlap_check(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(1000, dim);
    let n_probes = 100;

    c.bench_function("dedup_overlap_check", |b| {
        b.iter_custom(|_iters| {
            let start = std::time::Instant::now();
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

            start.elapsed()
        });
    });
}

criterion_group!(
    benches,
    bench_hybrid_entity_search,
    bench_hybrid_edge_search
);
criterion_group!(
    dedup,
    bench_dedup_brute_force_1k,
    bench_dedup_hybrid_1k,
    bench_dedup_brute_force_10k,
    bench_dedup_hybrid_10k,
    bench_dedup_overlap_check
);
criterion_group!(
    dedup_50k,
    bench_dedup_brute_force_50k,
    bench_dedup_hybrid_50k
);
criterion_main!(benches, dedup, dedup_50k);
