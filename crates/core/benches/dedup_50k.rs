use criterion::{criterion_group, criterion_main, Criterion};

#[path = "common/mod.rs"]
mod bench_common;
use bench_common::{measure_brute_force_ns, setup_bench_db_n};

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

criterion_group!(
    dedup_50k,
    bench_dedup_brute_force_50k,
    bench_dedup_hybrid_50k
);
criterion_main!(dedup_50k);
