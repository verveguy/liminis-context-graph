use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use crate::{
    app_state::AppState,
    db::escape_pub,
    error::Error,
    types::{EntityRow, EpisodicRow, ExtractionResult, MentionsEdge, RelatesToEdge},
};

#[derive(Debug)]
pub struct AddEpisodeResult {
    pub episode_uuid: String,
    pub nodes_extracted: usize,
    pub edges_extracted: usize,
}

struct ActiveWriteGuard(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for ActiveWriteGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

const DEDUP_THRESHOLD: f32 = 0.85;

static HYBRID_THRESHOLD: OnceLock<usize> = OnceLock::new();

fn hybrid_threshold() -> usize {
    *HYBRID_THRESHOLD.get_or_init(|| {
        std::env::var("LIMINIS_DEDUP_HYBRID_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000)
    })
}

enum DedupDecision {
    Merge {
        existing_uuid: String,
        merged_summary: String,
    },
    Insert {
        row: EntityRow,
    },
}

/// Runs the full add_episode pipeline in three async phases (AD-4).
///
/// Phase A: concurrent HTTP (no lock) — embed body, extract entities/edges, embed names/facts.
/// Phase B: async dedup (no lock) — fetch cosine candidates, call DedupAdapter per candidate.
/// Phase C: commit (exclusive write lock) — apply dedup decisions, insert edges, episodic, MENTIONS.
///
/// Returns the episode UUID.
pub async fn add_episode(
    state: Arc<AppState>,
    name: &str,
    body: &str,
    source: &str,
    source_description: &str,
    reference_time: &str,
    group_id: &str,
) -> Result<AddEpisodeResult, Error> {
    // Track write in flight so rebuild_from_wal can gate on active writes.
    state.active_writes.fetch_add(1, Ordering::Relaxed);
    let _active_guard = ActiveWriteGuard(Arc::clone(&state.active_writes));

    // ── Phase A: concurrent HTTP (no lock) ────────────────────────────────────
    let (content_embedding, extraction): (Vec<f32>, ExtractionResult) = tokio::try_join!(
        state.embedder.embed(body),
        state.extractor.extract(body, group_id),
    )?;

    let entity_names: Vec<String> = extraction.entities.iter().map(|e| e.name.clone()).collect();
    let edge_facts: Vec<String> = extraction.edges.iter().map(|e| e.fact.clone()).collect();

    let mut name_embeddings: Vec<Vec<f32>> = Vec::with_capacity(entity_names.len());
    for n in &entity_names {
        name_embeddings.push(state.embedder.embed(n).await?);
    }
    let mut fact_embeddings: Vec<Vec<f32>> = Vec::with_capacity(edge_facts.len());
    for f in &edge_facts {
        fact_embeddings.push(state.embedder.embed(f).await?);
    }

    // ── Phase B: async dedup (no lock) ────────────────────────────────────────
    // Fetch cosine candidates in a blocking pass, then verify each with DedupAdapter.
    let db_shared = state.db.load_full().ok_or_else(|| {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        Error::DbUnavailable(reason)
    })?;
    let gid_b = group_id.to_string();
    let name_embs_b = name_embeddings.clone();
    let db_b = Arc::clone(&db_shared);
    let entity_count = tokio::task::spawn_blocking(move || {
        let conn = db_b.connect()?;
        conn.entity_count_in_group(&gid_b)
    })
    .await??;

    let use_hybrid = entity_count >= hybrid_threshold();
    let db_b = Arc::clone(&db_shared);
    let gid_b = group_id.to_string();
    let entity_names_b = entity_names.clone();
    let candidates: Vec<Option<EntityRow>> = tokio::task::spawn_blocking(move || {
        let conn = db_b.connect()?;
        let mut out = Vec::with_capacity(entity_names_b.len());
        for (i, _name) in entity_names_b.iter().enumerate() {
            let emb = &name_embs_b[i];
            let candidate = if use_hybrid {
                conn.hybrid_dedup_similar_entity(emb, _name, &gid_b, DEDUP_THRESHOLD)?
            } else {
                conn.brute_force_similar_entity(emb, &gid_b, DEDUP_THRESHOLD)?
            };
            out.push(candidate);
        }
        Ok::<_, Error>(out)
    })
    .await??;

    // Async dedup verification loop (no lock)
    let mut decisions: Vec<DedupDecision> = Vec::with_capacity(extraction.entities.len());
    let ref_time_owned = reference_time.to_string();
    let gid_owned = group_id.to_string();
    for (i, extracted) in extraction.entities.iter().enumerate() {
        let decision = if let Some(existing) = &candidates[i] {
            if state.dedup.is_duplicate(existing, extracted).await? {
                DedupDecision::Merge {
                    existing_uuid: existing.uuid.clone(),
                    merged_summary: format!("{} {}", existing.summary, extracted.summary),
                }
            } else {
                DedupDecision::Insert {
                    row: EntityRow {
                        uuid: uuid::Uuid::new_v4().to_string(),
                        name: extracted.name.clone(),
                        group_id: gid_owned.clone(),
                        labels: {
                            let mut labels = vec!["Entity".to_string()];
                            if !extracted.entity_type.is_empty()
                                && extracted.entity_type != "Entity"
                            {
                                labels.push(extracted.entity_type.clone());
                            }
                            labels
                        },
                        created_at: ref_time_owned.clone(),
                        name_embedding: name_embeddings[i].clone(),
                        summary: extracted.summary.clone(),
                        attributes: "{}".to_string(),
                        episode_uuids: vec![],
                        source_descriptions: vec![],
                    },
                }
            }
        } else {
            DedupDecision::Insert {
                row: EntityRow {
                    uuid: uuid::Uuid::new_v4().to_string(),
                    name: extracted.name.clone(),
                    group_id: gid_owned.clone(),
                    labels: {
                        let mut labels = vec!["Entity".to_string()];
                        if !extracted.entity_type.is_empty() && extracted.entity_type != "Entity" {
                            labels.push(extracted.entity_type.clone());
                        }
                        labels
                    },
                    created_at: ref_time_owned.clone(),
                    name_embedding: name_embeddings[i].clone(),
                    summary: extracted.summary.clone(),
                    attributes: "{}".to_string(),
                    episode_uuids: vec![],
                    source_descriptions: vec![],
                },
            }
        };
        decisions.push(decision);
    }

    // Capture counts before extraction moves into the Phase C closure.
    let nodes_extracted = extraction.entities.len();
    let edges_extracted = extraction.edges.len();

    // ── Phase C: commit under write lock ─────────────────────────────────────
    let episode_uuid = uuid::Uuid::new_v4().to_string();
    let ep_uuid = episode_uuid.clone();
    let name_owned = name.to_string();
    let body_owned = body.to_string();
    let source_owned = source.to_string();
    let source_desc_owned = source_description.to_string();
    let ref_time_owned = reference_time.to_string();
    let gid_owned = group_id.to_string();
    let db_c = state.db.load_full().ok_or_else(|| {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        Error::DbUnavailable(reason)
    })?;

    // Guard stays in async scope; spawn_blocking completes while it is held.
    // tokio::sync::RwLockWriteGuard is not 'static so it cannot move into the closure.
    let _write_guard = state.write_lock.write().await;
    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let conn = db_c.connect()?;

        // Apply dedup decisions → collect entity UUIDs
        let mut entity_uuids: Vec<String> = Vec::with_capacity(decisions.len());
        for decision in decisions {
            match decision {
                DedupDecision::Merge {
                    existing_uuid,
                    merged_summary,
                } => {
                    conn.run_cypher(&format!(
                        "MATCH (e:Entity {{uuid: '{}'}}) SET e.summary = '{}'",
                        escape_pub(&existing_uuid),
                        escape_pub(&merged_summary),
                    ))?;
                    entity_uuids.push(existing_uuid);
                }
                DedupDecision::Insert { row } => {
                    let uuid = row.uuid.clone();
                    conn.insert_entity(&row)?;
                    entity_uuids.push(uuid);
                }
            }
        }

        // name→uuid map for edge endpoint resolution
        let name_to_uuid: std::collections::HashMap<String, String> = extraction
            .entities
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.clone(), entity_uuids[i].clone()))
            .collect();

        // Insert relationship edges
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
                episode_uuids: vec![],
                source_descriptions: vec![],
            })?;
        }

        // Insert episodic node
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

        // Insert MENTIONS edges
        for entity_uuid in &entity_uuids {
            conn.insert_mentions_edge(&MentionsEdge {
                episodic_uuid: ep_uuid.clone(),
                entity_uuid: entity_uuid.clone(),
                group_id: gid_owned.clone(),
            })?;
        }

        // Step 7: TODO issue #3 — append WAL line and emit TelemetryEvent::WalAppend { duration_us, bytes } via sink

        Ok(())
    })
    .await??;
    drop(_write_guard);

    Ok(AddEpisodeResult {
        episode_uuid,
        nodes_extracted,
        edges_extracted,
    })
}
