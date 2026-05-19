use std::sync::{Arc, OnceLock};

use crate::{
    db::Db,
    embedder::Embedder,
    error::Error,
    extractor::Extractor,
    types::{EntityRow, EpisodicRow, ExtractionResult, MentionsEdge, RelatesToEdge},
};

const DEDUP_THRESHOLD: f32 = 0.85;

/// Minimum entity count in a group before the hybrid HNSW+BM25 dedup path is used.
/// Below this threshold the brute-force cosine path is used instead.
/// Override with the `LIMINIS_DEDUP_HYBRID_THRESHOLD` environment variable (default: 1000).
static HYBRID_THRESHOLD: OnceLock<usize> = OnceLock::new();

fn hybrid_threshold() -> usize {
    *HYBRID_THRESHOLD.get_or_init(|| {
        std::env::var("LIMINIS_DEDUP_HYBRID_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000)
    })
}

/// Runs the full add_episode pipeline (US1 FR-001).
///
/// Returns the episode UUID.
pub async fn add_episode(
    db: Arc<Db>,
    embedder: Arc<Embedder>,
    extractor: Arc<Extractor>,
    name: &str,
    body: &str,
    source: &str,
    source_description: &str,
    reference_time: &str,
    group_id: &str,
) -> Result<String, Error> {
    // 1+2: concurrent — embed body and extract entities/edges
    let (content_embedding, extraction): (Vec<f32>, ExtractionResult) = tokio::try_join!(
        embedder.embed(body),
        extractor.extract(body, group_id),
    )?;

    // Collect names and facts as owned Strings so they outlive the futures
    let entity_names: Vec<String> = extraction.entities.iter().map(|e| e.name.clone()).collect();
    let edge_facts: Vec<String> = extraction.edges.iter().map(|e| e.fact.clone()).collect();

    // Embed all entity names sequentially (parallel embed is issue #4 optimisation)
    let mut name_embeddings: Vec<Vec<f32>> = Vec::with_capacity(entity_names.len());
    for n in &entity_names {
        name_embeddings.push(embedder.embed(n).await?);
    }
    let mut fact_embeddings: Vec<Vec<f32>> = Vec::with_capacity(edge_facts.len());
    for f in &edge_facts {
        fact_embeddings.push(embedder.embed(f).await?);
    }

    // 3-6: all DB work in one spawn_blocking
    let episode_uuid = uuid::Uuid::new_v4().to_string();
    let ep_uuid = episode_uuid.clone();
    let name_owned = name.to_string();
    let body_owned = body.to_string();
    let source_owned = source.to_string();
    let source_desc_owned = source_description.to_string();
    let ref_time_owned = reference_time.to_string();
    let gid_owned = group_id.to_string();

    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let conn = db.connect()?;

        // Step 3: dedup entities
        // Determine dedup strategy once per episode (count query is cheap relative to dedup).
        let entity_count = conn.entity_count_in_group(&gid_owned)?;
        let use_hybrid = entity_count >= hybrid_threshold();
        let mut entity_uuids: Vec<String> = Vec::new();

        for (i, extracted) in extraction.entities.iter().enumerate() {
            let name_emb = &name_embeddings[i];
            let existing = if use_hybrid {
                conn.hybrid_dedup_similar_entity(
                    name_emb,
                    &extracted.name,
                    &gid_owned,
                    DEDUP_THRESHOLD,
                )?
            } else {
                conn.brute_force_similar_entity(name_emb, &gid_owned, DEDUP_THRESHOLD)?
            };

            let entity_uuid = if let Some(existing_row) = existing {
                let merged_summary = format!("{} {}", existing_row.summary, extracted.summary);
                conn.run_cypher(&format!(
                    "MATCH (e:Entity {{uuid: '{}'}}) SET e.summary = '{}'",
                    crate::db::escape_pub(&existing_row.uuid),
                    crate::db::escape_pub(&merged_summary),
                ))?;
                existing_row.uuid
            } else {
                let new_uuid = uuid::Uuid::new_v4().to_string();
                let mut labels = vec!["Entity".to_string()];
                if !extracted.entity_type.is_empty() && extracted.entity_type != "Entity" {
                    labels.push(extracted.entity_type.clone());
                }
                conn.insert_entity(&EntityRow {
                    uuid: new_uuid.clone(),
                    name: extracted.name.clone(),
                    group_id: gid_owned.clone(),
                    labels,
                    created_at: ref_time_owned.clone(),
                    name_embedding: name_emb.clone(),
                    summary: extracted.summary.clone(),
                    attributes: "{}".to_string(),
                })?;
                new_uuid
            };
            entity_uuids.push(entity_uuid);
        }

        // name→uuid map for edge endpoint resolution
        let name_to_uuid: std::collections::HashMap<String, String> = extraction
            .entities
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.clone(), entity_uuids[i].clone()))
            .collect();

        // Step 4: insert relationship edges
        for (i, edge) in extraction.edges.iter().enumerate() {
            let src_uuid = match name_to_uuid.get(&edge.source_name) {
                Some(u) => u.clone(),
                None => continue,
            };
            let dst_uuid = match name_to_uuid.get(&edge.target_name) {
                Some(u) => u.clone(),
                None => continue,
            };
            conn.insert_relates_to_edge(&RelatesToEdge {
                uuid: uuid::Uuid::new_v4().to_string(),
                name: format!("{} → {}", edge.source_name, edge.target_name),
                source_node_uuid: src_uuid,
                target_node_uuid: dst_uuid,
                group_id: gid_owned.clone(),
                fact: edge.fact.clone(),
                fact_embedding: fact_embeddings[i].clone(),
                created_at: ref_time_owned.clone(),
                valid_at: Some(ref_time_owned.clone()),
                invalid_at: None,
                attributes: "{}".to_string(),
            })?;
        }

        // Step 5: insert episodic node
        conn.insert_episodic(&EpisodicRow {
            uuid: ep_uuid.clone(),
            name: name_owned,
            group_id: gid_owned.clone(),
            created_at: ref_time_owned.clone(),
            source: source_owned,
            source_description: source_desc_owned,
            content: body_owned,
            content_embedding,
            valid_at: ref_time_owned.clone(),
            entity_edges: entity_uuids.clone(),
        })?;

        // Step 6: insert MENTIONS edges
        for entity_uuid in &entity_uuids {
            conn.insert_mentions_edge(&MentionsEdge {
                episodic_uuid: ep_uuid.clone(),
                entity_uuid: entity_uuid.clone(),
                group_id: gid_owned.clone(),
            })?;
        }

        // Step 7: TODO issue #3: append WAL line

        Ok(())
    })
    .await??;

    Ok(episode_uuid)
}
