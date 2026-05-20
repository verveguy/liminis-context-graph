use liminis_graph_core::{Db, EntityRow, EpisodicRow};

/// LadybugDB binding spike: proves lbug 0.16.1 can open a file DB, write Entity and Episodic
/// nodes with 768-dim vector properties, create HNSW vector indexes, and search by name prefix.
/// The FTS extension is loaded (required for future FTS index creation), but FTS index
/// creation and queries are out of scope for this spike.
#[test]
fn round_trip_entity_and_episodic() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db_path = dir.path().join("spike.db");
    let db_path_str = db_path.to_str().expect("utf-8 path");

    // 1. Open DB and connect (extensions loaded automatically by connect()).
    let db = Db::open(db_path_str).expect("open db");
    let conn = db.connect().expect("connect");

    // 2. Initialise schema with 768-dim embeddings (bge-base-en-v1.5).
    conn.init_schema(768).expect("init schema");

    // 3. Insert 3 Entity rows with synthetic zero-valued embeddings.
    let embedding = vec![0.0f32; 768];
    let ts = "2026-01-01 00:00:00";
    for i in 0..3 {
        conn.insert_entity(&EntityRow {
            uuid: format!("entity-{i}"),
            name: format!("Entity {i}"),
            group_id: "test-group".to_string(),
            labels: vec!["test".to_string()],
            created_at: ts.to_string(),
            name_embedding: embedding.clone(),
            summary: format!("Summary {i}"),
            attributes: "{}".to_string(),
        })
        .expect("insert entity");
    }

    // 4. Insert 1 Episodic row.
    conn.insert_episodic(&EpisodicRow {
        uuid: "episodic-0".to_string(),
        name: "Episode 0".to_string(),
        group_id: "test-group".to_string(),
        created_at: ts.to_string(),
        source: "test".to_string(),
        source_description: "synthetic test episode".to_string(),
        content: "The quick brown fox.".to_string(),
        content_embedding: embedding.clone(),
        valid_at: ts.to_string(),
        entity_edges: vec![],
    })
    .expect("insert episodic");

    // 5. Create HNSW indexes AFTER inserts (AD-4).
    conn.create_vector_indexes().expect("create vector indexes");

    // 6. Search — empty prefix matches all entities.
    let results = conn.search_entities("").expect("search entities");
    assert_eq!(
        results.len(),
        3,
        "expected 3 entities, got {}",
        results.len()
    );

    // 7. Prefix search should narrow results.
    let results = conn.search_entities("Entity 1").expect("prefix search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "Entity 1");
}
