use liminis_graph_core::{Db, EntityRow};

fn setup_dedup_db(dim: usize) -> (Db, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("dedup.db").to_str().unwrap()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }
    (db, dir)
}

/// Inserts `n` entities into group with unit-vector embeddings along axis `i % dim`.
fn insert_test_entities(conn: &liminis_graph_core::Conn<'_>, n: usize, dim: usize, group_id: &str) {
    let ts = "2026-01-01 00:00:00";
    for i in 0..n {
        let axis = i % dim;
        let emb: Vec<f32> = (0..dim)
            .map(|j| if j == axis { 1.0 } else { 0.0 })
            .collect();
        conn.insert_entity(&EntityRow {
            uuid: format!("entity-{i:04}"),
            name: format!("Entity {i}"),
            group_id: group_id.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: ts.to_string(),
            name_embedding: emb,
            summary: format!("Summary {i}"),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }
    conn.build_indices_and_constraints().unwrap();
}

#[test]
fn hybrid_dedup_returns_none_when_below_threshold() {
    let dim = 8;
    let (db, _dir) = setup_dedup_db(dim);
    let conn = db.connect().unwrap();
    insert_test_entities(&conn, 3, dim, "test-group");

    // Query with a vector orthogonal to all stored embeddings: axis 7 (no entity uses it for dim<7)
    // Entity 0 -> axis 0, Entity 1 -> axis 1, Entity 2 -> axis 2
    // Use all-same small value to ensure cosine sim is below 0.85
    let query_emb: Vec<f32> = (0..dim)
        .map(|j| if j == dim - 1 { 1.0 } else { 0.0 })
        .collect();

    let result = conn
        .hybrid_dedup_similar_entity(&query_emb, "Entity 99", "test-group", 0.85)
        .unwrap();
    assert!(
        result.is_none(),
        "expected None for dissimilar query, got {:?}",
        result.map(|e| e.uuid)
    );
}

#[test]
fn hybrid_dedup_returns_best_match_above_threshold() {
    let dim = 8;
    let (db, _dir) = setup_dedup_db(dim);
    let conn = db.connect().unwrap();
    insert_test_entities(&conn, 3, dim, "test-group");

    // Entity 1 has embedding [0, 1, 0, 0, 0, 0, 0, 0]
    // Query with the identical embedding → cosine sim = 1.0 ≥ 0.85
    let query_emb: Vec<f32> = (0..dim).map(|j| if j == 1 { 1.0 } else { 0.0 }).collect();

    let result = conn
        .hybrid_dedup_similar_entity(&query_emb, "Entity 1", "test-group", 0.85)
        .unwrap();
    assert!(result.is_some(), "expected a match for identical embedding");
    assert_eq!(result.unwrap().uuid, "entity-0001");
}
