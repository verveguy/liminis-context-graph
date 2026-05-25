use std::sync::Arc;

use liminis_graph_core::{
    db::Db,
    embedder::{Embedder, OaiEmbedder},
    search,
    types::{EntityRow, EpisodicRow},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::TempDir::new()?;
    let db_path = dir.path().join("demo.db");
    let db = Arc::new(Db::open(db_path.to_str().unwrap())?);
    {
        let conn = db.connect()?;
        conn.init_schema(768)?;
    }

    let embedding = vec![0.0f32; 768];
    let ts = "2026-01-01 00:00:00";
    let docs = [
        ("Alice", "Alice is a software engineer."),
        ("Bob", "Bob works on distributed systems."),
        ("Carol", "Carol specialises in knowledge graphs."),
    ];

    {
        let conn = db.connect()?;
        for (name, summary) in &docs {
            conn.insert_entity(&EntityRow {
                uuid: format!("entity-{}", name.to_lowercase()),
                name: name.to_string(),
                group_id: "demo".to_string(),
                labels: vec!["Person".to_string()],
                created_at: ts.to_string(),
                name_embedding: embedding.clone(),
                summary: summary.to_string(),
                attributes: "{}".to_string(),
                ..Default::default()
            })?;
        }

        conn.insert_episodic(&EpisodicRow {
            uuid: "ep-0".to_string(),
            name: "Team intro".to_string(),
            group_id: "demo".to_string(),
            created_at: ts.to_string(),
            source: "manual".to_string(),
            source_description: "demo episode".to_string(),
            content: "Alice, Bob, and Carol form the core team.".to_string(),
            content_embedding: embedding.clone(),
            valid_at: ts.to_string(),
            entity_edges: vec![],
        })?;

        conn.create_vector_indexes()?;

        // FTS search (works without an embedding service)
        println!("FTS results for 'knowledge':");
        for (uuid, score) in conn.fts_search_entities("knowledge", &["demo"], 5)? {
            println!("  uuid={uuid}  score={score:.4}");
        }
    }

    // Hybrid search: requires LCG_EMBEDDING_URL to be set to a running
    // embedding service. Falls back gracefully if the service is unavailable.
    let embedder: Arc<dyn Embedder> = Arc::new(OaiEmbedder::from_env());
    println!("\nHybrid entity search for 'distributed systems':");
    match search::hybrid_entity_search(
        Arc::clone(&db),
        Arc::clone(&embedder),
        "distributed systems",
        vec!["demo".to_string()],
        5,
    )
    .await
    {
        Ok(entities) if entities.is_empty() => println!("  (no results)"),
        Ok(entities) => {
            for e in &entities {
                println!("  {} — {}", e.name, e.summary);
            }
        }
        Err(e) => println!("  (embedding service unavailable: {e})"),
    }

    Ok(())
}
