use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use chrono::DateTime;

use crate::{
    app_state::{build_indices_once, load_db, AppState, OntologyDriftState},
    db::Db,
    error::{is_missing_index_error, Error, MISSING_INDEX_USER_MSG},
    extractor::ExtractOptions,
    ontology::{normalize_entity_type, OntologyMode},
    ontology_sidecar,
    types::{EntityRow, EpisodicRow, ExtractionResult, MentionsEdge, RelatesToEdge, SourceType},
    wal_exec,
};

#[derive(Debug)]
pub struct AddEpisodeResult {
    pub episode_uuid: String,
    pub nodes_extracted: usize,
    pub edges_extracted: usize,
    /// Edges whose endpoint(s) could not be resolved against either this batch's entities or
    /// the persisted graph, and were dropped at Phase C commit time (FR-004, FR-005).
    pub edges_dropped_unresolvable: usize,
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

/// Result of Phase B's per-entity resolution attempt.
/// Name-matched entities skip the async dedup-adapter check entirely.
enum PhaseBResult {
    /// Exact case-insensitive name match found in the persisted graph.
    NameMatch { existing: EntityRow },
    /// No name match; embedding-based candidate (may be None if no similar entity exists).
    EmbeddingCandidate { candidate: Option<EntityRow> },
}

/// Validates and returns a timestamp string from LLM output.
///
/// Returns `None` for empty strings or values that cannot be parsed as RFC 3339,
/// so invalid LLM output does not reach the DB's `timestamp()` call.
fn validate_llm_timestamp(s: Option<String>) -> Option<String> {
    let s = s?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if DateTime::parse_from_rfc3339(trimmed).is_ok() {
        Some(trimmed.to_string())
    } else {
        eprintln!(
            "liminis-context-graph: dropping invalid LLM timestamp: {:?}",
            trimmed
        );
        None
    }
}

/// Resolves each extracted entity name against the persisted graph: an exact case-insensitive
/// name match short-circuits to `NameMatch`, otherwise falls back to embedding-based candidate
/// lookup (hybrid HNSW+FTS above the dedup threshold, brute-force cosine scan below it).
///
/// Runs the whole batch inside a single `spawn_blocking` closure. On a missing-index error
/// (the hybrid path depends on `entity_name_embedding_idx`), the caller retries by calling this
/// function again in full — there is no partial/mid-batch resume.
async fn resolve_phase_b(
    db: Arc<Db>,
    group_id: String,
    entity_names: Vec<String>,
    name_embeddings: Vec<Vec<f32>>,
    use_hybrid: bool,
) -> Result<Vec<PhaseBResult>, Error> {
    tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let mut out = Vec::with_capacity(entity_names.len());
        for (i, name) in entity_names.iter().enumerate() {
            let trimmed = name.trim();
            // Name-first resolution: case-insensitive exact match short-circuits embedding lookup.
            if let Some(existing) = conn.get_entity_by_name_ci(trimmed, &group_id)? {
                out.push(PhaseBResult::NameMatch { existing });
                continue;
            }
            // Embedding-based resolution fallback.
            let emb = &name_embeddings[i];
            let candidate = if use_hybrid {
                conn.hybrid_dedup_similar_entity(emb, trimmed, &group_id, DEDUP_THRESHOLD)?
            } else {
                conn.brute_force_similar_entity(emb, &group_id, DEDUP_THRESHOLD)?
            };
            out.push(PhaseBResult::EmbeddingCandidate { candidate });
        }
        Ok::<_, Error>(out)
    })
    .await?
}

