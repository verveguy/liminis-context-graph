use lcg_core::{Db, EntityRow};
use std::sync::Arc;

/// Seeds exactly `n` entities with deterministic 8-dim unit-vector embeddings and
/// builds both HNSW vector and BM25 full-text indexes.
#[allow(dead_code)]
pub fn setup_bench_db_n(n: usize, dim: usize) -> (Arc<Db>, tempfile::TempDir) {
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
#[allow(dead_code)]
pub fn measure_brute_force_ns(db: &Arc<Db>, query_emb: &[f32], samples: usize) -> u128 {
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
