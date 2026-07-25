//! Fact-based LLM relation classification pass (issue #210) — the relation-side twin of
//! `handlers::handle_reprocess_entity_types`.
//!
//! For each in-scope edge, asks the configured extraction LLM to pick exactly one of the
//! ontology's declared relation types, based solely on the edge's `fact` text — or to honestly
//! abstain. Unlike `canonicalize.rs` (lexical/embedding match against the edge's predicate) and
//! `backfill.rs` (mints a fact-prefix pseudo-type), this pass never force-assigns a nearest
//! match: an abstention is written as the literal sentinel `UNCLASSIFIED`, a real WAL-durable
//! mutation rather than a no-op. See ADR-0037 for why this diverges from entity classification's
//! abstain-means-skip behavior.
//!
//! Crash mid-pass: only committed write batches appear in the WAL. Re-running the same scope is
//! idempotent — a verdict identical to the edge's current `relation_type` produces no write.
use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    app_state::AppState, corrections, db::value_as_string, error::Error, ontology::Ontology,
    wal_exec,
};

const PAGE_SIZE: usize = 500;
const WRITE_BATCH_SIZE: usize = 250;
const PROGRESS_EVERY: usize = 5000;

/// Sentinel written when the LLM honestly cannot map an edge's fact to any declared type.
pub const UNCLASSIFIED: &str = "UNCLASSIFIED";

// ── Public types ──────────────────────────────────────────────────────────────

/// Scope of edges targeted by `reprocess_relation_types`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationReprocessScope {
    /// Only edges with no relation_type (NULL or empty string). Default.
    Untyped,
    /// Untyped edges plus edges whose relation_type is not a declared ontology type
    /// (covers prior UNCLASSIFIED sentinels and backfill's fact-prefix pseudo-types).
    OffOntology,
    /// Every edge in the group, regardless of current relation_type.
    All,
}

pub struct ReprocessRelationsParams {
    pub group_id: String,
    pub scope: RelationReprocessScope,
    pub dry_run: bool,
}

struct EdgeCandidate {
    uuid: String,
    fact: String,
    current_type: Option<String>,
}

// ── Candidate-selection helpers ──────────────────────────────────────────────

/// FR-004: an edge is untyped when `relation_type IS NULL OR relation_type = ''` — the same
/// predicate `backfill_relation_types` uses.
fn is_untyped(rt: &str) -> bool {
    rt.trim().is_empty()
}

/// FR-005: an edge qualifies for `off_ontology` when untyped, or when its relation_type is
/// non-empty but absent from the declared ontology type-name set. This naturally covers
/// `UNCLASSIFIED` sentinels and fact-prefix pseudo-types with no special-casing (A3).
fn is_off_ontology(rt: &str, type_names: &HashSet<String>) -> bool {
    is_untyped(rt) || !type_names.contains(rt)
}

