use liminis_graph_core::{Db, EntityRow};

fn setup_db(dim: usize) -> (Db, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("escape_chars.db").to_str().unwrap()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }
    (db, dir)
}

/// REQ-06: insert_entity must succeed for names containing ', \, ", \n, and {}.
#[test]
fn insert_entity_with_adversarial_chars() {
    let dim = 4;
    let (db, _dir) = setup_db(dim);
    let conn = db.connect().unwrap();

    let emb = vec![1.0_f32, 0.0, 0.0, 0.0];
    let result = conn.insert_entity(&EntityRow {
        uuid: "adversarial-0001".to_string(),
        // Contains apostrophe, backslash, double-quote, newline, and curly braces.
        name: r#"Lincoln Center's "51st" \Chaplin\ Award
{ceremony}"#
            .to_string(),
        group_id: "test-group".to_string(),
        labels: vec!["Entity".to_string()],
        created_at: "2026-01-01 00:00:00".to_string(),
        name_embedding: emb,
        summary: r#"Subject's work on "film" in the \arts\ domain."#.to_string(),
        attributes: r#"{"key": "O'Brien's value", "path": "C:\\dir"}"#.to_string(),
    });

    assert!(
        result.is_ok(),
        "insert_entity with adversarial characters failed: {:?}",
        result.err()
    );
}
