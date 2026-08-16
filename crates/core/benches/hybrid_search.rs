use criterion::{criterion_group, criterion_main, Criterion};

#[path = "common/mod.rs"]
mod bench_common;
use bench_common::setup_bench_db_n;

fn bench_hybrid_entity_search(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db_n(100, dim);

    let query_vec: Vec<f32> = (0..dim)
        .map(|i| if i == 0 { 1.0f32 } else { 0.0 })
        .collect();

    c.bench_function("hybrid_entity_search_fts_fallback", |b| {
        b.iter(|| {
            let conn = db.connect().unwrap();
            let _ = conn.fts_search_entities("Entity", Some(&["bench"]), 10);
            let _ = conn.vector_search_entities(&query_vec, Some(&["bench"]), 10);
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
            let _ = conn.fts_search_edges("fact", Some(&["bench"]), 10);
            let _ = conn.vector_search_edges(&query_vec, Some(&["bench"]), 10);
        });
    });
}

criterion_group!(
    hybrid_search,
    bench_hybrid_entity_search,
    bench_hybrid_edge_search
);
criterion_main!(hybrid_search);
