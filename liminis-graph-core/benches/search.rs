use criterion::{criterion_group, criterion_main, Criterion};
use liminis_graph_core::{Db, EntityRow};
use std::sync::Arc;

fn setup_bench_db(dim: usize) -> (Arc<Db>, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("bench.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        let ts = "2026-01-01 00:00:00";
        for i in 0..100usize {
            let emb: Vec<f32> = (0..dim)
                .map(|j| if j == i % dim { 1.0 } else { 0.0 })
                .collect();
            conn.insert_entity(&EntityRow {
                uuid: format!("e{i:04}"),
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
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

fn bench_hybrid_entity_search(c: &mut Criterion) {
    let dim = 8; // Use small dim for benchmark speed
    let (db, _dir) = setup_bench_db(dim);

    let query_vec: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("hybrid_entity_search_fts_fallback", |b| {
        b.iter(|| {
            // Test the sync portion (FTS + vector search without embedder)
            let conn = db.connect().unwrap();
            let _ = conn.fts_search_entities("Entity", &["bench"], 10);
            let _ = conn.vector_search_entities(&query_vec, &["bench"], 10);
        });
    });
}

fn bench_hybrid_edge_search(c: &mut Criterion) {
    let dim = 8;
    let (db, _dir) = setup_bench_db(dim);

    let query_vec: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0f32 } else { 0.0 }).collect();

    c.bench_function("hybrid_edge_search_fts_fallback", |b| {
        b.iter(|| {
            let conn = db.connect().unwrap();
            let _ = conn.fts_search_edges("fact", &["bench"], 10);
            let _ = conn.vector_search_edges(&query_vec, &["bench"], 10);
        });
    });
}

criterion_group!(benches, bench_hybrid_entity_search, bench_hybrid_edge_search);
criterion_main!(benches);
