use criterion::{criterion_group, criterion_main, Criterion};

#[path = "bench_common.rs"]
mod bench_common;
use bench_common::setup_bench_db_n;

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

criterion_group!(
    dedup_search,
    bench_dedup_brute_force_1k,
    bench_dedup_hybrid_1k,
    bench_dedup_brute_force_10k,
    bench_dedup_hybrid_10k
);
criterion_main!(dedup_search);
