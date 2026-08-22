//! Backfill pass for issue #470 (FR-005): computes `summary_embedding` for existing `Entity`
//! rows, so entities created before this feature existed become semantically retrievable by
//! summary paraphrase — the documented backfill path SC-004 requires.
//!
//! Unlike `backfill_relation_types`, this pass does not skip already-embedded rows: no cheap
//! "is this already a real embedding, or still the migration's zero-vector placeholder" check
//! exists (decoding a `FLOAT[]` array back out of a query result has no precedent in `db.rs`
//! outside a handful of narrow read paths). On each invocation, every entity in `group_id` with
//! a non-empty `summary` gets rescored. This is deliberate: the operation is explicit,
//! infrequent, and operator-invoked — not a hot path — and re-computing an embedding for an
//! unchanged summary is idempotent in effect even though not idempotent in cost. `dry_run` and
//! progress events give cost visibility before/during a run, exactly like
//! `knowledge_backfill_relation_types`.
//!
//! `summary_embedding` is normally a write-once, HNSW-indexed column (see
//! `Conn::update_entity_core`'s doc comment for the underlying constraint) — no code path can
//! refresh it in place while `entity_summary_embedding_idx` exists; lbug rejects a plain `SET`
//! on an indexed column outright. This pass is the one place `summary_embedding` *can* be
//! refreshed for existing rows, because it owns the index's lifecycle directly for the duration
//! of Phase C: drop the index, batch-write real embeddings via plain `SET` (legal once the index
//! is gone), then rebuild the index. The write lock is held for the *entire* Phase C sequence —
//! not released between batches, unlike `backfill_relation_types` — so no concurrent read can
//! observe the index in its dropped state and race the auto-heal path
//! (`is_missing_index_error` → `build_indices_once`) against this pass's own rebuild. If the
//! process crashes mid-backfill (index dropped, not yet rebuilt), the next read's auto-heal path
//! self-heals it via the same idempotent `CREATE_VECTOR_INDEX` call — no extra recovery needed.
//!
//! Callers must add `knowledge_backfill_summary_embeddings` to `service_protocol.py` in the
//! liminis-app repo.
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::{app_state::AppState, db::value_as_string, error::Error, wal_exec};

const PAGE_SIZE: usize = 500;
const WRITE_BATCH_SIZE: usize = 100;
const PROGRESS_EVERY: usize = 1000;

// ── Public types ──────────────────────────────────────────────────────────────

pub struct BackfillParams {
    pub group_id: String,
    pub dry_run: bool,
}

pub struct BackfillReport {
    pub group_id: String,
    pub total_entities: usize,
    pub backfilled: usize,
    pub dry_run: bool,
}

impl BackfillReport {
    fn to_json(&self) -> Value {
        json!({
            "group_id": self.group_id,
            "total_entities": self.total_entities,
            "backfilled": self.backfilled,
            "dry_run": self.dry_run,
        })
    }
}

// ── Candidate record ────────────────────────────────────────────────────────

struct EntityCandidate {
    uuid: String,
    summary: String,
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Runs the summary-embedding backfill pass as a three-phase operation (issue #470, FR-005).
pub async fn backfill_summary_embeddings(
    state: Arc<AppState>,
    params: BackfillParams,
    progress_tx: Option<UnboundedSender<Value>>,
) -> Result<Value, Error> {
    // ── Phase A: paginated read of all Entity rows with a non-empty summary (read lock) ──────
    let db = state
        .db
        .load_full()
        .ok_or_else(|| Error::DbUnavailable("DB unavailable".to_string()))?;
    let _read_guard = state.write_lock.read().await;
    let db_a = Arc::clone(&db);
    let group_id_a = params.group_id.clone();

    let (total_entities, candidates): (usize, Vec<EntityCandidate>) =
        tokio::task::spawn_blocking(move || {
            let conn = db_a.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
            let mut all_candidates = Vec::new();
            let mut total = 0usize;
            let mut offset = 0;
            loop {
                let rows = conn
                    .dump_entities_page(Some(&group_id_a), offset, PAGE_SIZE)
                    .map_err(|e| Error::Ipc(format!("read entities page: {e}")))?;
                let count = rows.len();
                for row in &rows {
                    total += 1;
                    // dump_entities_page columns: [uuid, name, group_id, labels, created_at,
                    // name_embedding, summary, attributes] — summary is index 6.
                    let summary = value_as_string(&row[6]);
                    if !summary.trim().is_empty() {
                        all_candidates.push(EntityCandidate {
                            uuid: value_as_string(&row[0]),
                            summary,
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
            group_id: params.group_id.clone(),
            total_entities,
            backfilled: backfill_count,
            dry_run: true,
        }
        .to_json());
    }

    if backfill_count == 0 {
        // Nothing to embed — skip the drop/rebuild cycle entirely rather than pay an exclusive
        // lock hold plus an index drop+recreate for a no-op run.
        return Ok(BackfillReport {
            group_id: params.group_id.clone(),
            total_entities,
            backfilled: 0,
            dry_run: false,
        }
        .to_json());
    }

    // ── Phase C: drop index → batched embed+SET → rebuild index, one held write lock ─────────
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(json!({
            "type": "progress",
            "phase": "writing",
            "total_mutations": backfill_count,
        }));
    }

    let _write_guard = state.write_lock.write().await;

    let db_drop = Arc::clone(&db);
    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let conn = db_drop
            .connect()
            .map_err(|e| Error::Ipc(format!("db: {e}")))?;
        conn.drop_entity_summary_embedding_index();
        Ok(())
    })
    .await??;

    let mut processed = 0usize;
    for batch in candidates.chunks(WRITE_BATCH_SIZE) {
        let mut batch_data: Vec<(String, Vec<f32>)> = Vec::with_capacity(batch.len());
        for candidate in batch {
            let emb = state.embedder.embed(&candidate.summary).await?;
            batch_data.push((candidate.uuid.clone(), emb));
        }

        let db_c = Arc::clone(&db);
        let state_c = Arc::clone(&state);
        let gid_c = params.group_id.clone();

        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let conn = db_c.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
            for (uuid, emb) in &batch_data {
                // No WHERE guard (unlike backfill_relation_types): every candidate is
                // unconditionally re-embedded on each run, by design (see module doc).
                conn.exec_params(
                    "MATCH (n:Entity {uuid: $uuid}) SET n.summary_embedding = $emb",
                    json!({ "uuid": uuid, "emb": emb }),
                )?;
            }
            let seq = wal_exec::wal_flush_ungrouped(&state_c, &gid_c, conn.drain_mutations());
            wal_exec::advance_wal_position(&conn, &gid_c, seq);
            Ok(())
        })
        .await??;

        processed += batch.len();
        if let Some(ref tx) = progress_tx {
            if processed.is_multiple_of(PROGRESS_EVERY) || processed == backfill_count {
                let _ = tx.send(json!({
                    "type": "progress",
                    "processed": processed,
                    "total": backfill_count,
                    "phase": "writing",
                }));
            }
        }
    }

    let db_rebuild = Arc::clone(&db);
    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let conn = db_rebuild
            .connect()
            .map_err(|e| Error::Ipc(format!("db: {e}")))?;
        conn.create_entity_summary_embedding_index()
    })
    .await??;

    drop(_write_guard);

    Ok(BackfillReport {
        group_id: params.group_id.clone(),
        total_entities,
        backfilled: backfill_count,
        dry_run: false,
    }
    .to_json())
}
