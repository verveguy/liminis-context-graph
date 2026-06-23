//! Additive backfill pass — sets `relation_type` on edges where it is currently empty or null.
//!
//! This pass is safe to run at any time and is idempotent:
//!   - It NEVER overwrites a populated `relation_type`.
//!   - It NEVER deletes any edge.
//!   - A second run on an already-backfilled graph produces zero additional WAL mutations.
//!
//! The derivation algorithm (see [`derive_relation_type`]) also powers the extractor's
//! going-forward fallback in `extractor.rs` (via [`derive_relation_type_from_fact`]).
//!
//! Concurrent safety: the SET CQL includes a WHERE guard so a `relation_type` set by a
//! concurrent episode ingestion between Phase A (read) and Phase C (write) is never overwritten.
//!
//! Progress streaming: pass a `progress_tx` and include `_progress_token` in the IPC request
//! to receive `{"type":"progress", ...}` events during a large backfill pass.
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    app_state::AppState, db::value_as_string, error::Error, ontology::normalize_relation_type,
    wal_exec,
};

const PAGE_SIZE: usize = 500;
const WRITE_BATCH_SIZE: usize = 250;
const PROGRESS_EVERY: usize = 5000;

// ── Public types ──────────────────────────────────────────────────────────────

pub struct BackfillParams {
    pub dry_run: bool,
}

pub struct BackfillReport {
    pub total_edges: usize,
    pub backfilled: usize,
    pub dry_run: bool,
}

impl BackfillReport {
    fn to_json(&self) -> Value {
        json!({
            "total_edges": self.total_edges,
            "backfilled": self.backfilled,
            "dry_run": self.dry_run,
        })
    }
}

// ── Derivation algorithm ──────────────────────────────────────────────────────

/// Derives a `relation_type` from the first 4 words of a fact string.
///
/// Used by the extractor fallback (when the LLM omits `relation_type`) and by
/// the backfill pass when an edge's `name` is an arrow pattern.
///
/// Returns `"UNCLASSIFIED"` when `fact` is empty or yields no normalizable words.
pub(crate) fn derive_relation_type_from_fact(fact: &str) -> String {
    let truncated = fact
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ");
    let normalized = normalize_relation_type(&truncated);
    if normalized.is_empty() {
        "UNCLASSIFIED".to_string()
    } else {
        normalized
    }
}

/// Derives a `relation_type` for a backfill candidate edge.
///
/// Strategy:
/// 1. If `name` is a plain predicate (not an arrow pattern), normalize and use it.
/// 2. Otherwise fall back to [`derive_relation_type_from_fact`] using the `fact` field.
///
/// Arrow detection uses a simple substring check for `→` or `->` so it catches mixed-case
/// extractor-produced names ("Brett Adam → Seattle") that the canonicalize noise regex misses.
pub(crate) fn derive_relation_type(name: &str, fact: &str) -> String {
    if !name.is_empty() && !name.contains('→') && !name.contains("->") {
        let normalized = normalize_relation_type(name);
        if !normalized.is_empty() {
            return normalized;
        }
    }
    derive_relation_type_from_fact(fact)
}

// ── Candidate record ──────────────────────────────────────────────────────────

struct EdgeCandidate {
    uuid: String,
    name: String,
    fact: String,
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Runs the backfill pass as a three-phase async operation.
///
/// Callers must add `knowledge_backfill_relation_types` to `service_protocol.py`
/// in the liminis-app repo.
pub async fn backfill_relation_types(
    state: Arc<AppState>,
    params: BackfillParams,
    progress_tx: Option<UnboundedSender<Value>>,
) -> Result<Value, Error> {
    // ── Phase A: paginated read of all RelatesToNode_ edges (read lock) ───────
    let db = state
        .db
        .load_full()
        .ok_or_else(|| Error::DbUnavailable("DB unavailable".to_string()))?;
    let _read_guard = state.write_lock.read().await;
    let db_a = Arc::clone(&db);

    let (total_edges, candidates): (usize, Vec<EdgeCandidate>) =
        tokio::task::spawn_blocking(move || {
            let conn = db_a.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
            let mut all_candidates = Vec::new();
            let mut total = 0usize;
            let mut offset = 0;
            loop {
                let rows = conn
                    .dump_relatos_page(None, offset, PAGE_SIZE)
                    .map_err(|e| Error::Ipc(format!("read edges page: {e}")))?;
                let count = rows.len();
                for row in &rows {
                    total += 1;
                    let rt_str = value_as_string(&row[11]);
                    if rt_str.is_empty() {
                        all_candidates.push(EdgeCandidate {
                            uuid: value_as_string(&row[0]),
                            name: value_as_string(&row[1]),
                            fact: value_as_string(&row[4]),
                        });
                    }
                }
                if count < PAGE_SIZE {
                    break;
                }
                offset += count;
            }
            Ok::<_, Error>((total, all_candidates))
        })
        .await??;
    drop(_read_guard);

    let backfill_count = candidates.len();

    // ── Phase B: dry_run early return ─────────────────────────────────────────
    if params.dry_run {
        return Ok(BackfillReport {
            total_edges,
            backfilled: backfill_count,
            dry_run: true,
        }
        .to_json());
    }

    // ── Phase C: batched write lock (ADR-0051: 250-edge batches) ─────────────
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(json!({
            "type": "progress",
            "phase": "writing",
            "total_mutations": backfill_count,
        }));
    }

