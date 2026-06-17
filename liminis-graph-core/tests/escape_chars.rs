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
        ..Default::default()
    });

    assert!(
        result.is_ok(),
        "insert_entity with adversarial characters failed: {:?}",
        result.err()
    );
}

/// Regression: a STRING column whose value *looks* like an RFC-3339 timestamp must be stored
/// verbatim — not silently rewritten to a TIMESTAMP. Timestamp coercion is gated on the
/// destination param name (created_at/valid_at/invalid_at/expired_at), never on value shape,
/// so user content like `Episodic.content` / `Entity.summary` is never corrupted.
#[test]
fn timestamp_shaped_string_columns_stored_verbatim() {
    use liminis_graph_core::{Db, EntityRow, EpisodicRow};
    let db_dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(db_dir.path().join("t.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let ts_like = "2026-03-25T16:58:57+00:00";
    conn.insert_entity(&EntityRow {
        uuid: "e1".into(),
        name: ts_like.into(), // STRING column with a timestamp-shaped value
        group_id: "g".into(),
        labels: vec!["Entity".into()],
        created_at: "2026-03-25T16:58:57.000000+00:00".into(),
        name_embedding: vec![0.1, 0.2, 0.3, 0.4],
        summary: ts_like.into(), // STRING column
        attributes: "{}".into(),
        ..Default::default()
    })
    .unwrap();
    conn.insert_episodic(&EpisodicRow {
        uuid: "ep1".into(),
        name: "chunk".into(),
        group_id: "g".into(),
        created_at: "2026-03-25T16:58:57.000000+00:00".into(),
        source: "text".into(),
        source_description: "doc".into(),
        content: ts_like.into(), // STRING column — the primary content
        content_embedding: vec![0.1, 0.2, 0.3, 0.4],
        valid_at: "2026-03-25T16:58:57.000000+00:00".into(),
        entity_edges: vec![],
    })
    .unwrap();

    let ent = conn.get_entity_by_uuid("e1").unwrap().expect("entity");
    assert_eq!(
        ent.name, ts_like,
        "Entity.name (STRING) must be verbatim, not a timestamp"
    );
    assert_eq!(
        ent.summary, ts_like,
        "Entity.summary (STRING) must be verbatim"
    );

    let eps = conn.retrieve_episodes("g", 10).unwrap();
    let p = eps.iter().find(|p| p.uuid == "ep1").expect("episode");
    assert_eq!(
        p.content, ts_like,
        "Episodic.content (STRING) must be verbatim, not a timestamp"
    );
}