/// Runs the full add_episode pipeline in three async phases (AD-4).
///
/// Phase A: concurrent HTTP (no lock) — embed body, extract entities/edges, embed names/facts.
/// Phase B: async dedup (no lock) — fetch cosine candidates, call DedupAdapter per candidate.
/// Phase C: commit (exclusive write lock) — apply dedup decisions, insert edges, episodic, MENTIONS.
///
/// Returns the episode UUID.
#[allow(clippy::too_many_arguments)]
pub async fn add_episode(
    state: Arc<AppState>,
    name: &str,
    body: &str,
    source: &str,
    source_description: &str,
    reference_time: &str,
    group_id: &str,
    source_type: SourceType,
    custom_instructions: Option<&str>,
) -> Result<AddEpisodeResult, Error> {
    // Track write in flight so rebuild_from_wal can gate on active writes.
    state.active_writes.fetch_add(1, Ordering::Relaxed);
    let _active_guard = ActiveWriteGuard(Arc::clone(&state.active_writes));

    // ── Phase A: concurrent HTTP (no lock) ────────────────────────────────────
    let ontology_ref = state.ontology.as_deref();
    let extract_opts = ExtractOptions {
        episode_body: body,
        group_id,
        source_type,
        custom_instructions,
        reference_time,
        ontology: ontology_ref,
    };
    let (content_embedding, mut extraction): (Vec<f32>, ExtractionResult) = tokio::select! {
        result = async {
            tokio::try_join!(
                state.embedder.embed(body),
                state.extractor.extract(extract_opts)
            )
        } => result?,
        _ = state.cancel_token.cancelled() => {
            state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Cancelled);
        }
    };

    // Strict-mode entity filtering: drop entities not in the declared vocabulary.
    if let Some(onto) = ontology_ref {
        if onto.mode == OntologyMode::Strict && onto.has_entity_types() {
            let vocab = onto.entity_type_names();
            extraction.entities.retain(|e| {
                let normalized = normalize_entity_type(&e.entity_type);
                if vocab.contains(&normalized) {
                    true
                } else {
                    eprintln!(
                        "liminis-context-graph: ontology strict: dropping entity '{}' (type '{}' not in vocabulary)",
                        e.name, e.entity_type
                    );
                    false
                }
            });
            // Rewrite entity_type to the canonical normalized form so DB labels are consistent.
            for e in extraction.entities.iter_mut() {
                e.entity_type = normalize_entity_type(&e.entity_type);
            }
            if extraction.entities.is_empty() {
                eprintln!(
                    "liminis-context-graph: ontology strict: no entities remain after vocabulary filtering for this chunk"
                );
                // Clear edges to avoid wasted embedding work; endpoints are gone.
                extraction.edges.clear();
            }
        }
    }

    // Strict-mode relation_type filtering: drop edges whose relation_type is not in the vocabulary.
    if let Some(onto) = ontology_ref {
        if onto.mode == OntologyMode::Strict && onto.has_relation_types() {
            let vocab = onto.relation_type_names();
            extraction.edges.retain(|e| {
                match &e.relation_type {
                    Some(rt) if vocab.contains(rt.as_str()) => true,
                    Some(rt) => {
                        eprintln!(
                            "liminis-context-graph: ontology strict: dropping edge '{}' → '{}' (relation_type '{}' not in vocabulary)",
                            e.source_name, e.target_name, rt
                        );
                        false
                    }
                    None => {
                        eprintln!(
                            "liminis-context-graph: ontology strict: dropping edge '{}' → '{}' (no relation_type)",
                            e.source_name, e.target_name
                        );
                        false
                    }
                }
            });
        }
    }

    // Drop entities with empty or whitespace-only names before edge validation so that
    // any edges referencing them are also dropped as unresolvable (spec edge case: treat
    // empty-name extraction as a failure and do not create a node for it).
    extraction.entities.retain(|e| !e.name.trim().is_empty());

    // Load the DB handle here (rather than at Phase B, below) — Phase B's entity-count check
    // and dedup resolution, and Phase C's commit, all reuse this same Arc.
    let db_shared = state.db.load_full().ok_or_else(|| {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        Error::DbUnavailable(reason)
    })?;

    // name_embeddings is computed here — before edge validation, rather than after it as
    // before — so the salvage step below can cosine-match an off-list edge endpoint against
    // the batch's own entity name embeddings without re-embedding anything (order-only change;
    // extraction.entities is already final by this point).
    let entity_names: Vec<String> = extraction.entities.iter().map(|e| e.name.clone()).collect();
    let mut name_embeddings: Vec<Vec<f32>> = Vec::with_capacity(entity_names.len());
    for n in &entity_names {
        name_embeddings.push(tokio::select! {
            r = state.embedder.embed(n) => r?,
            _ = state.cancel_token.cancelled() => {
                state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
                return Err(Error::Cancelled);
            }
        });
    }

    // Post-extraction edge validation (pre-lock, advisory only): drop self-referential edges —
    // a pure, DB-independent check that's always correct — then *salvage*, rather than
    // permanently drop, edges whose endpoint name is absent from this batch's own entity list.
    // An off-list endpoint's name embedding is cosine-matched against the batch's entity
    // name_embeddings, reusing DEDUP_THRESHOLD (the same threshold already used for entity
    // dedup); a match rewrites the edge's endpoint to that entity's canonical name in place.
    // Anything that doesn't salvage-match is left untouched and passed through to Phase C
    // (write-lock held), which is now the *sole* point that resolves an endpoint — falling back
    // to the persisted graph — or finally drops the edge, making `edges_dropped_unresolvable`
    // authoritative (FR-003, FR-005) instead of one of two independent, easily-desynced passes.
    extraction.edges.retain(|edge| {
        if edge.source_name.trim().to_lowercase() == edge.target_name.trim().to_lowercase() {
            eprintln!(
                "liminis-context-graph: dropping self-referential edge: '{}' → '{}'",
                edge.source_name, edge.target_name
            );
            return false;
        }
        true
    });

    if !extraction.edges.is_empty() {
        let entity_name_set: std::collections::HashSet<String> = extraction
            .entities
            .iter()
            .map(|e| e.name.trim().to_lowercase())
            .collect();

        // Collect the unique off-batch endpoint names needing salvage, keyed by lowercase, so a
        // batch with many edges naming the same missing endpoint costs one embed+match, not one
        // per edge.
        let mut missing_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for edge in &extraction.edges {
            for name in [&edge.source_name, &edge.target_name] {
                let trimmed = name.trim();
                let lower = trimmed.to_lowercase();
                if !entity_name_set.contains(&lower) {
                    missing_names
                        .entry(lower)
                        .or_insert_with(|| trimmed.to_string());
                }
            }
        }

        let mut salvage_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (lower, original) in missing_names {
            let emb = tokio::select! {
                r = state.embedder.embed(&original) => r?,
                _ = state.cancel_token.cancelled() => {
                    state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
                    return Err(Error::Cancelled);
                }
            };
            let mut best: Option<(f32, &str)> = None;
            for (i, candidate_emb) in name_embeddings.iter().enumerate() {
                let score = crate::db::cosine_similarity(&emb, candidate_emb);
                let is_better = match best {
                    Some((b, _)) => score > b,
                    None => true,
                };
                if score >= DEDUP_THRESHOLD && is_better {
                    best = Some((score, extraction.entities[i].name.as_str()));
                }
            }
            if let Some((score, canonical)) = best {
                eprintln!(
                    "liminis-context-graph: salvaging off-list edge endpoint '{}' → '{}' (cosine similarity {:.3})",
                    original, canonical, score
                );
                salvage_map.insert(lower, canonical.to_string());
            }
        }

        if !salvage_map.is_empty() {
            for edge in extraction.edges.iter_mut() {
                if let Some(canonical) = salvage_map.get(&edge.source_name.trim().to_lowercase()) {
                    edge.source_name = canonical.clone();
                }
                if let Some(canonical) = salvage_map.get(&edge.target_name.trim().to_lowercase()) {
                    edge.target_name = canonical.clone();
                }
            }

            // Rewriting an off-list endpoint to its canonical entity name can make two
            // previously-distinct endpoints collide (e.g. "Global Warming" and "Climate Change"
            // both salvage to the same batch entity) — re-run the self-referential filter after
            // salvage so a rewritten edge like that doesn't slip past the earlier check and get
            // inserted as a self-loop in Phase C, which has no self-reference guard of its own.
            extraction.edges.retain(|edge| {
                if edge.source_name.trim().to_lowercase() == edge.target_name.trim().to_lowercase()
                {
                    eprintln!(
                        "liminis-context-graph: dropping self-referential edge after salvage: '{}' → '{}'",
                        edge.source_name, edge.target_name
                    );
                    return false;
                }
                true
            });
        }
    }

    let edge_facts: Vec<String> = extraction.edges.iter().map(|e| e.fact.clone()).collect();
    let mut fact_embeddings: Vec<Vec<f32>> = Vec::with_capacity(edge_facts.len());
    for f in &edge_facts {
        fact_embeddings.push(tokio::select! {
            r = state.embedder.embed(f) => r?,
            _ = state.cancel_token.cancelled() => {
                state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
                return Err(Error::Cancelled);
            }
        });
    }

    // ── Phase B: async dedup (no lock) ────────────────────────────────────────
    if state.cancel_token.is_cancelled() {
        state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
        return Err(Error::Cancelled);
    }
    // Fetch cosine candidates in a blocking pass, then verify each with DedupAdapter.
    // `db_shared` was already loaded above, before edge validation (#209).
    let gid_b = group_id.to_string();
    let db_b = Arc::clone(&db_shared);
    let entity_count = tokio::task::spawn_blocking(move || {
        let conn = db_b.connect()?;
        conn.entity_count_in_group(&gid_b)
    })
    .await??;

    let use_hybrid = entity_count >= hybrid_threshold();
    // Above the hybrid-dedup threshold, this query depends on entity_name_embedding_idx (and the
    // FTS indexes hybrid_dedup_similar_entity also uses). On a workspace where nothing has ever
    // built those indices, retry once via the same missing-index auto-heal the search handlers
    // use (ADR-0025) rather than failing the chunk (#208).
    let phase_b_attempt = resolve_phase_b(
        Arc::clone(&db_shared),
        group_id.to_string(),
        entity_names.clone(),
        name_embeddings.clone(),
        use_hybrid,
    )
    .await;
    let phase_b_results: Vec<PhaseBResult> = match phase_b_attempt {
        Ok(results) => results,
        Err(e) if is_missing_index_error(&e) => {
            if state.indices_built.load(Ordering::Acquire) {
                // Indices are supposedly built but the query still failed this way — a redundant
                // rebuild wouldn't help (mirrors the search handlers' identical quirk).
                return Err(Error::Ipc(MISSING_INDEX_USER_MSG.to_string()));
            }
            build_indices_once(&state).await?;
            // Reload the db in case a concurrent clear_all swapped it while we were building.
            let db_retry = load_db(&state)?;
            resolve_phase_b(
                db_retry,
                group_id.to_string(),
                entity_names.clone(),
                name_embeddings.clone(),
                use_hybrid,
            )
            .await
            .map_err(|e2| {
                if is_missing_index_error(&e2) {
                    Error::Ipc(MISSING_INDEX_USER_MSG.to_string())
                } else {
                    e2
                }
            })?
        }
        Err(e) => return Err(e),
    };

    // Async dedup verification loop (no lock)
    let mut decisions: Vec<DedupDecision> = Vec::with_capacity(extraction.entities.len());
    let ref_time_owned = reference_time.to_string();
    let gid_owned = group_id.to_string();
    for (i, extracted) in extraction.entities.iter().enumerate() {
        if state.cancel_token.is_cancelled() {
            state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Cancelled);
        }
        let make_insert_row = |name_embedding: Vec<f32>| DedupDecision::Insert {
            row: EntityRow {
                uuid: uuid::Uuid::new_v4().to_string(),
                name: extracted.name.clone(),
                group_id: gid_owned.clone(),
                labels: {
                    let mut labels = vec!["Entity".to_string()];
                    if !extracted.entity_type.is_empty() && extracted.entity_type != "Entity" {
                        if let Some(ancestors) =
                            ontology_ref.and_then(|o| o.ancestor_map.get(&extracted.entity_type))
                        {
                            labels.extend(ancestors.iter().cloned());
                        }
                        labels.push(extracted.entity_type.clone());
                    }
                    labels
                },
                created_at: ref_time_owned.clone(),
                name_embedding,
                summary: extracted.summary.clone(),
                attributes: "{}".to_string(),
                episode_uuids: vec![],
                source_descriptions: vec![],
            },
        };
        let decision = match &phase_b_results[i] {
            PhaseBResult::NameMatch { existing } => {
                // Exact name match — resolve immediately, no dedup-adapter check needed.
                if !extracted.entity_type.is_empty()
                    && extracted.entity_type != "Entity"
                    && !existing.labels.contains(&extracted.entity_type)
                {
                    eprintln!(
                        "liminis-context-graph: entity resolution: type conflict for '{}': \
                         existing labels {:?}, extracted type '{}'",
                        existing.name, existing.labels, extracted.entity_type
                    );
                }
                DedupDecision::Merge {
                    existing_uuid: existing.uuid.clone(),
                    merged_summary: format!("{} {}", existing.summary, extracted.summary),
                }
            }
            PhaseBResult::EmbeddingCandidate {
                candidate: Some(existing),
            } => {
                let is_dup = tokio::select! {
                    r = state.dedup.is_duplicate(existing, extracted) => r?,
                    _ = state.cancel_token.cancelled() => {
                        state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
                        return Err(Error::Cancelled);
                    }
                };
                if is_dup {
                    DedupDecision::Merge {
                        existing_uuid: existing.uuid.clone(),
                        merged_summary: format!("{} {}", existing.summary, extracted.summary),
                    }
                } else {
                    make_insert_row(name_embeddings[i].clone())
                }
            }
            PhaseBResult::EmbeddingCandidate { candidate: None } => {
                make_insert_row(name_embeddings[i].clone())
            }
        };
        decisions.push(decision);
    }

    // Capture counts before extraction moves into the Phase C closure. `edges_extracted` is
    // no longer precomputed here — it's now the actual insert count Phase C returns, since
    // Phase C is the sole point that finally resolves or drops an edge (FR-004, FR-005).
    let nodes_extracted = extraction.entities.len();

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

    let wal_writer_c = Arc::clone(&state.wal_writer);
    let sink_c = Arc::clone(&state.sink);
    // Guard stays in async scope; spawn_blocking completes while it is held.
    // tokio::sync::RwLockWriteGuard is not 'static so it cannot move into the closure.
    // Cancellation is checked here — once the write guard is acquired the commit runs to
    // completion (FR-003: Phase C must not be torn mid-write).
    let _write_guard = tokio::select! {
        g = state.write_lock.write() => g,
        _ = state.cancel_token.cancelled() => {
            state.cancelled_chunks.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Cancelled);
        }
    };
    let (edges_inserted, edges_dropped_unresolvable) = tokio::task::spawn_blocking(
        move || -> Result<(usize, usize), Error> {
        let conn = db_c.connect()?;
        let mut edges_inserted = 0usize;
        let mut edges_dropped_unresolvable = 0usize;

        // Apply dedup decisions → collect entity UUIDs
        let mut entity_uuids: Vec<String> = Vec::with_capacity(decisions.len());
        for decision in decisions {
            match decision {
                DedupDecision::Merge {
                    existing_uuid,
                    merged_summary,
                } => {
                    conn.exec_params(
                        "MATCH (e:Entity {uuid: $uuid}) SET e.summary = $summary",
                        serde_json::json!({ "uuid": &existing_uuid, "summary": &merged_summary }),
                    )?;
                    entity_uuids.push(existing_uuid);
                }
                DedupDecision::Insert { row } => {
                    let uuid = row.uuid.clone();
                    conn.insert_entity(&row)?;
                    entity_uuids.push(uuid);
                }
            }
        }

        // name→uuid map for edge endpoint resolution. Keys are lowercase-normalized so a
        // batch-internal case mismatch between an entity's extracted name and an edge's
        // endpoint name doesn't fall through to the global fallback unnecessarily (#209).
        let name_to_uuid: std::collections::HashMap<String, String> = extraction
            .entities
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.trim().to_lowercase(), entity_uuids[i].clone()))
            .collect();

        // Insert relationship edges. This is the sole, authoritative point at which an edge's
        // endpoints are finally resolved or the edge is dropped (FR-003, FR-005) — pre-lock,
        // above, only salvage-rewrites an off-list endpoint name; it never drops for endpoint
        // reasons (except the always-correct self-referential case).
        for (i, edge) in extraction.edges.iter().enumerate() {
            // An endpoint absent from this batch's name→uuid map may still resolve against the
            // persisted Entity table (e.g. a recurring hub entity created in an earlier ingest
            // batch that survived Site 1's validation via the same fallback) (FR-002, FR-003).
            let src_uuid = match name_to_uuid.get(&edge.source_name.trim().to_lowercase()) {
                Some(u) => Some(u.clone()),
                None => conn
                    .get_entity_by_name_ci(&edge.source_name, &gid_owned)?
                    .map(|existing| existing.uuid),
            };
            let dst_uuid = match name_to_uuid.get(&edge.target_name.trim().to_lowercase()) {
                Some(u) => Some(u.clone()),
                None => conn
                    .get_entity_by_name_ci(&edge.target_name, &gid_owned)?
                    .map(|existing| existing.uuid),
            };
            let (src_uuid, dst_uuid) = match (src_uuid, dst_uuid) {
                (Some(s), Some(d)) => (s, d),
                (src, dst) => {
                    eprintln!(
                        "liminis-context-graph: dropping edge at commit, unresolvable endpoint: '{}' → '{}' (src_resolved={}, dst_resolved={})",
                        edge.source_name, edge.target_name, src.is_some(), dst.is_some()
                    );
                    edges_dropped_unresolvable += 1;
                    continue;
                }
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
                valid_at: validate_llm_timestamp(edge.valid_at.clone())
                    .or_else(|| Some(ref_time_owned.clone())),
                invalid_at: validate_llm_timestamp(edge.invalid_at.clone()),
                attributes: "{}".to_string(),
                relation_type: edge.relation_type.clone(),
                episode_uuids: vec![],
                source_descriptions: vec![],
            })?;
            edges_inserted += 1;
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

        wal_exec::wal_flush_chunk(&wal_writer_c, conn.drain_mutations(), &sink_c);

        Ok((edges_inserted, edges_dropped_unresolvable))
    })
    .await??;
    drop(_write_guard);

    // After a successful DB commit, persist the current ontology hash to `.lcg/ontology-hash.json`
    // and clear the drift flag. Errors are non-fatal — a missed write means drift stays reported
    // until the next successful ingest.
    if let Some(ref root) = state.workspace_root {
        let ontology_ref = state.ontology.as_deref();
        if let Err(e) = ontology_sidecar::write_sidecar(root, ontology_ref) {
            eprintln!(
                "liminis-context-graph: ontology-sidecar: failed to update {:?}: {} — drift indicator may persist",
                root, e
            );
        } else if let Ok(mut guard) = state.ontology_drift.lock() {
            *guard = OntologyDriftState::default();
        }
    }

    Ok(AddEpisodeResult {
        episode_uuid,
        nodes_extracted,
        edges_extracted: edges_inserted,
        edges_dropped_unresolvable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-007: a genuine (non-missing-index) error from the hybrid dedup query must be
    /// distinguishable from a missing-index error, since `add_episode`'s retry logic only
    /// triggers `build_indices_once` when `is_missing_index_error` returns true — anything
    /// else takes the `Err(e) => return Err(e)` arm and propagates immediately, un-retried.
    ///
    /// `resolve_phase_b` is private, so this is exercised directly here rather than through
    /// the public `add_episode` API (there is no clean way to force a genuine DB error through
    /// the full ingest pipeline without reaching into internals — see #208 Plan).
    #[tokio::test]
    async fn resolve_phase_b_genuine_error_is_not_classified_as_missing_index() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("phase_b_error_test.db");
        let dim = 8;
        let db = Arc::new(crate::db::Db::open(db_path.to_str().unwrap()).unwrap());
        {
            let conn = db.connect().unwrap();
            conn.init_schema(dim).unwrap();
            // Build indices for real, so a subsequent failure here cannot be a missing-index
            // error — isolating the "genuine DB error" case FR-007 is about.
            conn.build_indices_and_constraints().unwrap();
        }

        // A name_embedding of the wrong dimension (dim+1 instead of dim) against an
        // already-indexed table is a genuine query failure, not a missing-index condition.
        let wrong_dim_embedding = vec![0.0f32; dim + 1];
        let result = resolve_phase_b(
            db,
            "test-group".to_string(),
            vec!["Someone".to_string()],
            vec![wrong_dim_embedding],
            true, // use_hybrid
        )
        .await;

        let err = match result {
            Ok(_) => panic!("dimension-mismatched vector query should fail"),
            Err(e) => e,
        };
        assert!(
            !is_missing_index_error(&err),
            "a dimension-mismatch error must not be classified as a missing-index error \
             (FR-007: only missing-index errors trigger auto-heal), got: {err}"
        );
    }
}
