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
//! is gone), then rebuild the index.
//!
//! For a real (non-dry-run) run, the write lock is acquired *before* Phase A's candidate read and
//! held continuously through Phase C's rebuild — not released in between, and not released
//! between Phase C's batches either (unlike `backfill_relation_types`). Holding it across Phase A
//! too (rather than the cheaper read lock a dry run uses) closes a TOCTOU window: without it, a
//! concurrent `knowledge_assert_entity` re-assert could change `summary` after Phase A captured
//! it but before Phase C writes the embedding, silently persisting an embedding of stale text.
//!
//! `state.indices_built` is set to `false` before the index is dropped and back to `true` once
//! the rebuild succeeds — mirroring `handle_rebuild_from_wal`'s exact bookkeeping
//! (`handlers.rs`'s `bg_indices_built.store(false, ...)` before its own drop/replay/rebuild
//! cycle). This is *not* redundant with holding `write_lock`: the hot-path search handlers
//! (`knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_search_passages`) never
//! take `write_lock` for their read queries, so a concurrent search genuinely can race in while
//! the index is dropped. What makes that race safe is `indices_built`: a search that hits
//! `is_missing_index_error` while `indices_built` is `false` calls `build_indices_once`, which
//! itself blocks on `write_lock.write()` until this pass releases it, then finds `indices_built`
//! already `true` and returns immediately, letting the search retry succeed. Without setting the
//! flag, `indices_built` would stay `true` throughout, and a search hitting the race would take
//! the *other* branch (`return Err(missing index)`) instead of auto-healing — a hard, user-visible
//! failure despite this pass ostensibly self-healing (see `ADR-0025`, `ADR-0036`). If the process
//! crashes mid-backfill (index dropped, `indices_built` never reset to `true`), the next read's
//! auto-heal path self-heals it via the same idempotent `CREATE_VECTOR_INDEX` call.
//!
//! Callers must add `knowledge_backfill_summary_embeddings` to `service_protocol.py` in the
//! liminis-app repo.
use std::sync::atomic::Ordering;
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
    // ── Phase A: paginated read of all Entity rows with a non-empty summary ──────────────────
    // A dry run only needs a consistent snapshot briefly, so it takes the cheaper read lock. A
    // real run holds the write lock from here through Phase C's rebuild, so a concurrent
    // `knowledge_assert_entity` re-assert can't change `summary` in the gap between this read and
    // Phase C's write of the embedding computed from it (see module doc).
    let db = state
        .db
        .load_full()
        .ok_or_else(|| Error::DbUnavailable("DB unavailable".to_string()))?;
    let write_guard = if !params.dry_run {
        Some(state.write_lock.write().await)
    } else {
        None
    };
    let _read_guard = if write_guard.is_none() {
        Some(state.write_lock.read().await)
    } else {
        None
    };
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
                    // name_embedding, summary, attributes, summary_embedding] — summary is
                    // index 6; the appended summary_embedding (index 8) is unused here.
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

    // ── Phase C: drop index → batched embed+SET → rebuild index (write lock held since Phase A) ─
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(json!({
            "type": "progress",
            "phase": "writing",
            "total_mutations": backfill_count,
        }));
    }

    // Mirrors `handle_rebuild_from_wal`'s bookkeeping: cleared before the index is actually
    // dropped, so a concurrent search that races in and hits a missing-index error takes the
    // auto-heal branch (`build_indices_once`, which blocks on `write_lock` until this pass
    // releases it) instead of a hard failure — see module doc.
    // Mirrors `handle_rebuild_from_wal`'s bookkeeping: cleared before the index is actually
    // dropped, so a concurrent search that races in and hits a missing-index error takes the
    // auto-heal branch (`build_indices_once`, which blocks on `write_lock` until this pass
    // releases it) instead of a hard failure — see module doc.
    state.indices_built.store(false, Ordering::Release);

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
    state.indices_built.store(true, Ordering::Release);

    drop(write_guard);

    Ok(BackfillReport {
        group_id: params.group_id.clone(),
        total_entities,
        backfilled: backfill_count,
        dry_run: false,
    }
    .to_json())
}
