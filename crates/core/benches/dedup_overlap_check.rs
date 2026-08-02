use criterion::{criterion_group, criterion_main, Criterion};

#[path = "bench_common.rs"]
mod bench_common;
use bench_common::setup_bench_db_n;

/// Runs 100 probe queries against a 1k-entity corpus with both brute-force and hybrid dedup,
/// then asserts decision overlap ≥ 95%. This is its own `[[bench]]` target (not a group sharing
/// a `criterion_main!` with other benches) so that invoking it in isolation
/// (`cargo bench --bench dedup_overlap_check`) pays only this function's setup cost, not any
/// sibling group's — see ADR-0316 for why that isolation matters.
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

criterion_group!(dedup_overlap_check, bench_dedup_overlap_check);
criterion_main!(dedup_overlap_check);