/// Collects candidate edges for `reprocess_relation_types` based on `scope`, paging through
/// all edges in `group_id` under the caller-held read lock.
fn list_edges_for_scope(
    conn: &crate::db::Conn,
    group_id: &str,
    scope: RelationReprocessScope,
    ontology_type_names: &HashSet<String>,
) -> Result<Vec<EdgeCandidate>, Error> {
    let mut all = Vec::new();
    let mut offset = 0;
    loop {
        let rows = conn
            .dump_relatos_page(Some(group_id), offset, PAGE_SIZE)
            .map_err(|e| Error::Ipc(format!("read edges page: {e}")))?;
        let count = rows.len();
        for row in &rows {
            let rt_str = value_as_string(&row[11]);
            let include = match scope {
                RelationReprocessScope::Untyped => is_untyped(&rt_str),
                RelationReprocessScope::OffOntology => {
                    is_off_ontology(&rt_str, ontology_type_names)
                }
                RelationReprocessScope::All => true,
            };
            if include {
                all.push(EdgeCandidate {
                    uuid: value_as_string(&row[0]),
                    fact: value_as_string(&row[4]),
                    current_type: if rt_str.is_empty() {
                        None
                    } else {
                        Some(rt_str)
                    },
                });
            }
        }
        if count < PAGE_SIZE {
            break;
        }
        offset += count;
    }
    Ok(all)
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Runs the relation reprocessing pass as a phase-split async operation.
///
/// Caller (`handlers::handle_reprocess_relation_types`) has already verified the ontology
/// declares at least one relation type (FR-002) for every scope value.
pub async fn reprocess_relation_types(
    state: Arc<AppState>,
    params: ReprocessRelationsParams,
    progress_tx: Option<UnboundedSender<Value>>,
    ontology: Arc<Ontology>,
) -> Result<Value, Error> {
    let ontology_type_names = ontology.relation_type_names();

    // Build the allowed_types menu (sorted for deterministic prompts).
    let mut allowed_types: Vec<(String, Option<String>)> = ontology
        .relation_types
        .iter()
        .map(|rt| (rt.name.clone(), rt.description.clone()))
        .collect();
    allowed_types.sort_by(|a, b| a.0.cmp(&b.0));

    // ── Phase A (read lock): collect candidate edges based on scope ──────────
    let db = state
        .db
        .load_full()
        .ok_or_else(|| Error::DbUnavailable("DB unavailable".to_string()))?;
    let group_id_a = params.group_id.clone();
    let scope = params.scope;
    let type_names_a = ontology_type_names.clone();
    let _read_guard = state.write_lock.read().await;
    let db_a = Arc::clone(&db);
    let candidates: Vec<EdgeCandidate> = tokio::task::spawn_blocking(move || {
        let conn = db_a.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        list_edges_for_scope(&conn, &group_id_a, scope, &type_names_a)
    })
    .await??;
    drop(_read_guard);

    if candidates.is_empty() {
        if params.dry_run {
            return Ok(json!({
                "would_reclassify_count": 0,
                "plan": [],
                "breakdown": {},
            }));
        }
        return Ok(json!({
            "success": true,
            "reclassified_count": 0,
            "unchanged_count": 0,
            "group_id": params.group_id,
        }));
    }

    // ── Phase B (no lock): classify edges via LLM in batches ─────────────────
    let pairs: Vec<(String, String)> = candidates
        .iter()
        .map(|c| (c.fact.clone(), c.current_type.clone().unwrap_or_default()))
        .collect();

    let total = candidates.len();
    let mut verdicts: Vec<String> = Vec::with_capacity(total);
    for (i, chunk) in pairs.chunks(corrections::REPROCESS_BATCH_SIZE).enumerate() {
        let processed = i * corrections::REPROCESS_BATCH_SIZE;
        if processed > 0 && processed.is_multiple_of(PROGRESS_EVERY) {
            if let Some(ref tx) = progress_tx {
                let _ = tx.send(json!({
                    "type": "progress",
                    "processed": processed,
                    "total": total,
                    "phase": "classifying",
                }));
            }
        }
        let refs: Vec<(&str, &str)> = chunk
            .iter()
            .map(|(fact, current)| (fact.as_str(), current.as_str()))
            .collect();
        match state
            .extractor
            .classify_relations(&refs, &allowed_types)
            .await
        {
            Ok(mut batch) => {
                batch.resize(chunk.len(), String::new());
                verdicts.extend(batch);
            }
            Err(e) => {
                return Ok(json!({
                    "success": false,
                    "group_id": params.group_id,
                    "error": format!("Failed to reprocess relation types: {e}"),
                }));
            }
        }
    }

    // ── Build plan + updates, applying idempotency (FR-009) ──────────────────
    let mut plan: Vec<Value> = Vec::new();
    let mut updates: Vec<(String, String)> = Vec::new();
    let mut unchanged_count: usize = 0;
    let mut breakdown: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for (edge, verdict) in candidates.iter().zip(verdicts.iter()) {
        // FR-008: abstention (empty string) resolves to the literal UNCLASSIFIED sentinel — a
        // real write, unlike entity classification's abstain-means-skip (A4).
        let new_type = if verdict.is_empty() {
            UNCLASSIFIED.to_string()
        } else {
            verdict.clone()
        };
        if edge.current_type.as_deref() == Some(new_type.as_str()) {
            unchanged_count += 1;
            continue;
        }
        plan.push(json!({
            "edge_id": edge.uuid,
            "fact": edge.fact,
            "old_type": edge.current_type,
            "new_type": new_type,
        }));
        *breakdown.entry(new_type.clone()).or_insert(0) += 1;
        updates.push((edge.uuid.clone(), new_type));
    }

    if params.dry_run {
        return Ok(json!({
            "would_reclassify_count": plan.len(),
            "plan": plan,
            "breakdown": breakdown,
        }));
    }

    // ── Phase C (batched write lock per ADR-0030) ─────────────────────────────
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(json!({
            "type": "progress",
            "phase": "writing",
            "total_mutations": updates.len(),
        }));
    }

    let mut reclassified = 0usize;
    for (batch_idx, batch) in updates.chunks(WRITE_BATCH_SIZE).enumerate() {
        let batch = batch.to_vec();
        let db_c = Arc::clone(&db);
        let wal_writer_c = Arc::clone(&state.wal_writer);
        let sink_c = Arc::clone(&state.sink);
        let _write_guard = state.write_lock.write().await;

        let processed = batch_idx * WRITE_BATCH_SIZE;
        if processed > 0 && processed.is_multiple_of(PROGRESS_EVERY) {
            if let Some(ref tx) = progress_tx {
                let _ = tx.send(json!({
                    "type": "progress",
                    "processed": processed,
                    "total": updates.len(),
                    "phase": "writing",
                }));
            }
        }

        let count = tokio::task::spawn_blocking(move || -> Result<usize, Error> {
            let conn = db_c.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
            for (uuid, rt) in &batch {
                conn.exec_params(
                    "MATCH (n:RelatesToNode_ {uuid: $uuid}) SET n.relation_type = $rt",
                    json!({ "uuid": uuid, "rt": rt }),
                )?;
            }
            wal_exec::wal_flush_ungrouped(&wal_writer_c, conn.drain_mutations(), &sink_c);
            Ok(batch.len())
        })
        .await??;
        reclassified += count;
        drop(_write_guard);
    }

    Ok(json!({
        "success": true,
        "reclassified_count": reclassified,
        "unchanged_count": unchanged_count,
        "group_id": params.group_id,
    }))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn type_names(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // ── is_untyped ────────────────────────────────────────────────────────────

    #[test]
    fn is_untyped_null_maps_to_empty_string() {
        // value_as_string maps NULL to "" — is_untyped must treat that as untyped.
        assert!(is_untyped(""));
    }

    #[test]
    fn is_untyped_whitespace_only_is_untyped() {
        assert!(is_untyped("   "));
    }

    #[test]
    fn is_untyped_declared_type_is_not_untyped() {
        assert!(!is_untyped("AUTHORED"));
    }

    #[test]
    fn is_untyped_unclassified_is_not_untyped() {
        // UNCLASSIFIED is a real (non-empty) value — off_ontology, not untyped.
        assert!(!is_untyped("UNCLASSIFIED"));
    }

    // ── is_off_ontology ───────────────────────────────────────────────────────

    #[test]
    fn is_off_ontology_untyped_edge_is_off_ontology() {
        let names = type_names(&["AUTHORED"]);
        assert!(is_off_ontology("", &names));
    }

    #[test]
    fn is_off_ontology_unclassified_sentinel_is_off_ontology() {
        let names = type_names(&["AUTHORED"]);
        assert!(is_off_ontology(UNCLASSIFIED, &names));
    }

    #[test]
    fn is_off_ontology_fact_prefix_pseudo_type_is_off_ontology() {
        let names = type_names(&["AUTHORED"]);
        assert!(is_off_ontology("BRETT_ATTENDED_THE", &names));
    }

    #[test]
    fn is_off_ontology_declared_type_is_not_off_ontology() {
        let names = type_names(&["AUTHORED", "AFFILIATED_WITH"]);
        assert!(!is_off_ontology("AUTHORED", &names));
    }
}
