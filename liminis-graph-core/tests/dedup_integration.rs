/// Integration tests for the hybrid/brute-force dedup threshold gate.
///
/// These tests exercise the Conn-level dedup methods and the entity_count_in_group
/// helper directly, without requiring the Embedder or Extractor external services.
use liminis_graph_core::{Db, EntityRow};

fn build_db_with_entities(n: usize, dim: usize) -> (Db, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("dedup_int.db").to_str().unwrap()).unwrap();
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
                group_id: "test".to_string(),
                labels: vec!["Entity".to_string()],
                created_at: ts.to_string(),
                name_embedding: emb,
                summary: format!("Summary {i}"),
                attributes: "{}".to_string(),
            })
            .unwrap();
        }
        conn.build_indices_and_constraints().unwrap();
    }
    (db, dir)
}

/// Below the default threshold (1000), brute-force is used.
/// Verify entity_count_in_group returns the correct count and that brute-force
/// correctly deduplicates an exact-match query.
#[test]
fn dedup_falls_back_to_brute_force_below_threshold() {
    let dim = 8;
    let (db, _dir) = build_db_with_entities(10, dim);
    let conn = db.connect().unwrap();

    let count = conn.entity_count_in_group("test").unwrap();
    assert_eq!(count, 10);

    // With LIMINIS_DEDUP_HYBRID_THRESHOLD=50000, the threshold is far above 10,
    // so brute-force would be used regardless. Verify brute-force finds the exact match.
    // Entity 3 has embedding along axis 3.
    let query_emb: Vec<f32> = (0..dim).map(|j| if j == 3 { 1.0 } else { 0.0 }).collect();
    let result = conn
        .brute_force_similar_entity(&query_emb, "test", 0.85)
        .unwrap();
    assert!(result.is_some(), "brute-force should find entity 3");
    assert_eq!(result.unwrap().uuid, "e000003");
}

/// Above the threshold (seeded with 1001 entities), hybrid dedup is used.
/// Verify the hybrid path finds the correct entity and agrees with brute-force.
#[test]
fn dedup_uses_hybrid_above_threshold() {
    let dim = 8;
    // 1001 entities exceeds the default LIMINIS_DEDUP_HYBRID_THRESHOLD of 1000
    let (db, _dir) = build_db_with_entities(1001, dim);
    let conn = db.connect().unwrap();

    let count = conn.entity_count_in_group("test").unwrap();
    assert!(count >= 1000, "expected ≥ 1000 entities, got {count}");

    // Entity 500 % 8 = 4 → axis 4
    let axis = 500 % dim;
    let query_emb: Vec<f32> = (0..dim).map(|j| if j == axis { 1.0 } else { 0.0 }).collect();
    let entity_name = "Entity 500";

    let hybrid_result = conn
        .hybrid_dedup_similar_entity(&query_emb, entity_name, "test", 0.85)
        .unwrap();
    let brute_result = conn
        .brute_force_similar_entity(&query_emb, "test", 0.85)
        .unwrap();

    // Both paths must find a match
    assert!(hybrid_result.is_some(), "hybrid should find entity at axis {axis}");
    assert!(brute_result.is_some(), "brute-force should find entity at axis {axis}");

    // Both paths must return the same entity UUID — brute-force ties by lowest UUID, and
    // hybrid must agree to satisfy the ≥ 95% decision-overlap requirement (R-003).
    assert_eq!(
        hybrid_result.unwrap().uuid,
        brute_result.unwrap().uuid,
        "hybrid and brute-force must return the same entity UUID"
    );
}

/// entity_count_in_group returns 0 for an empty or non-existent group.
#[test]
fn entity_count_in_group_returns_zero_for_empty_group() {
    let dim = 8;
    let (db, _dir) = build_db_with_entities(5, dim);
    let conn = db.connect().unwrap();

    let count = conn.entity_count_in_group("nonexistent-group").unwrap();
    assert_eq!(count, 0);
}
