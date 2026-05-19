use liminis_graph_core::{Db, EntityRow, EpisodicRow};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::TempDir::new()?;
    let db_path = dir.path().join("demo.db");

    println!("Opening database at {}", db_path.display());

    let db = Db::open(db_path.to_str().unwrap())?;
    let conn = db.connect()?;
    conn.init_schema(768)?;

    let embedding = vec![0.0f32; 768];
    let ts = "2026-01-01 00:00:00";

    let docs = [
        ("Alice", "Alice is a software engineer."),
        ("Bob", "Bob works on distributed systems."),
        ("Carol", "Carol specialises in knowledge graphs."),
    ];

    for (name, summary) in &docs {
        conn.insert_entity(&EntityRow {
            uuid: format!("entity-{}", name.to_lowercase()),
            name: name.to_string(),
            group_id: "demo".to_string(),
            labels: vec!["person".to_string()],
            created_at: ts.to_string(),
            name_embedding: embedding.clone(),
            summary: summary.to_string(),
            attributes: "{}".to_string(),
        })?;
        println!("Ingested: {name}");
    }

    conn.insert_episodic(&EpisodicRow {
        uuid: "ep-0".to_string(),
        name: "Team intro".to_string(),
        group_id: "demo".to_string(),
        created_at: ts.to_string(),
        source: "manual".to_string(),
        source_description: "demo episode".to_string(),
        content: "Alice, Bob, and Carol form the core team.".to_string(),
        content_embedding: embedding,
        valid_at: ts.to_string(),
        entity_edges: vec![],
    })?;

    conn.create_vector_indexes()?;

    println!("\nSearch results for prefix \"\":");
    for entity in conn.search_entities("")? {
        println!("  {} — {}", entity.name, entity.summary);
    }

    Ok(())
}
