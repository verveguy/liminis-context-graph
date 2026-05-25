use liminis_graph_core::{Db, EntityRow};

const DIM: usize = 8;

fn zero_vec(len: usize) -> Vec<f32> {
    vec![0.0f32; len]
}

fn unit_vec(len: usize, hot_idx: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; len];
    if hot_idx < len {
        v[hot_idx] = 1.0;
    }
    v
}

fn format_float_array(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f:.6}")).collect();
    format!("[{}]", parts.join(","))
}

/// T002 [LDB] — REL TABLE creation and MATCH query round-trip.
#[test]
fn test_rel_table_creation_and_query() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("spike.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(DIM).unwrap();

    conn.insert_entity(&EntityRow {
        uuid: "e1".to_string(),
        name: "Alice".to_string(),
        group_id: "test".to_string(),
        labels: vec!["Entity".to_string()],
        created_at: "2026-01-01 00:00:00".to_string(),
        name_embedding: zero_vec(DIM),
        summary: "Alice summary".to_string(),
        attributes: "{}".to_string(),
        ..Default::default()
    })
    .unwrap();
    conn.insert_entity(&EntityRow {
        uuid: "e2".to_string(),
        name: "Bob".to_string(),
        group_id: "test".to_string(),
        labels: vec!["Entity".to_string()],
        created_at: "2026-01-01 00:00:00".to_string(),
        name_embedding: zero_vec(DIM),
        summary: "Bob summary".to_string(),
        attributes: "{}".to_string(),
        ..Default::default()
    })
    .unwrap();

    // Create RELATES_TO rel table
    conn.run_cypher(
        "CREATE REL TABLE IF NOT EXISTS RELATES_TO (FROM Entity TO Entity, \
         uuid STRING, name STRING, group_id STRING, fact STRING, \
         valid_at TIMESTAMP, invalid_at TIMESTAMP, attributes STRING)",
    )
    .unwrap();

    // Insert a RELATES_TO edge between Alice and Bob
    conn.run_cypher(
        "MATCH (a:Entity {uuid: 'e1'}), (b:Entity {uuid: 'e2'}) \
         CREATE (a)-[:RELATES_TO {uuid: 'r1', name: 'knows', group_id: 'test', \
         fact: 'Alice knows Bob', attributes: '{}'}]->(b)",
    )
    .unwrap();

    // Query it back and collect as strings
    let rows = conn
        .cypher_query(
            "MATCH (a:Entity)-[r:RELATES_TO]->(b:Entity) \
             RETURN r.uuid, r.fact",
        )
        .unwrap();

    assert!(
        !rows.is_empty(),
        "RELATES_TO edge was not found after insert"
    );
    let first = &rows[0];
    assert_eq!(first[0], "r1", "uuid mismatch");
    assert_eq!(first[1], "Alice knows Bob", "fact mismatch");
}

/// T003 [P] [LDB] — FTS index creation and QUERY_FTS_INDEX round-trip.
#[test]
fn test_fts_index_creation_and_query() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("spike.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(DIM).unwrap();

    for (uuid, name) in [("e1", "Alice"), ("e2", "Bob"), ("e3", "Alice Smith")] {
        conn.insert_entity(&EntityRow {
            uuid: uuid.to_string(),
            name: name.to_string(),
            group_id: "test".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: zero_vec(DIM),
            summary: format!("{name} summary"),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    // FTS index created by init_schema above; query using lbug-native syntax (no YIELD)
    let rows = conn
        .cypher_query(
            "CALL QUERY_FTS_INDEX('Entity', 'node_name_and_summary', 'Alice') \
             WITH node, score RETURN node.uuid, score",
        )
        .unwrap();

    assert!(
        !rows.is_empty(),
        "FTS query for 'Alice' returned no results"
    );
    let first = &rows[0];
    assert_eq!(first.len(), 2, "expected 2 columns (uuid, score)");
    eprintln!("FTS result[0]: uuid={:?}, score={:?}", first[0], first[1]);
}

/// T004 [P] [LDB] — HNSW vector index creation and QUERY_VECTOR_INDEX round-trip.
#[test]
fn test_hnsw_vector_query() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("spike.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(DIM).unwrap();

    // Insert entities with distinct embeddings
    for i in 0..5usize {
        conn.insert_entity(&EntityRow {
            uuid: format!("e{i}"),
            name: format!("Entity {i}"),
            group_id: "test".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: unit_vec(DIM, i % DIM),
            summary: format!("Summary {i}"),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    // Create HNSW index after inserts (AD-4)
    conn.run_cypher(
        "CALL CREATE_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', \
         'name_embedding', metric := 'cosine')",
    )
    .unwrap();

    // Query the HNSW index — find 3 nearest to unit_vec[0]
    let query_vec = unit_vec(DIM, 0);
    let vec_literal = format_float_array(&query_vec);
    let sql = format!(
        "CALL QUERY_VECTOR_INDEX('Entity', 'entity_name_embedding_idx', {vec_literal}, 3) \
         RETURN node.uuid, distance"
    );
    let rows = conn.cypher_query(&sql).unwrap();

    assert!(!rows.is_empty(), "HNSW query returned no results");
    let first = &rows[0];
    assert_eq!(first.len(), 2, "expected 2 columns (uuid, distance)");
    eprintln!(
        "HNSW result[0]: uuid={:?}, distance={:?}",
        first[0], first[1]
    );
    // The closest result should be e0 (same direction as query vector)
    assert_eq!(first[0], "e0", "nearest neighbor should be e0");
}