    for (batch_idx, batch) in candidates.chunks(WRITE_BATCH_SIZE).enumerate() {
        let batch_data: Vec<(String, String)> = batch
            .iter()
            .map(|e| (e.uuid.clone(), derive_relation_type(&e.name, &e.fact)))
            .collect();

        let db_c = Arc::clone(&db);
        let wal_writer = Arc::clone(&state.wal_writer);
        let sink = Arc::clone(&state.sink);
        let _write_guard = state.write_lock.write().await;

        let processed = batch_idx * WRITE_BATCH_SIZE;
        if processed > 0 && processed % PROGRESS_EVERY == 0 {
            if let Some(ref tx) = progress_tx {
                let _ = tx.send(json!({
                    "type": "progress",
                    "processed": processed,
                    "total": backfill_count,
                    "phase": "writing",
                }));
            }
        }

        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let conn = db_c.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
            for (uuid, rt) in &batch_data {
                // WHERE guard: prevents overwriting a relation_type set by concurrent ingestion
                // between Phase A and Phase C. Safe to WAL-record even if WHERE matches 0 rows
                // (replay is a no-op when relation_type is already populated).
                conn.exec_params(
                    "MATCH (n:RelatesToNode_ {uuid: $uuid}) \
                     WHERE n.relation_type IS NULL OR n.relation_type = '' \
                     SET n.relation_type = $rt",
                    json!({ "uuid": uuid, "rt": rt }),
                )?;
            }
            wal_exec::wal_flush_ungrouped(&wal_writer, conn.drain_mutations(), &sink);
            Ok(())
        })
        .await??;
        drop(_write_guard);
    }

    Ok(BackfillReport {
        total_edges,
        backfilled: backfill_count,
        dry_run: false,
    }
    .to_json())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_relation_type_from_fact_basic() {
        let rt = derive_relation_type_from_fact("Brett Adam lives in Seattle");
        assert!(!rt.is_empty(), "must not be empty");
        assert!(!rt.contains('→'), "must not contain →");
        assert!(!rt.contains("->"), "must not contain ->");
        // Normalized to SCREAMING_SNAKE_CASE — all uppercase letters and underscores
        assert!(
            rt.chars().all(|c| c.is_uppercase() || c == '_'),
            "must be SCREAMING_SNAKE_CASE: {rt}"
        );
    }

    #[test]
    fn test_derive_relation_type_from_fact_empty() {
        assert_eq!(
            derive_relation_type_from_fact(""),
            "UNCLASSIFIED",
            "empty fact must yield UNCLASSIFIED"
        );
    }

    #[test]
    fn test_derive_relation_type_from_fact_truncates_to_4_words() {
        // "Alice attended the meeting yesterday" → first 4 words → "Alice attended the meeting"
        // normalized → ALICE_ATTENDED_THE_MEETING (4 words, not 5)
        let rt = derive_relation_type_from_fact("Alice attended the meeting yesterday");
        assert!(!rt.contains("YESTERDAY"), "must not include 5th word");
    }

    #[test]
    fn test_derive_relation_type_plain_name() {
        // Plain predicate name (not an arrow) should be normalized and used
        let rt = derive_relation_type("ATTENDED", "Alice attended the meeting");
        assert_eq!(rt, "ATTENDED");
    }

    #[test]
    fn test_derive_relation_type_arrow_falls_back_to_fact() {
        // Arrow-pattern name should trigger fact fallback
        let rt = derive_relation_type("Brett → Seattle", "Brett lives in Seattle");
        assert!(!rt.is_empty(), "must not be empty");
        assert!(!rt.contains('→'), "must not contain →");
    }

    #[test]
    fn test_derive_relation_type_mixed_case_arrow() {
        // Mixed-case arrow (extractor-produced) should also trigger fact fallback
        let rt = derive_relation_type(
            "Brett Adam → PSET Architecture Council",
            "Brett Adam participated in PSET Architecture Council",
        );
        assert!(!rt.is_empty());
        assert!(!rt.contains('→'));
    }

    #[test]
    fn test_derive_relation_type_empty_name_uses_fact() {
        let rt = derive_relation_type("", "Alice knows Bob");
        assert!(!rt.is_empty());
    }
}
