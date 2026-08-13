use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::{
    app_state::{build_indices_once, load_db, AppState, OntologyDriftState},
    assert, backfill, canonicalize, corrections,
    cross_group::{self, CreateCrossGroupEdgeParams, EndpointSpec},
    db::{self, Db},
    episode,
    error::{is_missing_index_error, is_missing_table_error, Error, MISSING_INDEX_USER_MSG},
    group_purge,
    ipc::{IpcRequest, IpcResponse},
    ontology_sidecar,
    pointer::{self, EndpointSide, PointerStateCounts},
    rebuild_job::{JobStatus, RebuildJob},
    replay::{ProgressFn, ReplayOptions, ReplayProgress, WalReplayer},
    reprocess_relations, search,
    telemetry::{now_ms, TelemetryEvent},
    types::{EntityRow, RelatesToEdge, SourceType},
    wal::WalWriter,
    wal_exec, wal_group,
    wal_group::DEFAULT_GROUP_ID,
};

/// Interval (in processed items) between periodic progress sends for long-running
/// reprocess passes. Mirrors `reprocess_relations::PROGRESS_EVERY`.
const REPROCESS_PROGRESS_EVERY: usize = 5000;

/// Dispatches an IPC request to the appropriate library function. [IPC]
///
/// `progress_tx` is `Some` when the caller detected `_progress_token` in the request params;
/// only `knowledge_rebuild_from_wal` uses it. All other handlers ignore it.
pub async fn dispatch(
    req: IpcRequest,
    state: Arc<AppState>,
    progress_tx: Option<UnboundedSender<Value>>,
) -> IpcResponse {
    let method = req.method.clone();
    let request_id = req.id.clone();
    let start = Instant::now();

    let (response, success) = match handle(&req, Arc::clone(&state), progress_tx).await {
        Ok(result) => (IpcResponse::ok(req.id, result), true),
        Err(e) => {
            use crate::error::Error;
            match e {
                Error::DbUnavailable(ref reason) => (
                    IpcResponse::err_with_data(
                        req.id,
                        -32001,
                        "DB unavailable, recovery required",
                        json!({"reason": reason}),
                    ),
                    false,
                ),
                _ => (IpcResponse::err(req.id, -32000, e.to_string()), false),
            }
        }
    };

    state.sink.emit(TelemetryEvent::IpcCall {
        ts_ms: now_ms(),
        method,
        request_id,
        duration_ms: start.elapsed().as_millis() as u64,
        success,
    });

    response
}

async fn handle(
    req: &IpcRequest,
    state: Arc<AppState>,
    progress_tx: Option<UnboundedSender<Value>>,
) -> Result<Value, Error> {
    // Degraded-mode guard: reject all methods except the recovery-safe subset
    // when the DB is unavailable. See ADR-0009.
    let exempt_in_degraded = matches!(
        req.method.as_str(),
        "health_check"
            | "knowledge_status"
            | "knowledge_recover"
            | "knowledge_recover_full"
            | "knowledge_close"
            // WAL checkpoints (#365, FR-011): list/delete are pure filesystem operations
            // against `.checkpoints/` and must not additionally fail merely because the DB is
            // degraded. `knowledge_wal_mark_create` is deliberately excluded — it depends on
            // applied_seq, which requires an open DB.
            | "knowledge_wal_mark_list"
            | "knowledge_wal_mark_delete"
    );
    if !exempt_in_degraded && state.db.load_full().is_none() {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        return Err(Error::DbUnavailable(reason));
    }

    match req.method.as_str() {
        "health_check" => handle_health_check(state).await,
        "knowledge_status" => handle_knowledge_status(state).await,
        "knowledge_process_chunk" => handle_knowledge_process_chunk(req, state).await,
        "knowledge_add_episode" => handle_add_episode(req, state).await,
        "knowledge_find_entities" => handle_find_entities(req, state).await,
        "knowledge_find_relationships" => handle_find_relationships(req, state).await,
        "knowledge_get_episodes" => handle_get_episodes(req, state).await,
        "knowledge_delete_episode" => handle_delete_episode(req, state).await,
        "knowledge_get_nodes_by_group" => handle_get_nodes_by_group(req, state).await,
        "knowledge_get_edges_by_group" => handle_get_edges_by_group(req, state).await,
        "knowledge_get_edges_by_uuids" => handle_get_edges_by_uuids(req, state).await,
        "knowledge_query_cypher" => handle_query_cypher(req, state).await,
        "knowledge_build_indices" => handle_build_indices(state).await,
        "knowledge_delete_by_source" => handle_delete_by_source(req, state).await,
        "knowledge_delete_chunk_episode" => handle_delete_chunk_episode(req, state).await,
        "knowledge_delete_by_group" => handle_delete_by_group(req, state).await,
        "knowledge_clear_all" => handle_clear_all(req, state).await,
        "knowledge_dump_wal" => handle_dump_wal(req, state).await,
        "knowledge_prepare_checkpoint" => handle_prepare_checkpoint(state).await,
        "knowledge_wal_mark_create" => handle_wal_mark_create(req, state).await,
        "knowledge_wal_mark_list" => handle_wal_mark_list(req, state).await,
        "knowledge_wal_mark_delete" => handle_wal_mark_delete(req, state).await,
        "knowledge_rebuild_from_wal" => handle_rebuild_from_wal(req, state, progress_tx).await,
        "knowledge_rebuild_status" => handle_rebuild_status(req, state).await,
        "knowledge_recover" => handle_knowledge_recover(req, state).await,
        "knowledge_recover_full" => handle_knowledge_recover_full(req, state).await,
        "knowledge_close" => Ok(json!({"status": "closed"})),
        "knowledge_search_passages" => handle_search_passages(req, state).await,
        "knowledge_list_entities" => handle_list_entities(req, state).await,
        "knowledge_list_relationships" => handle_list_relationships(req, state).await,
        "knowledge_get_entity_neighbors" => handle_get_entity_neighbors(req, state).await,
        "knowledge_get_entities_by_source" => handle_get_entities_by_source(req, state).await,
        "knowledge_validate_corrections" => handle_validate_corrections(state).await,
        "knowledge_apply_corrections" => handle_apply_corrections(req, state).await,
        "knowledge_merge_entities" => handle_merge_entities(req, state).await,
        "knowledge_add_cross_group_edge" => handle_add_cross_group_edge(req, state).await,
        "knowledge_rebind_pointers" => handle_rebind_pointers(req, state).await,
        "knowledge_assert_entity" => handle_assert_entity(req, state).await,
        "knowledge_assert_relationship" => handle_assert_relationship(req, state).await,
        "knowledge_reprocess_entity_types" => {
            handle_reprocess_entity_types(req, state, progress_tx).await
        }
        "knowledge_canonicalize_relations" => {
            handle_canonicalize_relations(req, state, progress_tx).await
        }
        "knowledge_backfill_relation_types" => {
            handle_backfill_relation_types(req, state, progress_tx).await
        }
        "knowledge_reprocess_relation_types" => {
            handle_reprocess_relation_types(req, state, progress_tx).await
        }
        _ => Err(Error::Ipc(format!("Method not found: {}", req.method))),
    }
}

async fn handle_add_episode(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let name = p["name"].as_str().unwrap_or("").to_string();
    let body = p["episode_body"].as_str().unwrap_or("").to_string();
    let source = p["source"].as_str().unwrap_or("text").to_string();
    let source_desc = p["source_description"].as_str().unwrap_or("").to_string();
    let ref_time = p["reference_time"].as_str().unwrap_or("").to_string();
    let group_id = p["group_id"]
        .as_str()
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string();

    let source_type = SourceType::from_str_lossy(&source);
    let result = episode::add_episode(
        state,
        &name,
        &body,
        &source,
        &source_desc,
        &ref_time,
        &group_id,
        source_type,
        None,
    )
    .await?;

    Ok(json!({ "episode_uuid": result.episode_uuid }))
}

async fn handle_health_check(state: Arc<AppState>) -> Result<Value, Error> {
    let db_opt = state.db.load_full();
    match db_opt {
        None => {
            let reason = state
                .degraded_reason
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .unwrap_or_else(|| "unknown".to_string());
            Ok(json!({"ok": false, "healthy": false, "state": "degraded", "reason": reason}))
        }
        Some(db) => {
            let _guard = state.write_lock.read().await;
            tokio::task::spawn_blocking(move || {
                let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
                conn.probe().map_err(|e| Error::Ipc(format!("db: {e}")))
            })
            .await??;
            drop(_guard);
            Ok(json!({"ok": true, "healthy": true, "state": "healthy"}))
        }
    }
}

/// Aggregated counts + WAL metadata gathered inside one blocking task, for the healthy
/// (graph queryable) case.
struct StatusFields {
    entity_count: u64,
    episode_count: u64,
    edge_count: u64,
    wal_exists: bool,
    wal_file_count: u64,
    wal_byte_size: u64,
    wal_applied_seq: Option<u64>,
    wal_max_seq: Option<u64>,
    /// Per-group `(group_id, applied_seq, max_seq)` for every group that has a WAL directory
    /// (issue #378 FR-007) — additive to the flat `wal_applied_seq`/`wal_max_seq` fields above,
    /// which stay pinned to the default group.
    wal_group_positions: Vec<(String, Option<u64>, Option<u64>)>,
    last_index_time: Option<String>,
    index_created_at: Option<String>,
    name_index_trusted: bool,
    name_index_fallback_scans: u64,
    cross_group_pointers: PointerStateCounts,
}

/// Outcome of the status-gathering blocking task: either the graph is open and every
/// table-touching query succeeded, or a core table (e.g. `Entity`) is missing and the graph is
/// open-but-not-queryable — a second degraded state distinct from ADR-0009's "DB never opened"
/// (see ADR-0325). WAL metadata and the in-memory name-index counters don't depend on the
/// broken table, so both outcomes carry them.
enum StatusOutcome {
    Queryable(StatusFields),
    NotQueryable {
        reason: String,
        wal_exists: bool,
        wal_file_count: u64,
        wal_byte_size: u64,
        wal_applied_seq: Option<u64>,
        wal_max_seq: Option<u64>,
        wal_group_positions: Vec<(String, Option<u64>, Option<u64>)>,
        name_index_trusted: bool,
        name_index_fallback_scans: u64,
    },
}

async fn handle_knowledge_status(state: Arc<AppState>) -> Result<Value, Error> {
    let ontology_summary = {
        let drift = state
            .ontology_drift
            .lock()
            .map(|g| (g.drifted, g.drift_summary.clone()))
            .unwrap_or((false, None));
        let (drifted, drift_summary) = drift;
        match &state.ontology {
            Some(o) => json!({
                "present": true,
                "loaded": true,
                "mode": o.mode.to_string(),
                "entity_type_count": o.entity_types.len(),
                "relation_type_count": o.relation_types.len(),
                "drifted": drifted,
                "drift_summary": drift_summary,
            }),
            None => json!({
                "present": false,
                "loaded": false,
                "mode": null,
                "entity_type_count": 0,
                "relation_type_count": 0,
                "drifted": drifted,
                "drift_summary": drift_summary,
            }),
        }
    };

    let db_opt = state.db.load_full();
    if db_opt.is_none() {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        // Only advertise rebuild_from_workspace_wal when a WAL root is actually configured;
        // otherwise clients would offer an option that always fails immediately.
        let mut recovery_available = vec!["drop_lbug_wal"];
        if state.wal_root.is_some() {
            recovery_available.push("rebuild_from_workspace_wal");
        }
        return Ok(json!({
            "running": true,
            "degraded": true,
            "reason": reason,
            "context_graph_initialized": false,
            "connected": false,
            "queryable": false,
            "initializing": false,
            "recovery_available": recovery_available,
            "ontology": ontology_summary,
            "indices_built": state.indices_built.load(Ordering::Acquire),
            "name_index_trusted": null,
            "name_index_fallback_scans": null,
            "cross_group_pointers": null,
        }));
    }
    let db = db_opt.unwrap();
    let db_path = state.db_path.clone();
    let embedding_model = state.embedding_model.clone();
    let embedding_dim = state.embedder.dim();
    let wal_root = state.wal_root.clone();
    // The flat wal.* fields stay pinned to the default group's own WAL directory (issue #378
    // FR-007), never the root itself — an existing consumer (e.g. orac) that only reads these
    // flat fields must see exactly the default group's position, unchanged from a pre-378
    // single-group instance.
    let default_group_dir = wal_root
        .as_deref()
        .map(|root| crate::wal_group::group_wal_dir(root, DEFAULT_GROUP_ID))
        .transpose()?;

    // Phase 0 (write lock): backfill every group's applied_seq that isn't known yet, before the
    // read-locked status computation below reads it (issue #378 Review finding). Every other
    // write in this codebase goes through the exclusive write_lock; backfill_applied_seq_if_absent
    // can issue a real MERGE...SET (set_applied_seq) once per group, the first time that group is
    // ever queried, so it must not run under a shared read lock — otherwise two concurrent
    // knowledge_status calls racing to backfill the same not-yet-backfilled group could issue
    // concurrent write transactions against the single-writer embedded DB with no serialization.
    // Held only for this narrow pass, not the whole handler, so status calls after the first one
    // per group (the common case — backfill_applied_seq_if_absent's own early-return makes every
    // later call a pure read) still mostly see read-lock-only contention below.
    {
        let db_c = Arc::clone(&db);
        let wal_root_c = wal_root.clone();
        let _write_guard = state.write_lock.write().await;
        tokio::task::spawn_blocking(move || {
            let conn = db_c.connect()?;
            if let Some(root) = wal_root_c.as_deref() {
                for (gid, dir) in crate::wal_group::list_group_wal_dirs(root).unwrap_or_default() {
                    let _ = crate::recovery::backfill_applied_seq_if_absent(&conn, &gid, &dir);
                }
            }
            Ok::<(), crate::error::Error>(())
        })
        .await??;
    }

    let _guard = state.write_lock.read().await;
    let outcome =
        tokio::task::spawn_blocking(move || -> Result<StatusOutcome, crate::error::Error> {
            let conn = db.connect()?;
            let (wal_exists, wal_file_count, wal_byte_size) =
                scan_wal_dir(default_group_dir.as_deref());
            // Read fresh on every call, never cached on AppState (FR-001, SC-001: must prove
            // persistence across restart, not memoisation). Each is best-effort: a read failure
            // (e.g. the WalPosition table or WAL dir is transiently unreadable) degrades to
            // `null`, the same "unknown position" value a genuine backfill failure reports —
            // never fatal to the rest of the status response. If the default group has no WAL
            // directory at all (e.g. a pure replica that only ever hydrated other groups), both
            // stay `null` — a documented signal of "no default group", not an error.
            let wal_applied_seq = conn.get_applied_seq(DEFAULT_GROUP_ID).unwrap_or(None);
            let wal_max_seq = default_group_dir
                .as_deref()
                .and_then(|d| crate::wal::wal_max_seq(d).unwrap_or(None));
            // Per-group breakdown (FR-007): every group that currently has a WAL directory.
            // Backfilling already happened above under the write lock (Phase 0) — this is a pure
            // read of whatever that pass established, never a write, so it's safe under the
            // shared read lock this closure runs under.
            let wal_group_positions: Vec<(String, Option<u64>, Option<u64>)> = wal_root
                .as_deref()
                .and_then(|root| crate::wal_group::list_group_wal_dirs(root).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|(gid, dir)| {
                    let applied = conn.get_applied_seq(&gid).unwrap_or(None);
                    let max = crate::wal::wal_max_seq(&dir).unwrap_or(None);
                    (gid, applied, max)
                })
                .collect();
            let name_index_trusted = conn.name_index_trusted();
            let name_index_fallback_scans = conn.name_index_fallback_scan_count();

            // Only the table-touching queries below can hit "table does not exist" (FR-001); the
            // WAL scan and name-index counters above are in-memory/filesystem and always succeed.
            let table_result: Result<_, crate::error::Error> = (|| {
                let entity_count = conn.count_nodes("Entity")?;
                let episode_count = conn.count_nodes("Episodic")?;
                let edge_count = conn.count_relates_to_edges()?;
                let last_index_time = conn.get_latest_episode_time()?;
                let index_created_at = conn.get_earliest_episode_time()?;
                let cross_group_pointers = conn.count_cross_group_pointers()?;
                Ok((
                    entity_count,
                    episode_count,
                    edge_count,
                    last_index_time,
                    index_created_at,
                    cross_group_pointers,
                ))
            })();

            match table_result {
                Ok((
                    entity_count,
                    episode_count,
                    edge_count,
                    last_index_time,
                    index_created_at,
                    cross_group_pointers,
                )) => Ok(StatusOutcome::Queryable(StatusFields {
                    entity_count,
                    episode_count,
                    edge_count,
                    wal_exists,
                    wal_file_count,
                    wal_byte_size,
                    wal_applied_seq,
                    wal_max_seq,
                    wal_group_positions,
                    last_index_time,
                    index_created_at,
                    name_index_trusted,
                    name_index_fallback_scans,
                    cross_group_pointers,
                })),
                // A missing core table means the graph is open but not queryable — degrade to a
                // status response (FR-001) rather than propagating (see ADR-0325). Any other error
                // (a genuinely different query failure) still propagates via `?` below (FR-006).
                Err(e) if is_missing_table_error(&e) => Ok(StatusOutcome::NotQueryable {
                    reason: e.to_string(),
                    wal_exists,
                    wal_file_count,
                    wal_byte_size,
                    wal_applied_seq,
                    wal_max_seq,
                    wal_group_positions,
                    name_index_trusted,
                    name_index_fallback_scans,
                }),
                Err(e) => Err(e),
            }
        })
        .await??;
    drop(_guard);

    // startup sequence (Db::open → init_schema → bind socket) completes before any request
    // can arrive, so these lifecycle values are always true/true/false at handler time
    let mut result = match outcome {
        StatusOutcome::Queryable(fields) => {
            let mut result = json!({
                "database_path": db_path,
                "embedding_model": embedding_model,
                "embedding_dim": embedding_dim,
                "entity_count": fields.entity_count,
                "relationship_count": fields.edge_count,
                "episode_count": fields.episode_count,
                "last_index_time": fields.last_index_time,
                "context_graph_initialized": true,
                "connected": true,
                "queryable": true,
                "initializing": false,
                "wal": {
                    "exists": fields.wal_exists,
                    "file_count": fields.wal_file_count,
                    "byte_size": fields.wal_byte_size,
                    // issue #353: null = unknown (backfill failed — full rebuild required),
                    // 0 = nothing applied, integer = known applied position. See
                    // docs/operations.md for the consumer comparison and the JS `null < 5`
                    // footgun (null coerces to true in JS numeric comparisons, unlike Rust
                    // and Python where it breaks arithmetic).
                    "applied_seq": fields.wal_applied_seq,
                    "max_seq": fields.wal_max_seq,
                },
                // Additive per-group breakdown (issue #378 FR-007): a map keyed by group_id,
                // each shaped like the flat "wal" object above. The flat fields above remain
                // pinned to the default group specifically, so an existing consumer that only
                // reads them (e.g. orac) requires no change.
                "wal_groups": wal_group_positions_json(&fields.wal_group_positions),
                "indices_built": state.indices_built.load(Ordering::Acquire),
                "name_index_trusted": fields.name_index_trusted,
                "name_index_fallback_scans": fields.name_index_fallback_scans,
                "cross_group_pointers": {
                    "bound": fields.cross_group_pointers.bound,
                    "unbound": fields.cross_group_pointers.unbound,
                    "ambiguous": fields.cross_group_pointers.ambiguous,
                },
            });
            if let Some(t) = fields.index_created_at {
                result["index_created_at"] = json!(t);
            }
            result
        }
        StatusOutcome::NotQueryable {
            reason,
            wal_exists,
            wal_file_count,
            wal_byte_size,
            wal_applied_seq,
            wal_max_seq,
            wal_group_positions,
            name_index_trusted,
            name_index_fallback_scans,
        } => json!({
            "database_path": db_path,
            "embedding_model": embedding_model,
            "embedding_dim": embedding_dim,
            // Counts are null, not 0, so a broken graph can never be mistaken for an empty one
            // (FR-003) — a genuinely empty graph reports 0 via the Queryable branch above.
            "entity_count": null,
            "relationship_count": null,
            "episode_count": null,
            "last_index_time": null,
            // Present as null (not omitted) so a caller reading this specific field doesn't see
            // it silently disappear the moment the table breaks. This does not make the key's
            // presence uniform across all three status states in general: the Queryable branch
            // above only sets it when a value exists (an empty graph omits it, matching
            // pre-existing behavior — see `test_knowledge_status_empty_db`), and the DB-not-open
            // branch (ADR-0009) omits it too. A caller must still treat the key as optional.
            "index_created_at": null,
            "context_graph_initialized": true,
            "connected": true,
            "queryable": false,
            "reason": reason,
            "initializing": false,
            "wal": {
                "exists": wal_exists,
                "file_count": wal_file_count,
                "byte_size": wal_byte_size,
                "applied_seq": wal_applied_seq,
                "max_seq": wal_max_seq,
            },
            "wal_groups": wal_group_positions_json(&wal_group_positions),
            // Forced false rather than reading the live atomic (FR-002): if the core table is
            // missing, any index built against it is meaningless, regardless of whether a
            // `knowledge_build_indices` call has run since the table broke to store `false`
            // itself. Reading the atomic here would let a stale `true` from before the table
            // went missing leak into a `queryable:false` response — the same class of staleness
            // bug #297 fixed for a different code path.
            "indices_built": false,
            "name_index_trusted": name_index_trusted,
            "name_index_fallback_scans": name_index_fallback_scans,
            "cross_group_pointers": {
                "bound": null,
                "unbound": null,
                "ambiguous": null,
            },
        }),
    };
    result["ontology"] = ontology_summary;
    Ok(result)
}

/// Renders `handle_knowledge_status`'s per-group `(group_id, applied_seq, max_seq)` tuples
/// (issue #378 FR-007) into the additive `wal_groups` JSON map.
fn wal_group_positions_json(positions: &[(String, Option<u64>, Option<u64>)]) -> Value {
    let map: serde_json::Map<String, Value> = positions
        .iter()
        .map(|(gid, applied, max)| {
            (
                gid.clone(),
                json!({ "applied_seq": applied, "max_seq": max }),
            )
        })
        .collect();
    Value::Object(map)
}

fn scan_wal_dir(wal_dir: Option<&std::path::Path>) -> (bool, u64, u64) {
    let dir = match wal_dir {
        Some(d) => d,
        None => return (false, 0, 0),
    };
    if !dir.exists() {
        return (false, 0, 0);
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return (true, 0, 0),
    };
    let mut file_count: u64 = 0;
    let mut byte_size: u64 = 0;
    for entry in rd.flatten() {
        if entry.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
            file_count += 1;
            if let Ok(meta) = entry.metadata() {
                byte_size += meta.len();
            }
        }
    }
    (true, file_count, byte_size)
}

async fn handle_knowledge_process_chunk(
    req: &IpcRequest,
    state: Arc<AppState>,
) -> Result<Value, Error> {
    let p = &req.params;

    let chunk_text = p["chunk_text"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("chunk_text is required and must be non-empty".to_string()))?
        .to_string();

    let chunk_id = p["chunk_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("chunk_id is required".to_string()))?
        .to_string();

    let source_file = p["source_file"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("source_file is required".to_string()))?
        .to_string();

    let group_id = p["group_id"]
        .as_str()
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string();

    let ref_time = match p["reference_time"].as_str() {
        Some(s) => {
            chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|_| Error::Ipc(format!("reference_time is not valid ISO 8601: {s}")))?;
            s.to_string()
        }
        None => chrono::Utc::now().to_rfc3339(),
    };

    let start = Instant::now();
    let source_desc = format!("{}:{}", source_file, chunk_id);
    let source_type = if source_file.to_lowercase().ends_with(".json") {
        SourceType::Json
    } else {
        SourceType::Text
    };
    let result = episode::add_episode(
        state,
        &chunk_id,
        &chunk_text,
        "text",
        &source_desc,
        &ref_time,
        &group_id,
        source_type,
        None,
    )
    .await?;

    Ok(json!({
        "success": true,
        "chunk_id": chunk_id,
        "source_file": source_file,
        "episode_uuid": result.episode_uuid,
        "nodes_extracted": result.nodes_extracted,
        "edges_extracted": result.edges_extracted,
        "edges_dropped_unresolvable": result.edges_dropped_unresolvable,
        "edges_reclassified_unclassified": result.edges_reclassified_unclassified,
        "entities_reclassified_unclassified": result.entities_reclassified_unclassified,
        "entities_dropped_malformed": result.entities_dropped_malformed,
        "edges_dropped_malformed": result.edges_dropped_malformed,
        "duration_seconds": start.elapsed().as_secs_f64(),
    }))
}

// ── Search handlers — no lock (hot read path, never blocked by writes) ────────

async fn handle_find_entities(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let query = p["query"].as_str().unwrap_or("").to_string();
    let group_ids = extract_group_ids(&p["group_ids"]);
    let limit = p["num_results"].as_u64().unwrap_or(10) as usize;

    let result = search::hybrid_entity_search(
        load_db(&state)?,
        Arc::clone(&state.embedder),
        &query,
        group_ids.clone(),
        limit,
    )
    .await;

    let entities = match result {
        Ok(e) => e,
        Err(e) if is_missing_index_error(&e) => {
            if !state.indices_built.load(Ordering::Acquire) {
                build_indices_once(&state).await?;
                search::hybrid_entity_search(
                    load_db(&state)?,
                    Arc::clone(&state.embedder),
                    &query,
                    group_ids,
                    limit,
                )
                .await
                .map_err(|e2| {
                    if is_missing_index_error(&e2) {
                        Error::Ipc(MISSING_INDEX_USER_MSG.to_string())
                    } else {
                        e2
                    }
                })?
            } else {
                return Err(Error::Ipc(MISSING_INDEX_USER_MSG.to_string()));
            }
        }
        Err(e) => return Err(e),
    };

    let count = entities.len();
    Ok(json!({"nodes": entities, "count": count}))
}

async fn handle_find_relationships(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let query = p["query"].as_str().unwrap_or("").to_string();
    let group_ids = extract_group_ids(&p["group_ids"]);
    let limit = p["num_results"].as_u64().unwrap_or(10) as usize;

    let result = search::hybrid_edge_search(
        load_db(&state)?,
        Arc::clone(&state.embedder),
        &query,
        group_ids.clone(),
        limit,
    )
    .await;

    let edges = match result {
        Ok(e) => e,
        Err(e) if is_missing_index_error(&e) => {
            if !state.indices_built.load(Ordering::Acquire) {
                build_indices_once(&state).await?;
                search::hybrid_edge_search(
                    load_db(&state)?,
                    Arc::clone(&state.embedder),
                    &query,
                    group_ids,
                    limit,
                )
                .await
                .map_err(|e2| {
                    if is_missing_index_error(&e2) {
                        Error::Ipc(MISSING_INDEX_USER_MSG.to_string())
                    } else {
                        e2
                    }
                })?
            } else {
                return Err(Error::Ipc(MISSING_INDEX_USER_MSG.to_string()));
            }
        }
        Err(e) => return Err(e),
    };

    let count = edges.len();
    Ok(json!({"facts": edges, "count": count}))
}

// ── Other read handlers — hold shared read guard across spawn_blocking ────────
//
// Guard stays in the async scope while spawn_blocking executes.
// RwLockReadGuard is not 'static so it cannot move into the closure.

async fn handle_get_episodes(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let group_id = p["group_id"]
        .as_str()
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string();
    let last_n = p["last_n"].as_u64().unwrap_or(50) as usize;

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let episodes = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.retrieve_episodes(&group_id, last_n)
    })
    .await??;
    drop(_guard);

    let count = episodes.len();
    Ok(json!({"episodes": episodes, "count": count}))
}

async fn handle_delete_episode(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let episode_uuid = req.params["episode_uuid"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let _guard = state.write_lock.write().await;
    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let conn = db.connect()?;
        // The request names no group_id, but this deletes exactly one episode in exactly one
        // group — unlike the genuinely multi-group FR-004-exempt sites, that group is knowable
        // by reading it before the delete runs, so the WAL flush below routes to it directly
        // rather than the default group's writer. A missing episode (already deleted, or a
        // bogus uuid) has no group to route to and no mutation to flush either — DEFAULT_GROUP_ID
        // is used only as a harmless fallback for that no-op case.
        let group_id = conn
            .get_episode_group_id(&episode_uuid)?
            .unwrap_or_else(|| DEFAULT_GROUP_ID.to_string());
        conn.remove_episode(&episode_uuid)?;
        wal_exec::wal_flush_ungrouped(&state_c, &group_id, conn.drain_mutations());
        Ok(())
    })
    .await??;
    drop(_guard);

    Ok(json!({"status": "deleted"}))
}

async fn handle_get_nodes_by_group(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let group_ids = extract_group_ids(&req.params["group_ids"]);

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let nodes = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids.iter().map(String::as_str).collect();
        conn.get_entities_by_group_ids(&gid_refs)
    })
    .await??;
    drop(_guard);

    let count = nodes.len();
    Ok(json!({"nodes": nodes, "count": count}))
}

async fn handle_get_edges_by_group(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let group_ids = extract_group_ids(&req.params["group_ids"]);

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let edges = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids.iter().map(String::as_str).collect();
        conn.get_edges_by_group_ids(&gid_refs)
    })
    .await??;
    drop(_guard);

    let count = edges.len();
    Ok(json!({"edges": edges, "count": count}))
}

async fn handle_get_edges_by_uuids(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let uuids: Vec<String> = req.params["uuids"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let edges = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let uuid_refs: Vec<&str> = uuids.iter().map(String::as_str).collect();
        conn.get_edges_by_uuids(&uuid_refs)
    })
    .await??;
    drop(_guard);

    let count = edges.len();
    Ok(json!({"edges": edges, "count": count}))
}

async fn handle_query_cypher(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let query = req.params["query"].as_str().unwrap_or("").to_string();

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    // Write lock required: this handler may execute mutations, and the WAL flush must
    // be serialized with all other write paths to preserve replay order.
    let _guard = state.write_lock.write().await;
    let rows = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<String>>, Error> {
        let conn = db.connect()?;
        // Raw Cypher is passed directly to lbug with no param interpolation or value coercion.
        // User-supplied strings are never rewritten, so this path has no type-coercion surface
        // and is explicitly out of scope per Issue #170 FR-008.
        let rows = conn.cypher_query(&query)?;
        // Arbitrary Cypher carries no group attribution at all (FR-004): routes through the
        // default group's writer, a documented limitation rather than mutation-level tracking.
        wal_exec::wal_flush_ungrouped(&state_c, DEFAULT_GROUP_ID, conn.drain_mutations());
        // The raw-Cypher escape hatch bypasses every NameIndex insert/update hook (issue
        // #283, FR-004): a CREATE/SET/DELETE/etc. issued here can create, rename, or remove
        // an Entity row with the index never learning about it. Reuse the WAL's own
        // mutation-keyword heuristic so read-only Cypher through this admin path pays no
        // extra scan cost, and only rebuild (a full Entity scan) when the query looks like a
        // write. Index DDL (CREATE/DROP INDEX) is excluded first, matching `log_mutation`'s
        // own filter — it never touches Entity rows, so it doesn't warrant a rebuild even
        // though it contains CREATE/DROP keywords. A rebuild failure is non-fatal to this
        // request — it marks the index untrusted so downstream endpoint-authority lookups
        // fall back to a scan until the next successful rebuild, matching the two
        // `knowledge_rebuild_from_wal` arms below.
        if !crate::wal::is_index_ddl(&query) && crate::wal::looks_like_mutation(&query) {
            if let Err(e) = conn.rebuild_name_index() {
                eprintln!("[NAME INDEX] rebuild after query_cypher mutation failed: {e}");
                conn.mark_name_index_untrusted();
            }
        }
        Ok(rows)
    })
    .await??;
    drop(_guard);

    Ok(json!({"rows": rows}))
}

async fn handle_build_indices(state: Arc<AppState>) -> Result<Value, Error> {
    let db = load_db(&state)?;
    let _guard = state.write_lock.write().await;
    // Preset to false before attempting the build: if spawn_blocking's join itself fails (e.g. a
    // panic), the early `?` return below must not leave a stale `true` from a prior successful
    // build in place.
    state.indices_built.store(false, Ordering::Release);
    let build_result = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.build_indices_and_constraints()
    })
    .await?;
    drop(_guard);

    // Unconditional store of the real outcome (FR-006), before propagating any error — a prior
    // successful build's `true` must not survive this call's failure.
    state
        .indices_built
        .store(build_result.is_ok(), Ordering::Release);
    build_result?;
    Ok(json!({"status": "ok", "indices_built": true}))
}

// ── Tier 1b read handlers ─────────────────────────────────────────────────────

async fn handle_search_passages(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let query = p["query"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("query is required and must be non-empty".to_string()))?
        .to_string();
    let num_results = (p["num_results"].as_u64().unwrap_or(10) as usize).clamp(1, 100);
    let min_score = p["min_score"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0);
    let group_ids = extract_optional_group_ids(&p["group_ids"]);

    let result = search::search_passages(
        load_db(&state)?,
        Arc::clone(&state.embedder),
        &query,
        group_ids.clone(),
        num_results,
        min_score,
    )
    .await;

    let passages = match result {
        Ok(p) => p,
        Err(e) if is_missing_index_error(&e) => {
            if !state.indices_built.load(Ordering::Acquire) {
                build_indices_once(&state).await?;
                search::search_passages(
                    load_db(&state)?,
                    Arc::clone(&state.embedder),
                    &query,
                    group_ids,
                    num_results,
                    min_score,
                )
                .await
                .map_err(|e2| {
                    if is_missing_index_error(&e2) {
                        Error::Ipc(MISSING_INDEX_USER_MSG.to_string())
                    } else {
                        e2
                    }
                })?
            } else {
                return Err(Error::Ipc(MISSING_INDEX_USER_MSG.to_string()));
            }
        }
        Err(e) => return Err(e),
    };

    let count = passages.len();
    Ok(json!({ "passages": passages, "count": count }))
}

async fn handle_list_entities(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let num_results_raw = p["num_results"].as_i64().unwrap_or(500);
    if num_results_raw <= 0 {
        return Err(Error::Ipc("num_results must be > 0".to_string()));
    }
    let num_results = num_results_raw as usize;
    let group_ids = extract_optional_group_ids(&p["group_ids"]);

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let nodes = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids
            .as_deref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let gid_slice = group_ids.as_deref().map(|_| gid_refs.as_slice());
        let mut nodes = conn.list_entities(gid_slice, num_results)?;
        let uuid_owned: Vec<String> = nodes.iter().map(|n| n.uuid.clone()).collect();
        let uuid_refs: Vec<&str> = uuid_owned.iter().map(String::as_str).collect();
        let mut ep_info = conn
            .get_episode_info_for_entities(&uuid_refs, gid_slice)
            .unwrap_or_default();
        for node in &mut nodes {
            if let Some((ep_uuids, src_descs)) = ep_info.remove(&node.uuid) {
                node.episode_uuids = ep_uuids;
                node.source_descriptions = src_descs;
            }
        }
        Ok::<_, crate::error::Error>(nodes)
    })
    .await??;
    drop(_guard);

    let count = nodes.len();
    Ok(json!({ "nodes": nodes, "count": count }))
}

async fn handle_list_relationships(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let num_results_raw = p["num_results"].as_i64().unwrap_or(1000);
    if num_results_raw <= 0 {
        return Err(Error::Ipc("num_results must be > 0".to_string()));
    }
    let num_results = num_results_raw as usize;
    let group_ids = extract_optional_group_ids(&p["group_ids"]);

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let facts = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids
            .as_deref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let gid_slice = group_ids.as_deref().map(|_| gid_refs.as_slice());
        let mut facts = conn.list_relationships(gid_slice, num_results)?;
        let entity_uuids_owned = edge_endpoint_uuids(&facts);
        let entity_uuid_refs: Vec<&str> = entity_uuids_owned.iter().map(String::as_str).collect();
        let ep_info = conn
            .get_episode_info_for_entities(&entity_uuid_refs, gid_slice)
            .unwrap_or_default();
        for edge in &mut facts {
            enrich_edge_from_entity_ep_info(edge, &ep_info);
        }
        Ok::<_, crate::error::Error>(facts)
    })
    .await??;
    drop(_guard);

    let count = facts.len();
    Ok(json!({ "facts": facts, "count": count }))
}

async fn handle_get_entity_neighbors(
    req: &IpcRequest,
    state: Arc<AppState>,
) -> Result<Value, Error> {
    let p = &req.params;
    let entity_uuid = p["entity_uuid"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("entity_uuid is required".to_string()))?
        .to_string();
    let num_results = p["num_results"].as_u64().unwrap_or(50) as usize;
    let group_ids = extract_optional_group_ids(&p["group_ids"]);

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let (edges, nodes) = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids
            .as_deref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let gid_slice = group_ids.as_deref().map(|_| gid_refs.as_slice());
        let (mut edges, neighbor_uuids) =
            conn.get_entity_neighbors(&entity_uuid, gid_slice, num_results)?;
        let mut nodes = conn.get_entities_by_uuids(&neighbor_uuids)?;
        // Collect all entity UUIDs: neighbor nodes + edge endpoints (for edge enrichment).
        let mut all_entity_uuids_owned = edge_endpoint_uuids(&edges);
        let mut seen: std::collections::HashSet<String> =
            all_entity_uuids_owned.iter().cloned().collect();
        for n in &nodes {
            if seen.insert(n.uuid.clone()) {
                all_entity_uuids_owned.push(n.uuid.clone());
            }
        }
        let all_entity_uuid_refs: Vec<&str> =
            all_entity_uuids_owned.iter().map(String::as_str).collect();
        let ep_info = conn
            .get_episode_info_for_entities(&all_entity_uuid_refs, gid_slice)
            .unwrap_or_default();
        for node in &mut nodes {
            if let Some((ep_uuids, src_descs)) = ep_info.get(&node.uuid) {
                node.episode_uuids = ep_uuids.clone();
                node.source_descriptions = src_descs.clone();
            }
        }
        for edge in &mut edges {
            enrich_edge_from_entity_ep_info(edge, &ep_info);
        }
        Ok::<_, crate::error::Error>((edges, nodes))
    })
    .await??;
    drop(_guard);

    let center_uuid = p["entity_uuid"].as_str().unwrap_or("").to_string();
    let node_count = nodes.len();
    let edge_count = edges.len();
    Ok(json!({
        "center_uuid": center_uuid,
        "nodes": nodes,
        "edges": edges,
        "count": node_count,
        "node_count": node_count,
        "edge_count": edge_count,
    }))
}

async fn handle_get_entities_by_source(
    req: &IpcRequest,
    state: Arc<AppState>,
) -> Result<Value, Error> {
    let p = &req.params;
    let source = p["source"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("source is required and must be non-empty".to_string()))?
        .to_string();
    let num_results = p["num_results"].as_u64().unwrap_or(100) as usize;
    let group_ids = extract_optional_group_ids(&p["group_ids"]);

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let nodes = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids
            .as_deref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let gid_slice = group_ids.as_deref().map(|_| gid_refs.as_slice());
        let mut nodes = conn.get_entities_by_source(&source, gid_slice, num_results)?;
        let uuid_owned: Vec<String> = nodes.iter().map(|n| n.uuid.clone()).collect();
        let uuid_refs: Vec<&str> = uuid_owned.iter().map(String::as_str).collect();
        let mut ep_info = conn
            .get_episode_info_for_entities(&uuid_refs, gid_slice)
            .unwrap_or_default();
        for node in &mut nodes {
            if let Some((ep_uuids, src_descs)) = ep_info.remove(&node.uuid) {
                node.episode_uuids = ep_uuids;
                node.source_descriptions = src_descs;
            }
        }
        Ok::<_, crate::error::Error>(nodes)
    })
    .await??;
    drop(_guard);

    let source_val = p["source"].as_str().unwrap_or("").to_string();
    let count = nodes.len();
    Ok(json!({ "source": source_val, "nodes": nodes, "count": count }))
}

async fn handle_delete_by_source(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let source_file = p["source_file"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("source_file is required and must be non-empty".to_string()))?
        .to_string();

    let group_ids_owned = extract_optional_group_ids(&p["group_ids"]);

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let _guard = state.write_lock.write().await;
    let deleted_uuids = tokio::task::spawn_blocking(move || -> Result<Vec<String>, Error> {
        let conn = db.connect()?;
        let gid_refs: Option<Vec<&str>> = group_ids_owned
            .as_ref()
            .map(|v| v.iter().map(String::as_str).collect());
        let uuids = conn.remove_episodes_by_source(&source_file, gid_refs.as_deref())?;
        // `group_ids` is optional and, when given, an array — a call with no filter or more
        // than one group can genuinely span multiple groups, so per-operation attribution can't
        // name one group there (FR-004), and it routes through the default group's writer, the
        // same documented limitation as handle_delete_by_group. But a caller that named exactly
        // one group is not ambiguous — route to that group directly rather than discarding
        // information the request already gave us.
        let target_group = match group_ids_owned.as_deref() {
            Some([single]) => single.as_str(),
            _ => DEFAULT_GROUP_ID,
        };
        wal_exec::wal_flush_ungrouped(&state_c, target_group, conn.drain_mutations());
        Ok(uuids)
    })
    .await??;
    drop(_guard);

    Ok(json!({
        "success": true,
        "source_file": req.params["source_file"],
        "deleted_count": deleted_uuids.len(),
        "deleted_uuids": deleted_uuids,
    }))
}

/// Deletes all Episodic nodes for the given chunk_id.
///
/// Note: only the episode nodes are removed. Entity nodes that were extracted
/// from this chunk and are connected solely to the deleted episodes become
/// orphaned — they remain in the graph. Callers should be aware of this outcome.
/// A future entity-GC method may clean up such orphans.
async fn handle_delete_chunk_episode(
    req: &IpcRequest,
    state: Arc<AppState>,
) -> Result<Value, Error> {
    let p = &req.params;
    let chunk_id = p["chunk_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("chunk_id is required and must be non-empty".to_string()))?
        .to_string();

    let group_ids_owned = extract_optional_group_ids(&p["group_ids"]);

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let _guard = state.write_lock.write().await;
    let deleted_uuids = tokio::task::spawn_blocking(move || -> Result<Vec<String>, Error> {
        let conn = db.connect()?;
        let gid_refs: Option<Vec<&str>> = group_ids_owned
            .as_ref()
            .map(|v| v.iter().map(String::as_str).collect());
        let uuids = conn.remove_episodes_by_chunk_id(&chunk_id, gid_refs.as_deref())?;
        // Same FR-004 rationale as handle_delete_by_source above: a single named group is
        // routed there directly; no filter, or more than one group, falls back to the default
        // group's writer as a genuinely multi-group call.
        let target_group = match group_ids_owned.as_deref() {
            Some([single]) => single.as_str(),
            _ => DEFAULT_GROUP_ID,
        };
        wal_exec::wal_flush_ungrouped(&state_c, target_group, conn.drain_mutations());
        Ok(uuids)
    })
    .await??;
    drop(_guard);

    Ok(json!({
        "success": true,
        "chunk_id": req.params["chunk_id"],
        "deleted_count": deleted_uuids.len(),
        "deleted_uuids": deleted_uuids,
    }))
}

/// `knowledge_delete_by_group` (issue #361): removes all `Entity`, `Episodic`, and same-group
/// `RelatesToNode_` data for one or more `group_ids`, leaving no orphans and never deleting a
/// `RelatesToNode_` owned by a group outside the call (FR-008). See `crate::group_purge` for
/// the full mechanism.
///
/// `group_ids` is required and validated explicitly here rather than via
/// `extract_optional_group_ids` (which treats an absent/empty value as "all groups") — a
/// destructive admin op must never silently default to purging everything.
///
/// `confirm` gates the real (mutating) call, exactly like `handle_clear_all`; `dry_run: true`
/// takes precedence and skips the gate entirely (FR-013), since it doesn't mutate anything.
///
/// This tool is `admin`-scoped (FR-007), not `write` — a read-only replica launched without
/// `write` still needs to purge as part of a per-source refresh.
async fn handle_delete_by_group(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    // Every element must be a non-empty string, or the request is rejected outright — for a
    // destructive, confirm-gated admin op, silently dropping a malformed element (e.g. a stray
    // number from an upstream template bug) and proceeding with the rest would purge less than
    // the caller specified without any indication that anything was ignored.
    let group_ids: Vec<String> = p["group_ids"]
        .as_array()
        .filter(|arr| {
            !arr.is_empty()
                && arr
                    .iter()
                    .all(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        })
        .map(|arr| {
            // Dedup while preserving first-seen order: a repeated group_id would otherwise
            // produce duplicate per-group entries in the response and redundant (harmless but
            // misleading) rebind passes in `group_purge::purge_groups`.
            let mut seen = std::collections::HashSet::new();
            arr.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .filter(|s| seen.insert(s.clone()))
                .collect::<Vec<_>>()
        })
        .ok_or_else(|| {
            Error::Ipc(
                "group_ids is required and must be a non-empty array of non-empty strings"
                    .to_string(),
            )
        })?;

    let dry_run = p["dry_run"].as_bool().unwrap_or(false);
    let confirm = p["confirm"].as_bool().unwrap_or(false);
    if !dry_run && !confirm {
        return Err(Error::Ipc(
            "Must set 'confirm' to true to purge group data (or 'dry_run' to preview)".to_string(),
        ));
    }

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let _guard = state.write_lock.write().await;
    let counts = tokio::task::spawn_blocking(move || -> Result<group_purge::PurgeCounts, Error> {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids.iter().map(String::as_str).collect();
        let ts = chrono::Utc::now().to_rfc3339();
        let counts = group_purge::purge_groups(&conn, &gid_refs, &ts, dry_run)?;
        // Purges an array of group_ids and its forced rebind can write into other, non-purged
        // "owning" groups' RelatesToNode_ rows in the same drain_mutations call (ADR-0361) —
        // genuinely multi-group, so per-operation attribution can't name one group (FR-004).
        // Routes through the default group's writer, a documented limitation.
        wal_exec::wal_flush_ungrouped(&state_c, DEFAULT_GROUP_ID, conn.drain_mutations());
        Ok(counts)
    })
    .await??;
    drop(_guard);

    Ok(json!({
        "success": true,
        "dry_run": counts.dry_run,
        "groups": counts.groups.iter().map(|g| json!({
            "group_id": g.group_id,
            "entities": g.entities,
            "episodes": g.episodes,
            "edges": g.edges,
        })).collect::<Vec<_>>(),
        "unbound_impacts": counts.unbound_impacts.iter().map(|u| json!({
            "group_id": u.owning_group_id,
            "pointer_count": u.pointer_count,
        })).collect::<Vec<_>>(),
        "applied_seq_reset": false,
    }))
}

async fn handle_clear_all(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let confirm = req.params["confirm"].as_bool().unwrap_or(false);
    if !confirm {
        return Err(Error::Ipc(
            "Must set 'confirm' to true to clear graph".to_string(),
        ));
    }
    // When true, the application WAL (.graphiti/wal/) is preserved so that
    // knowledge_rebuild_from_wal can replay mutations after the DB is cleared.
    // Default false preserves existing behavior (WAL deleted alongside the DB).
    let preserve_wal = req.params["preserve_wal"].as_bool().unwrap_or(false);

    let db_path = state.db_path.clone();
    let wal_root = state.wal_root.clone();
    let embedding_dim = state.embedder.dim();

    let _guard = state.write_lock.write().await;

    // Drop the old handle before deleting its files, mirroring clear_db_for_rebuild's existing
    // pattern (see its own comment): otherwise the old Db stays alive, with its underlying
    // file deleted out from under it, for the whole Phase-1-delete + Phase-2-reopen window
    // below. That window grew long enough for lbug's own internal checkpoint to occasionally
    // fire against the now-deleted file (`Error renaming file ... to ....wal.checkpoint: No
    // such file or directory`) once Phase 2 started doing more work per reinit (issue #353's
    // set_applied_seq(0) write below). load_db() already treats a None state.db as
    // Error::DbUnavailable, so this briefly surfaces as the existing degraded-mode error rather
    // than a file-not-found.
    state.db.store(None);

    if !preserve_wal {
        // Flush and drop every group's WalWriter before deleting the WAL root to avoid writing
        // to a path that no longer exists (issue #378: this now clears the whole per-group map,
        // not one writer). Nothing is eagerly recreated below — lazy per-group creation (FR-003)
        // means the next write to any group creates its writer and directory on demand, same as
        // a fresh install.
        state
            .wal_writers
            .lock()
            .map_err(|_| Error::Ipc("wal_writers lock poisoned".to_string()))?
            .clear();
    }

    // Phase 1: delete DB files (and optionally WAL directory) — point of no return
    let db_path_del = db_path.clone();
    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let path = std::path::Path::new(&db_path_del);
        if path.is_dir() {
            std::fs::remove_dir_all(path).map_err(|e| {
                Error::Ipc(format!("failed to delete DB dir '{}': {e}", db_path_del))
            })?;
        } else if path.exists() {
            std::fs::remove_file(path).map_err(|e| {
                Error::Ipc(format!("failed to delete DB file '{}': {e}", db_path_del))
            })?;
            // Remove lbug sibling files (e.g. <db>.wal) that lbug creates next to the DB file.
            // If we leave them behind, lbug will reject them on the next open because the
            // database ID in the WAL won't match the freshly created DB.
            for ext in &[".wal", ".lock"] {
                let _ = std::fs::remove_file(format!("{}{}", db_path_del, ext));
            }
        }
        if !preserve_wal {
            if let Some(root) = wal_root {
                if root.exists() {
                    std::fs::remove_dir_all(&root).map_err(|e| {
                        Error::Ipc(format!(
                            "failed to delete WAL root '{}': {e}",
                            root.display()
                        ))
                    })?;
                }
            }
        }
        Ok(())
    })
    .await??;

    // Phase 2: create fresh DB and initialize schema
    let db_path_reinit = db_path.clone();
    let reinit_result = tokio::task::spawn_blocking(move || -> Result<Db, Error> {
        // Ensure parent directory exists (it may have been removed with the DB).
        // Skip if parent is empty (e.g. db_path is a bare filename with no directory component).
        if let Some(parent) = std::path::Path::new(&db_path_reinit).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let db = Db::open(&db_path_reinit)?;
        {
            let conn = db.connect()?;
            conn.init_schema(embedding_dim)?;
            // Reset the applied-WAL-seq position (issue #353, FR-005) for the default group:
            // this DB is freshly empty, so `0` ("known, nothing applied") is correct — not
            // `null` ("unknown"), which would otherwise trigger an unnecessary backfill scan on
            // next open. Other groups' WalPosition rows are simply absent post-clear (correct:
            // "unknown/no stream yet", not "known-empty" — issue #378 preserves FR-009 parity
            // for the default group only, matching pre-378 single-group behavior exactly).
            conn.set_applied_seq(DEFAULT_GROUP_ID, 0)?;
            // Fresh empty table — a no-op today, kept for uniformity with every other
            // Entity-population path (issue #219).
            conn.rebuild_name_index()?;
        }
        Ok(db)
    })
    .await;

    let new_db = match reinit_result {
        Ok(Ok(db)) => db,
        Ok(Err(e)) => {
            drop(_guard);
            return Err(Error::Ipc(format!(
                "Graph files deleted but reinitialization failed: {e}. \
                 Restart the service to recover."
            )));
        }
        Err(e) => {
            drop(_guard);
            return Err(Error::Ipc(format!(
                "Graph files deleted but reinitialization task panicked: {e}. \
                 Restart the service to recover."
            )));
        }
    };

    state.db.store(Some(Arc::new(new_db)));
    state
        .indices_built
        .store(false, std::sync::atomic::Ordering::Release);

    // No eager WAL writer re-init here (unlike the pre-378 regression fix for #100): lazy
    // per-group creation (FR-003) means the next write to any group creates its writer and
    // directory on demand, the same as a fresh install — `wal_writers` was already cleared
    // above, so there is nothing stale left pointing at a deleted directory to guard against.
    drop(_guard);

    Ok(json!({"success": true, "message": "Graph cleared and reinitialized successfully"}))
}

// ── WAL admin handlers ────────────────────────────────────────────────────────

// FR-014: callers must add knowledge_dump_wal to service_protocol.py in the liminis-app repo.
async fn handle_dump_wal(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let group_id = p["group_id"].as_str().map(|s| s.to_string());

    // Resolve target_dir: caller-supplied, or default to {workspace}/.lcg/wal-compacted/.
    // db_path is typically "{workspace}/.lcg/db/liminis.db", so we go up two levels to reach
    // the .lcg directory.
    let target_dir: std::path::PathBuf = if let Some(s) = p["target_dir"].as_str() {
        std::path::PathBuf::from(s)
    } else {
        std::path::Path::new(&state.db_path)
            .parent() // .lcg/db
            .and_then(|p| p.parent()) // .lcg
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("wal-compacted")
    };

    // Fast-fail pre-lock check (FR-004): refuse any non-empty target_dir so callers cannot
    // accidentally mix a dump with pre-existing files or a prior dump.
    if target_dir.exists() && is_nonempty_dir(&target_dir) {
        return Err(Error::Ipc(format!(
            "knowledge_dump_wal: target_dir '{}' already exists and is not empty; \
             supply a clean path or remove existing files first",
            target_dir.display()
        )));
    }

    // Track whether the directory was created by us so cleanup-on-failure is safe (FR-012).
    let dir_existed_before = target_dir.exists();

    let max_events = state.wal_max_events_per_file;
    // Write lock held for the entire dump to prevent concurrent mutations from interleaving
    // with the snapshot (FR-009). Load the DB under the lock so any in-flight DB swap
    // (e.g. from knowledge_clear_all) is visible to us.
    let _guard = state.write_lock.write().await;
    let db = load_db(&state)?;

    let target_dir_c = target_dir.clone();
    let group_id_c = group_id.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<crate::dump::DumpResult, Error> {
        // Authoritative FR-004 re-check under the write lock to close the TOCTOU window.
        if target_dir_c.exists() && is_nonempty_dir(&target_dir_c) {
            return Err(Error::Ipc(format!(
                "knowledge_dump_wal: target_dir '{}' already exists and is not empty; \
                 supply a clean path or remove existing files first",
                target_dir_c.display()
            )));
        }
        let mut writer = WalWriter::new(&target_dir_c, max_events, 0).map_err(|e| {
            Error::Ipc(format!(
                "knowledge_dump_wal: failed to create WalWriter: {e}"
            ))
        })?;
        let conn = db.connect()?;
        let params = crate::dump::DumpParams {
            group_id: group_id_c,
        };
        crate::dump::run_dump(&conn, &params, &mut writer)
    })
    .await;

    // On any failure: clean up partial output (FR-012).
    // If we created the directory, remove it entirely. If it pre-existed, remove only the
    // .jsonl files we wrote so we don't destroy the caller's pre-existing non-WAL files.
    let cleanup = |dir: &std::path::Path| {
        if dir_existed_before {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    if e.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        } else {
            let _ = std::fs::remove_dir_all(dir);
        }
    };

    match result {
        Ok(Ok(dump_result)) => {
            drop(_guard);
            let files_written = count_jsonl_files_in_dir(&target_dir);
            Ok(json!({
                "success": true,
                "nodes_dumped": dump_result.nodes_dumped,
                "edges_dumped": dump_result.edges_dumped,
                "files_written": files_written,
                "target_dir": target_dir.to_string_lossy(),
            }))
        }
        Ok(Err(e)) => {
            drop(_guard);
            cleanup(&target_dir);
            Err(e)
        }
        Err(join_err) => {
            drop(_guard);
            cleanup(&target_dir);
            Err(Error::Ipc(format!(
                "knowledge_dump_wal: spawn_blocking panicked: {join_err}"
            )))
        }
    }
}

fn count_jsonl_files_in_dir(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
                .count()
        })
        .unwrap_or(0)
}

async fn handle_prepare_checkpoint(state: Arc<AppState>) -> Result<Value, Error> {
    let wal_root = state.wal_root.clone();
    let state_c = Arc::clone(&state);

    // Serialize with in-flight writes (FR-006)
    let _write_guard = state.write_lock.write().await;

    let (files_flushed, files_total) =
        tokio::task::spawn_blocking(move || -> Result<(u32, u32), Error> {
            let mut files_flushed = 0u32;
            let mut files_total = 0u32;
            // Rotate every group with a live writer in this process (issue #378: an instance-wide
            // operation now spans N writers, not one).
            let mut seen_groups: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            {
                let mut writers = state_c
                    .wal_writers
                    .lock()
                    .map_err(|_| Error::Ipc("wal_writers lock poisoned".to_string()))?;
                for (gid, writer) in writers.iter_mut() {
                    let (r, t) = writer.rotate();
                    files_flushed += r;
                    files_total += t;
                    seen_groups.insert(gid.clone());
                }
            }
            // A group with a WAL directory on disk but no live writer in this process (e.g.
            // right after a fresh clear, or written only by another process) still counts
            // toward files_total, matching the pre-378 "no writer; count JSONL files" fallback.
            if let Some(root) = wal_root.as_deref() {
                if let Ok(dirs) = crate::wal_group::list_group_wal_dirs(root) {
                    for (gid, dir) in dirs {
                        if seen_groups.contains(&gid) {
                            continue;
                        }
                        files_total += count_jsonl_files_in_dir(&dir) as u32;
                    }
                }
            }
            Ok((files_flushed, files_total))
        })
        .await??;
    drop(_write_guard);

    Ok(json!({
        "success": true,
        "files_flushed": files_flushed,
        "files_total": files_total,
    }))
}

// ── WAL checkpoints (issue #365) ────────────────────────────────────────────────
//
// Distinct from `knowledge_prepare_checkpoint` above: that flushes/rotates the live WAL writer
// ahead of an external filesystem backup. These three operations record/list/remove a *named
// WAL position* — "this graph was known-good here" — for later bounded-replay recovery
// (FR-014). The two features share the word "checkpoint" by coincidence, not by relation.

/// Resolves `group_id`'s own WAL directory under `state.wal_root` (issue #378 FR-012),
/// replacing the pre-378 single-directory `require_wal_dir`. Fails with the same clear error
/// `knowledge_rebuild_from_wal` uses when no WAL root is configured at all.
fn resolve_group_wal_dir(state: &AppState, group_id: &str) -> Result<std::path::PathBuf, Error> {
    let root = state
        .wal_root
        .as_deref()
        .ok_or_else(|| Error::Ipc("No WAL directory configured (set LCG_WAL_DIR)".to_string()))?;
    let dir_name = wal_group::encode_group_dir_name(group_id)?;
    // Same case-insensitive-filesystem guard as `AppState::with_wal_writer` (issue #378) —
    // required here too: `knowledge_wal_mark_create` can be the very first operation ever
    // performed against a new group_id (checkpoint creation doesn't require a prior write), and
    // `checkpoint::create`'s `fs::create_dir_all` would otherwise interleave two case-colliding
    // groups' directories with no warning.
    wal_group::check_no_case_insensitive_collision(root, &dir_name)?;
    Ok(root.join(dir_name))
}

/// Reads an optional `group_id` param, defaulting to `DEFAULT_GROUP_ID` (issue #378 FR-012) —
/// preserves every pre-378 caller's zero-change single-group behavior.
fn group_id_param(params: &Value) -> String {
    params["group_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string()
}

/// `knowledge_wal_mark_create` — records a new named checkpoint at the database's current
/// `applied_seq` (FR-003), against `group_id`'s own WAL directory (default `"liminis"`, issue
/// #378 FR-012). O(1) relative to WAL size: no WAL file scan or replay occurs here, only a DB
/// read (`applied_seq`, and conditionally the FR-005 graph-emptiness check) and a single
/// exclusive file create under that group's `.checkpoints/`.
async fn handle_wal_mark_create(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let name = req.params["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("name is required and must be a non-empty string".to_string()))?
        .to_string();
    // Validate before touching the DB so a malformed name fails fast with a specific error
    // rather than surfacing only once checkpoint::create is reached.
    crate::checkpoint::validate_name(&name)?;

    let group_id = group_id_param(&req.params);
    let wal_dir = resolve_group_wal_dir(&state, &group_id)?;

    // Read lock only: this call never mutates the graph, but it must observe a consistent
    // applied_seq/content snapshot relative to any concurrently in-flight write.
    let _guard = state.write_lock.read().await;
    let db = load_db(&state)?;

    let gid_c = group_id.clone();
    let seq = tokio::task::spawn_blocking(move || -> Result<Option<u64>, Error> {
        let conn = db.connect()?;
        let applied_seq = conn.get_applied_seq(&gid_c)?.ok_or_else(|| {
            Error::Ipc(
                "knowledge_wal_mark_create: applied_seq is unknown (null) — the current \
                 position cannot be determined, so no checkpoint can be recorded. Resolve the \
                 unknown position (e.g. a full knowledge_rebuild_from_wal) before checkpointing."
                    .to_string(),
            )
        })?;
        if applied_seq == 0 {
            // FR-005: applied_seq == 0 conflates "nothing applied" (a genuinely fresh/cleared
            // DB) with "WAL line 0 applied" (WalWriter's global_seq starts at 0) — disambiguate
            // via the same graph-emptiness check backfill_applied_seq_if_absent already uses.
            if crate::recovery::group_has_no_content(&conn, &gid_c)? {
                return Ok(None);
            }
            return Ok(Some(0));
        }
        Ok(Some(applied_seq))
    })
    .await??;
    drop(_guard);

    let name_c = name.clone();
    tokio::task::spawn_blocking(move || crate::checkpoint::create(&wal_dir, &name_c, seq))
        .await??;

    Ok(json!({
        "success": true,
        "name": name,
        "group_id": group_id,
        "seq": seq,
    }))
}

/// `knowledge_wal_mark_list` — lists every active checkpoint with its reachability against WAL
/// content currently on disk (FR-009), for `group_id`'s own WAL directory (default `"liminis"`).
/// Filesystem-only: does not require the database to be open (FR-011), so it is exempt from the
/// degraded-mode guard in `handle`. With no explicit `group_id`, returns only the default
/// group's checkpoints — matching pre-378 single-stream behavior exactly, never aggregating
/// across every active group (FR-012).
async fn handle_wal_mark_list(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let group_id = group_id_param(&req.params);
    let wal_dir = resolve_group_wal_dir(&state, &group_id)?;
    let checkpoints =
        tokio::task::spawn_blocking(move || crate::checkpoint::list(&wal_dir)).await??;
    Ok(json!({ "group_id": group_id, "checkpoints": checkpoints }))
}

/// `knowledge_wal_mark_delete` — tombstones an active checkpoint (FR-007/FR-008) in `group_id`'s
/// own WAL directory (default `"liminis"`). Filesystem-only: does not require the database to be
/// open (FR-011), so it is exempt from the degraded-mode guard in `handle`.
async fn handle_wal_mark_delete(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let name = req.params["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("name is required and must be a non-empty string".to_string()))?
        .to_string();

    let group_id = group_id_param(&req.params);
    let wal_dir = resolve_group_wal_dir(&state, &group_id)?;
    let name_c = name.clone();
    tokio::task::spawn_blocking(move || crate::checkpoint::delete(&wal_dir, &name_c)).await??;

    Ok(json!({ "success": true, "name": name, "group_id": group_id }))
}

/// Warns (non-fatally, does not block or alter the rebuild) when a non-full (`from_seq > 0`)
/// `knowledge_rebuild_from_wal` call may have skipped a range of WAL content relative to the
/// DB's `applied_seq` before this rebuild started. `from_seq` is trusted at face value by the
/// caller of this function (per FR-004, `knowledge_rebuild_from_wal` unconditionally advances
/// `applied_seq` to the replay's `last_committed_seq` on completion) — this is a diagnostic
/// aid for operators, not a guard, since a caller may legitimately skip a known-bad or
/// already-applied-elsewhere range. `from_seq == 0` is always a full rebuild (or a resume from
/// scratch against an empty DB) and never warrants this warning.
fn warn_on_rebuild_seq_gap(
    conn: &crate::db::Conn<'_>,
    group_id: &str,
    from_seq: u64,
    context: &str,
) {
    if from_seq == 0 {
        return;
    }
    if let Ok(Some(prior)) = conn.get_applied_seq(group_id) {
        if from_seq > prior + 1 {
            eprintln!(
                "liminis-context-graph: {context}: knowledge_rebuild_from_wal called with \
                 from_seq={from_seq}, which skips seq range [{gap_start}, {gap_end}] relative \
                 to this DB's applied_seq={prior} recorded before the rebuild started — the \
                 resulting applied_seq will report the DB as caught up even though that range \
                 was never replayed into it. Expected only if the caller intentionally skipped \
                 known-bad or already-applied-elsewhere WAL content; otherwise this DB may now \
                 be missing data.",
                gap_start = prior + 1,
                gap_end = from_seq - 1,
            );
        }
    }
}

async fn handle_rebuild_from_wal(
    req: &IpcRequest,
    state: Arc<AppState>,
    progress_tx: Option<UnboundedSender<Value>>,
) -> Result<Value, Error> {
    let p = &req.params;

    let from_seq = validate_from_seq(&p["from_seq"])?;
    let to_seq = validate_to_seq(&p["to_seq"])?;
    if let Some(to_seq) = to_seq {
        if to_seq < from_seq {
            return Err(Error::Ipc(format!(
                "knowledge_rebuild_from_wal: to_seq ({to_seq}) must not be less than from_seq \
                 ({from_seq}) — bounded replay requires from_seq <= to_seq."
            )));
        }
    }
    let dry_run = p["dry_run"].as_bool().unwrap_or(false);
    let group_id = group_id_param(p);

    let wal_dir = resolve_group_wal_dir(&state, &group_id)?;

    if !wal_dir.exists() || !has_jsonl_files(&wal_dir) {
        return Err(Error::Ipc(format!(
            "No WAL files found at {}",
            wal_dir.display()
        )));
    }

    // FR-005: a `from_seq: 0` full rebuild against a group that already has data would emit
    // a native CREATE (Entity/Episodic/RelatesToNode_, see db.rs) for every existing row,
    // producing a duplicate-primary-key failure per node — a large, benign-looking failed_lines
    // count that (via FR-001's fix) still surfaces as noise, not the operator's real problem.
    // A `from_seq > 0` incremental resume intentionally targets a non-empty group (e.g.
    // resuming after a checkpoint) and must not be affected — see FR-006. Scoped to `group_id`
    // alone (issue #378 FR-006): a full rebuild of one group must not be blocked by, nor collide
    // with, another group's data.
    let force_clear = p["force_clear"].as_bool().unwrap_or(false);
    if from_seq == 0 {
        let db_for_check = load_db(&state)?;
        let gid_check = group_id.clone();
        let non_empty = tokio::task::spawn_blocking(move || -> Result<bool, Error> {
            let conn = db_for_check.connect()?;
            // An unqueryable label (older/partially-initialised schema missing the table) is
            // treated as empty, not a hard error — this guard's only job is "is there data I
            // would collide with?", and erroring here would make the rebuild/recovery tool
            // unusable in exactly the degraded situation an operator reaches for it (mirrors
            // recovery.rs's group-scoped emptiness check precedent).
            let gids = [gid_check.as_str()];
            let entity = conn.count_entities_by_group_ids(&gids).unwrap_or(0);
            let episodic = conn.count_episodics_by_group_ids(&gids).unwrap_or(0);
            let relates = conn.count_relates_to_by_group_ids(&gids).unwrap_or(0);
            Ok(entity > 0 || episodic > 0 || relates > 0)
        })
        .await??;

        if non_empty {
            if dry_run {
                // A dry run never executes Cypher (see replay_opts), so it can't reproduce the
                // duplicate-key failure either way — but dry-run is the primary way operators
                // preview a rebuild before committing to one, so it must surface this problem
                // rather than silently preview a "clean" run that would fail for real.
                return Err(Error::Ipc(format!(
                    "knowledge_rebuild_from_wal: group {group_id:?} already contains data and \
                     from_seq: 0 is a full rebuild. A dry run cannot preview this cleanly — \
                     replaying against a populated group would fail with a duplicate-primary-key \
                     error for every existing node. Clear the group first with \
                     knowledge_delete_by_group, or re-run with force_clear: true (non-dry-run) \
                     to clear it automatically before replay."
                )));
            }
            if !force_clear {
                return Err(Error::Ipc(format!(
                    "knowledge_rebuild_from_wal: group {group_id:?} already contains data and \
                     from_seq: 0 is a full rebuild. Replaying now would fail with a \
                     duplicate-primary-key error for every existing node. Pass force_clear: true \
                     to clear the group before replaying, or clear it first with \
                     knowledge_delete_by_group."
                )));
            }
            // Refuse to destructively clear the group while a write is in flight — checking
            // only in the later streaming/non-streaming branches would let the clear happen
            // before either of those checks ever runs, since this block executes first.
            let active = state.active_writes.load(Ordering::Relaxed);
            if active > 0 {
                return Err(Error::Ipc(format!(
                    "Service is busy: {active} write operation(s) in progress — wait until they complete before rebuilding"
                )));
            }
            // Issue #352 (FR-002): clear_group_for_rebuild itself does not re-derive
            // WalWriter::global_seq. This is only safe because execution always falls through
            // into one of the two real-replay paths below in this same function call, and both
            // of those call resync_global_seq_after_rebuild on completion. If
            // clear_group_for_rebuild ever grows a second caller that doesn't fall through to a
            // replay, that caller needs its own re-derivation call.
            clear_group_for_rebuild(&state, &group_id).await?;
        }
    }

    let is_streaming = progress_tx.is_some();

    if is_streaming {
        if !dry_run {
            let active = state.active_writes.load(Ordering::Relaxed);
            if active > 0 {
                return Err(Error::Ipc(format!(
                    "Service is busy: {active} write operation(s) in progress — wait until they complete before rebuilding"
                )));
            }
        }

        let db = load_db(&state)?;
        let wal_dir_c = wal_dir.clone();
        let tx = progress_tx;
        // Clone sender for the post-spawn shutdown cancel notification (R9).
        let tx_notify = tx.clone();
        // Tracks whether the cancel_fn fired specifically because of shutdown (not client
        // disconnect). Used to emit the R9 cancellation progress event only when shutdown
        // actually interrupted the replay, not when replay completed and shutdown started later.
        let shutdown_cancelled = Arc::new(AtomicBool::new(false));
        let shutdown_flag_cancel = state.cancel_token.clone();
        let shutdown_cancelled_inner = Arc::clone(&shutdown_cancelled);

        // Write lock held in async scope; guard released after spawn_blocking completes. A
        // dry run takes a *read* guard instead: it never mutates the DB, but it does read it,
        // and holding no lock at all would let a concurrent `force_clear: true` rebuild delete
        // the DB files mid-read (`active_writes` doesn't cover this — it only tracks episode
        // ingest writes, not rebuild reads).
        let _write_guard = if !dry_run {
            Some(state.write_lock.write().await)
        } else {
            None
        };
        let _read_guard = if dry_run {
            Some(state.write_lock.read().await)
        } else {
            None
        };

        let replay_started_at = std::time::Instant::now();
        let mut wal_files_total: u64 = 0;
        if let Ok(mut entries) = tokio::fs::read_dir(&wal_dir_c).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    wal_files_total += 1;
                }
            }
        }
        let bg_indices_built = Arc::clone(&state.indices_built);
        let state_c = Arc::clone(&state);
        let gid_c = group_id.clone();
        // Preset to false before the indexes are actually dropped below: if replay fails, is
        // cancelled, or the spawn_blocking join itself fails (panic), the early `?`/`??` returns
        // downstream must not leave a stale `true` in place after the indexes have already been
        // dropped — that would wrongly disable the search auto-heal path (FR-006).
        if !dry_run {
            bg_indices_built.store(false, Ordering::Release);
        }
        let (stats, indices_built) = tokio::task::spawn_blocking(
            move || -> Result<(crate::replay::ReplayStats, bool), Error> {
                // Issue #352 (FR-002): armed whenever this is a real (non-dry-run) attempt, so
                // an early `?` return below — e.g. `db.connect()` or `replay_opts` failing after
                // a `force_clear` already ran — still resyncs `global_seq` on the way out,
                // instead of only on the happy path. `mark_done()` below disarms it once the
                // normal call site has already resynced with the more accurate
                // `last_committed_seq`, so a clean run doesn't pay for a second directory scan.
                let mut resync_guard = (!dry_run)
                    .then(|| wal_exec::GlobalSeqResyncGuard::new(&state_c, &gid_c));
                let conn = db.connect()?;
                // Drop FTS + HNSW vector indexes before replay so inline index maintenance is
                // eliminated during bulk load, and so a stale pre-rebuild HNSW index (which
                // create_vector_indexes' "already exists" swallow won't refresh) doesn't survive
                // the rebuild. Errors suppressed — idempotent if already absent.
                if !dry_run {
                    crate::schema::drop_fts_indexes(&conn);
                    conn.drop_vector_indexes();
                }
                // Composite cancel: fire on client disconnect OR service shutdown (R9).
                let cancel_fn: Option<crate::replay::CancelFn> = tx.as_ref().map(|t| {
                    let t = t.clone();
                    let flag = shutdown_flag_cancel.clone();
                    let cancelled = shutdown_cancelled_inner;
                    let f: crate::replay::CancelFn = Box::new(move || {
                        let client_gone = t.is_closed();
                        let shutting_down = flag.is_cancelled();
                        if shutting_down {
                            cancelled.store(true, Ordering::Relaxed);
                        }
                        client_gone || shutting_down
                    });
                    f
                });
                let stats = WalReplayer::new(&wal_dir_c).replay_opts(
                    &conn,
                    ReplayOptions {
                        from_seq,
                        to_seq,
                        dry_run,
                        cancel_fn,
                        progress_fn: build_progress_fn(tx),
                        failure_sample_cap: None,
                        batch_size: None,
                        log_interval_override: None,
                        progress_log_fn: None,
                    },
                )?;
                // Rebuild all indexes (FTS + HNSW vector) once over the fully-loaded data.
                // Non-fatal to the replay outcome: a build failure leaves the graph unindexed
                // (the auto-heal path will recover on the first search per ADR-0025) but the
                // real outcome is captured and returned so the caller can observe it (FR-004/006)
                // rather than have it silently swallowed.
                let mut build_ok = false;
                if !dry_run {
                    match conn.build_indices_and_constraints() {
                        Ok(()) => build_ok = true,
                        Err(e) => {
                            eprintln!("liminis-context-graph: reload: end-of-reload index build failed: {e} (non-fatal)");
                        }
                    }
                    // Replay bypassed insert_entity/update_entity_created_at (issue #219) —
                    // rebuild the name index from the fully-loaded data, same non-fatal
                    // posture as the index build above.
                    if let Err(e) = conn.rebuild_name_index() {
                        eprintln!("liminis-context-graph: reload: end-of-reload name index rebuild failed: {e} (non-fatal)");
                        // FR-003: the index may now be blind to replayed entities — mark it
                        // untrusted so endpoint-authority lookups fall back to a scan until a
                        // later rebuild succeeds, instead of silently trusting a stale index.
                        conn.mark_name_index_untrusted();
                    }
                    // Issue #352: a WAL directory populated after this writer was constructed
                    // (external seed, restore, distributed-WAL pull) may contain seqs the
                    // writer doesn't know about — re-derive so the next mutation doesn't
                    // collide with what was just replayed. Only disarm the safety-net guard if
                    // this resync actually succeeded; on failure, leave it armed so `Drop`'s
                    // scan-only fallback gets a second chance instead of leaving `global_seq`
                    // stale.
                    let resynced = wal_exec::resync_global_seq_after_rebuild(
                        &state_c,
                        &gid_c,
                        stats.last_committed_seq,
                    );
                    if resynced {
                        if let Some(g) = resync_guard.as_mut() {
                            g.mark_done();
                        }
                    }
                    // Persist this group's applied-WAL-seq position at the replay's own
                    // last_committed_seq (issue #353, FR-004; scoped per group by issue #378).
                    // Non-fatal: a missed write doesn't undo the rebuild that just succeeded.
                    if let Some(seq) = stats.last_committed_seq {
                        warn_on_rebuild_seq_gap(&conn, &gid_c, from_seq, "reload");
                        if let Err(e) = conn.set_applied_seq(&gid_c, seq) {
                            eprintln!(
                                "liminis-context-graph: reload: failed to persist applied_seq={seq} (non-fatal): {e}"
                            );
                        }
                    }
                }
                Ok((stats, build_ok))
            },
        )
        .await??;
        // Unconditional store of the real outcome (FR-006): a prior successful build's `true`
        // must not survive this rebuild's failed index build, and vice versa.
        if !dry_run {
            bg_indices_built.store(indices_built, Ordering::Release);
        }
        drop(_write_guard);
        drop(_read_guard);

        state.sink.emit(TelemetryEvent::WalReplayComplete {
            ts_ms: now_ms(),
            mutations_replayed: stats.lines_replayed,
            unrecognised_lines: stats.unrecognised_lines,
            failed_lines: stats.failed_lines,
            unparseable_lines: stats.unparseable_lines,
            legacy_skipped_lines: stats.legacy_skipped_lines,
            duration_ms: replay_started_at.elapsed().as_millis() as u64,
        });

        // After a successful non-dry-run WAL replay the graph is fully under the current ontology.
        if !dry_run {
            if let Some(ref root) = state.workspace_root {
                let ontology_ref = state.ontology.as_deref();
                if let Err(e) = ontology_sidecar::write_sidecar(root, ontology_ref) {
                    eprintln!(
                        "liminis-context-graph: ontology-sidecar: WAL replay write failed {:?}: {}",
                        root, e
                    );
                } else if let Ok(mut guard) = state.ontology_drift.lock() {
                    *guard = OntologyDriftState::default();
                }
            }
        }

        // R9: emit a final progress event when shutdown interrupted the replay mid-stream.
        // Only fires when the cancel_fn actually returned true due to shutdown (not client
        // disconnect, and not when replay completed before shutdown was checked).
        if shutdown_cancelled.load(Ordering::Relaxed) {
            if let Some(ref notify_tx) = tx_notify {
                let _ = notify_tx.send(json!({
                    "type": "progress",
                    "message": "Rebuild cancelled (service shutdown)",
                    "cancelled": true,
                    "mutations_replayed_so_far": stats.lines_replayed,
                    "match_prefixed_replayed_so_far": stats.match_prefixed_replayed,
                    "files_processed_so_far": stats.files_read,
                    "files_total": wal_files_total,
                    "failed_lines_so_far": stats.failed_lines,
                    "legacy_skipped_lines_so_far": stats.legacy_skipped_lines,
                    "unrecognised_lines": stats.unrecognised_lines,
                    "failed_lines": stats.failed_lines,
                    "unparseable_lines": stats.unparseable_lines,
                    "lines_skipped": stats.lines_skipped(),
                    "failed_samples": stats.failed_samples,
                    "rolled_back_lines": stats.rolled_back_lines,
                    "last_committed_seq": stats.last_committed_seq,
                    "transactions_committed": stats.transactions_committed,
                    "transactions_rolled_back": stats.transactions_rolled_back,
                }));
            }
        }

        let mut result = json!({
            "success": true,
            "group_id": group_id,
            "from_seq": from_seq,
            "to_seq": to_seq,
            "mutations_replayed": stats.lines_replayed,
            "match_prefixed_replayed": stats.match_prefixed_replayed,
            "wal_files_processed": stats.files_read,
            "indexes_created": stats.indexes_created,
            "unrecognised_lines": stats.unrecognised_lines,
            "failed_lines": stats.failed_lines,
            "unparseable_lines": stats.unparseable_lines,
            "legacy_skipped_lines": stats.legacy_skipped_lines,
            "lines_skipped": stats.lines_skipped(),
            "failed_samples": stats.failed_samples,
            "failed_sample_categories_dropped": stats.failed_sample_categories_dropped,
            "fidelity_warning": stats.fidelity_warning,
            "rolled_back_lines": stats.rolled_back_lines,
            "last_committed_seq": stats.last_committed_seq,
            "transactions_committed": stats.transactions_committed,
            "transactions_rolled_back": stats.transactions_rolled_back,
        });
        if dry_run {
            // Dry-run never touches indices (FR-007) — omit the field rather than report
            // true/false, either of which would misleadingly imply indices were (or weren't)
            // touched "as of this rebuild".
            result["dry_run"] = json!(true);
        } else {
            result["indices_built"] = json!(indices_built);
        }
        return Ok(result);
    }

    // Non-streaming path
    if !dry_run {
        let active = state.active_writes.load(Ordering::Relaxed);
        if active > 0 {
            return Err(Error::Ipc(format!(
                "Service is busy: {active} write operation(s) in progress — wait until they complete before rebuilding"
            )));
        }
    }

    // Non-streaming dry_run: run synchronously and return stats immediately
    if dry_run {
        // A dry run never mutates the DB, but it does read it — hold a *read* guard for the
        // duration of the replay so a concurrent `force_clear: true` rebuild (which takes
        // `write_lock.write()` in `clear_db_for_rebuild`) cannot delete the DB files out from
        // under this in-flight read. `active_writes` doesn't cover this: it only tracks episode
        // ingest writes, not rebuild reads, so nothing else previously serialized these two.
        let _read_guard = state.write_lock.read().await;
        let db = load_db(&state)?;
        let wal_dir_c = wal_dir.clone();
        let replay_started_at = std::time::Instant::now();
        let stats =
            tokio::task::spawn_blocking(move || -> Result<crate::replay::ReplayStats, Error> {
                let conn = db.connect()?;
                WalReplayer::new(&wal_dir_c).replay_opts(
                    &conn,
                    ReplayOptions {
                        from_seq,
                        to_seq,
                        dry_run: true,
                        progress_fn: None,
                        cancel_fn: None,
                        failure_sample_cap: None,
                        batch_size: None,
                        log_interval_override: None,
                        progress_log_fn: None,
                    },
                )
            })
            .await??;
        state.sink.emit(TelemetryEvent::WalReplayComplete {
            ts_ms: now_ms(),
            mutations_replayed: stats.lines_replayed,
            unrecognised_lines: stats.unrecognised_lines,
            failed_lines: stats.failed_lines,
            unparseable_lines: stats.unparseable_lines,
            legacy_skipped_lines: stats.legacy_skipped_lines,
            duration_ms: replay_started_at.elapsed().as_millis() as u64,
        });
        return Ok(json!({
            "success": true,
            "group_id": group_id,
            "from_seq": from_seq,
            "to_seq": to_seq,
            "mutations_replayed": stats.lines_replayed,
            "match_prefixed_replayed": stats.match_prefixed_replayed,
            "wal_files_processed": stats.files_read,
            "indexes_created": stats.indexes_created,
            "dry_run": true,
            "unrecognised_lines": stats.unrecognised_lines,
            "failed_lines": stats.failed_lines,
            "unparseable_lines": stats.unparseable_lines,
            "legacy_skipped_lines": stats.legacy_skipped_lines,
            "lines_skipped": stats.lines_skipped(),
            "failed_samples": stats.failed_samples,
            "failed_sample_categories_dropped": stats.failed_sample_categories_dropped,
            "fidelity_warning": stats.fidelity_warning,
            "rolled_back_lines": stats.rolled_back_lines,
            "last_committed_seq": stats.last_committed_seq,
            "transactions_committed": stats.transactions_committed,
            "transactions_rolled_back": stats.transactions_rolled_back,
        }));
    }

    // Atomically check for an existing running job and register a new one (prevents FR-011 TOCTOU race)
    let job_id = {
        let mut jobs = state
            .rebuild_jobs
            .lock()
            .map_err(|_| Error::Ipc("rebuild_jobs lock poisoned".to_string()))?;
        if let Some(existing_id) = jobs
            .values()
            .find(|j| j.status == JobStatus::Running)
            .map(|j| j.job_id.clone())
        {
            return Ok(json!({
                "success": true,
                "job_id": existing_id,
                "status": "running",
            }));
        }
        let job_id = Uuid::new_v4().to_string();
        jobs.insert(job_id.clone(), RebuildJob::new(job_id.clone()));
        job_id
    };

    // Spawn background task; write lock acquired inside the task
    let db = load_db(&state)?;
    let write_lock = Arc::clone(&state.write_lock);
    let rebuild_jobs = Arc::clone(&state.rebuild_jobs);
    let rebuild_jobs_handle_store = Arc::clone(&state.rebuild_jobs);
    let job_id_task = job_id.clone();
    let job_id_handle_store = job_id.clone();
    let wal_dir_c = wal_dir.clone();
    let bg_workspace_root = state.workspace_root.clone();
    let bg_ontology = state.ontology.clone();
    let bg_ontology_drift = Arc::clone(&state.ontology_drift);
    let bg_sink = Arc::clone(&state.sink);
    let bg_indices_built = Arc::clone(&state.indices_built);
    let bg_state = Arc::clone(&state);
    let bg_gid = group_id.clone();

    let spawn_handle = tokio::spawn(async move {
        // OwnedRwLockWriteGuard is 'static + Send — safe to hold in a spawned task
        let _write_guard = if !dry_run {
            Some(write_lock.write_owned().await)
        } else {
            None
        };

        let jobs_ref = Arc::clone(&rebuild_jobs);
        let jid = job_id_task.clone();
        let replay_started_at = std::time::Instant::now();
        let ib = Arc::clone(&bg_indices_built);
        // Preset to false before the indexes are actually dropped below: if replay fails or the
        // spawn_blocking join itself fails (panic), the flag must not be left at a stale `true`
        // after the indexes have already been dropped — that would wrongly disable the search
        // auto-heal path (FR-006).
        if !dry_run {
            ib.store(false, Ordering::Release);
        }
        // bg_gid is moved into the spawn_blocking closure below; keep a clone for the
        // post-completion job_result JSON.
        let bg_gid_for_result = bg_gid.clone();

        let result = tokio::task::spawn_blocking(
            move || -> Result<(crate::replay::ReplayStats, bool), Error> {
                // Issue #352 (FR-002): armed whenever this is a real (non-dry-run) attempt, so
                // an early `?` return below — e.g. `db.connect()` or `replay_opts` failing after
                // a `force_clear` already ran — still resyncs `global_seq` on the way out,
                // instead of only on the happy path. `mark_done()` below disarms it once the
                // normal call site has already resynced with the more accurate
                // `last_committed_seq`, so a clean run doesn't pay for a second directory scan.
                let mut resync_guard =
                    (!dry_run).then(|| wal_exec::GlobalSeqResyncGuard::new(&bg_state, &bg_gid));
                let conn = db.connect()?;
                // Drop FTS + HNSW vector indexes before replay so inline index maintenance is
                // eliminated during bulk load, and so a stale pre-rebuild HNSW index doesn't
                // survive the rebuild. Errors suppressed — idempotent if already absent.
                if !dry_run {
                    crate::schema::drop_fts_indexes(&conn);
                    conn.drop_vector_indexes();
                }
                let progress_fn: Box<dyn Fn(&ReplayProgress) -> bool + Send> = Box::new(move |p| {
                    if let Ok(mut guard) = jobs_ref.lock() {
                        if let Some(job) = guard.get_mut(&jid) {
                            job.mutations_replayed = p.mutations_replayed;
                            job.wal_files_processed = p.files_processed;
                            job.wal_files_total = p.files_total;
                        }
                    }
                    true
                });
                let stats = WalReplayer::new(&wal_dir_c).replay_opts(
                    &conn,
                    ReplayOptions {
                        from_seq,
                        to_seq,
                        dry_run,
                        progress_fn: Some(progress_fn),
                        cancel_fn: None,
                        failure_sample_cap: None,
                        batch_size: None,
                        log_interval_override: None,
                        progress_log_fn: None,
                    },
                )?;
                // Rebuild all indexes (FTS + HNSW vector) once over the fully-loaded data.
                // Non-fatal to the replay outcome; the real outcome is captured and returned
                // (FR-004/006) instead of being silently swallowed.
                let mut build_ok = false;
                if !dry_run {
                    match conn.build_indices_and_constraints() {
                        Ok(()) => build_ok = true,
                        Err(e) => {
                            eprintln!("liminis-context-graph: reload(bg): end-of-reload index build failed: {e} (non-fatal)");
                        }
                    }
                    // Replay bypassed insert_entity/update_entity_created_at (issue #219) —
                    // rebuild the name index from the fully-loaded data, same non-fatal
                    // posture as the index build above.
                    if let Err(e) = conn.rebuild_name_index() {
                        eprintln!("liminis-context-graph: reload(bg): end-of-reload name index rebuild failed: {e} (non-fatal)");
                        // FR-003: the index may now be blind to replayed entities — mark it
                        // untrusted so endpoint-authority lookups fall back to a scan until a
                        // later rebuild succeeds, instead of silently trusting a stale index.
                        conn.mark_name_index_untrusted();
                    }
                    // Issue #352: a WAL directory populated after this writer was constructed
                    // (external seed, restore, distributed-WAL pull) may contain seqs the
                    // writer doesn't know about — re-derive so the next mutation doesn't
                    // collide with what was just replayed. Only disarm the safety-net guard if
                    // this resync actually succeeded; on failure, leave it armed so `Drop`'s
                    // scan-only fallback gets a second chance instead of leaving `global_seq`
                    // stale.
                    let resynced = wal_exec::resync_global_seq_after_rebuild(
                        &bg_state,
                        &bg_gid,
                        stats.last_committed_seq,
                    );
                    if resynced {
                        if let Some(g) = resync_guard.as_mut() {
                            g.mark_done();
                        }
                    }
                    // Persist this group's applied-WAL-seq position at the replay's own
                    // last_committed_seq (issue #353, FR-004; scoped per group by issue #378).
                    // Non-fatal: a missed write doesn't undo the rebuild that just succeeded.
                    if let Some(seq) = stats.last_committed_seq {
                        warn_on_rebuild_seq_gap(&conn, &bg_gid, from_seq, "reload(bg)");
                        if let Err(e) = conn.set_applied_seq(&bg_gid, seq) {
                            eprintln!(
                                "liminis-context-graph: reload(bg): failed to persist applied_seq={seq} (non-fatal): {e}"
                            );
                        }
                    }
                }
                Ok((stats, build_ok))
            },
        )
        .await;

        // Unconditional store of the real outcome (FR-006), mirroring the streaming path. Stored
        // while the write lock is still held so a concurrent search never observes a stale value
        // in the gap between the build outcome landing and the lock being released.
        if let Ok(Ok((_, build_ok))) = &result {
            if !dry_run {
                ib.store(*build_ok, Ordering::Release);
            }
        }

        drop(_write_guard);

        if let Ok(mut jobs) = rebuild_jobs.lock() {
            if let Some(job) = jobs.get_mut(&job_id_task) {
                match result {
                    Ok(Ok((stats, indices_built))) => {
                        job.status = JobStatus::Completed;
                        job.mutations_replayed = stats.lines_replayed;
                        job.wal_files_processed = stats.files_read;
                        bg_sink.emit(TelemetryEvent::WalReplayComplete {
                            ts_ms: now_ms(),
                            mutations_replayed: stats.lines_replayed,
                            unrecognised_lines: stats.unrecognised_lines,
                            failed_lines: stats.failed_lines,
                            unparseable_lines: stats.unparseable_lines,
                            legacy_skipped_lines: stats.legacy_skipped_lines,
                            duration_ms: replay_started_at.elapsed().as_millis() as u64,
                        });
                        let mut job_result = json!({
                            "group_id": bg_gid_for_result,
                            "from_seq": from_seq,
                            "to_seq": to_seq,
                            "mutations_replayed": stats.lines_replayed,
                            "match_prefixed_replayed": stats.match_prefixed_replayed,
                            "wal_files_processed": stats.files_read,
                            "indexes_created": stats.indexes_created,
                            "dry_run": dry_run,
                            "unrecognised_lines": stats.unrecognised_lines,
                            "failed_lines": stats.failed_lines,
                            "unparseable_lines": stats.unparseable_lines,
                            "legacy_skipped_lines": stats.legacy_skipped_lines,
                            "lines_skipped": stats.lines_skipped(),
                            "failed_samples": stats.failed_samples,
                            "failed_sample_categories_dropped": stats.failed_sample_categories_dropped,
                            "fidelity_warning": stats.fidelity_warning,
                            "rolled_back_lines": stats.rolled_back_lines,
                            "last_committed_seq": stats.last_committed_seq,
                            "transactions_committed": stats.transactions_committed,
                            "transactions_rolled_back": stats.transactions_rolled_back,
                        });
                        // Dry-run never touches indices (FR-007) — omit the field.
                        if !dry_run {
                            job_result["indices_built"] = json!(indices_built);
                        }
                        job.result = Some(job_result);
                        // Update the sidecar so drift clears after a successful WAL rebuild.
                        if !dry_run {
                            if let Some(ref root) = bg_workspace_root {
                                let ontology_ref = bg_ontology.as_deref();
                                if let Err(e) = ontology_sidecar::write_sidecar(root, ontology_ref)
                                {
                                    eprintln!(
                                        "liminis-context-graph: ontology-sidecar: bg WAL replay write failed {:?}: {}",
                                        root, e
                                    );
                                } else if let Ok(mut guard) = bg_ontology_drift.lock() {
                                    *guard = OntologyDriftState::default();
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        job.status = JobStatus::Failed;
                        job.error = Some(e.to_string());
                    }
                    Err(e) => {
                        job.status = JobStatus::Failed;
                        job.error = Some(format!("Task panicked: {e}"));
                    }
                }
            }
        }
    });

    // Store the JoinHandle in the job record so shutdown can abort and await it.
    if let Ok(mut jobs) = rebuild_jobs_handle_store.lock() {
        if let Some(job) = jobs.get_mut(&job_id_handle_store) {
            job.spawn_handle = Some(spawn_handle);
        }
    }

    Ok(json!({
        "success": true,
        "job_id": job_id,
        "status": "running",
    }))
}

/// Clears the lbug database (never the WAL directory — the caller is about to replay it) so a
/// `from_seq: 0` `knowledge_rebuild_from_wal` call can proceed without duplicate-primary-key
/// collisions (FR-005). Mirrors `recover_rebuild_from_workspace_wal`'s DB-file-delete + reopen +
/// `state.db` ArcSwap hot-swap pattern (ADR-0003), scoped identically: only the lbug DB file/dir
/// and its `.wal`/`.lock` sidecars are removed.
/// Clears `group_id`'s own graph data (never any other group's, and never the WAL directory —
/// the caller is about to replay it) so a `from_seq: 0` `knowledge_rebuild_from_wal` call can
/// proceed without duplicate-primary-key collisions for that group (FR-005, scoped per group by
/// issue #378 FR-006). Unlike the pre-378 whole-DB-file delete + reopen, this purges via
/// `group_purge::purge_groups` against the already-open DB — group_purge already handles the
/// forced rebind of any foreign pointer into the cleared group within the same transaction, so
/// no `state.db` swap or file deletion is needed here.
async fn clear_group_for_rebuild(state: &Arc<AppState>, group_id: &str) -> Result<(), Error> {
    let _guard = state.write_lock.write().await;
    let db = load_db(state)?;
    let state_c = Arc::clone(state);
    let gid = group_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        let conn = db.connect()?;
        let ts = chrono::Utc::now().to_rfc3339();
        group_purge::purge_groups(&conn, &[gid.as_str()], &ts, false)?;
        // The forced rebind inside purge_groups can touch a foreign, non-purged group's
        // RelatesToNode_ rows in the same transaction (ADR-0361) — genuinely multi-group, so
        // per-operation attribution can't name one group (FR-004). Routes through the default
        // group's writer, the same documented limitation as handle_delete_by_group.
        wal_exec::wal_flush_ungrouped(&state_c, DEFAULT_GROUP_ID, conn.drain_mutations());
        // Reset this group's applied-WAL-seq position (issue #353 FR-005, scoped per group by
        // issue #378) — its data is freshly empty. The from_seq: 0 rebuild this function exists
        // to enable will overwrite this with the replay's precise last_committed_seq once it
        // completes.
        conn.set_applied_seq(&gid, 0)?;
        Ok(())
    })
    .await??;

    state.indices_built.store(false, Ordering::Release);
    Ok(())
}

async fn handle_rebuild_status(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let job_id = req.params["job_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Ipc("job_id is required and must be non-empty".to_string()))?
        .to_string();

    let jobs = state
        .rebuild_jobs
        .lock()
        .map_err(|_| Error::Ipc("rebuild_jobs lock poisoned".to_string()))?;

    let Some(job) = jobs.get(&job_id) else {
        return Ok(json!({"status": "not_found"}));
    };

    Ok(json!({
        "job_id": job.job_id,
        "status": job.status.as_str(),
        "mutations_replayed": job.mutations_replayed,
        "wal_files_processed": job.wal_files_processed,
        "wal_files_total": job.wal_files_total,
        "start_time": job.start_time.to_rfc3339(),
        "elapsed_seconds": job.elapsed_seconds(),
        "error": job.error,
        "result": job.result,
    }))
}

// ── Corrections handlers ──────────────────────────────────────────────────────

async fn handle_validate_corrections(state: Arc<AppState>) -> Result<Value, Error> {
    let workspace_root = state.workspace_root.clone().ok_or_else(|| {
        Error::Ipc("LIMINIS_WORKSPACE_ROOT not set; corrections unavailable".to_string())
    })?;

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let result = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        Ok::<_, Error>(corrections::validate_corrections_file(
            &conn,
            &workspace_root,
        ))
    })
    .await??;
    drop(_guard);

    Ok(json!({
        "valid": result.valid,
        "message": result.message,
        "total_corrections": result.total_corrections,
        "unapplied_corrections": result.unapplied_corrections,
        "issues": result.issues,
        "warnings": result.warnings,
    }))
}

async fn handle_apply_corrections(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let workspace_root = state.workspace_root.clone().ok_or_else(|| {
        Error::Ipc("LIMINIS_WORKSPACE_ROOT not set; corrections unavailable".to_string())
    })?;

    let dry_run = req.params["dry_run"].as_bool().unwrap_or(false);

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let _guard = state.write_lock.write().await;
    let result = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        let apply_result = corrections::apply_corrections_file(&conn, &workspace_root, dry_run);
        if !dry_run {
            // A corrections file can address entities across several groups in one call, with
            // no single group_id in scope here (FR-004). Routes through the default group's
            // writer, the same documented limitation as handle_delete_by_group.
            wal_exec::wal_flush_ungrouped(&state_c, DEFAULT_GROUP_ID, conn.drain_mutations());
        }
        Ok::<_, Error>(apply_result)
    })
    .await??;
    drop(_guard);

    let mut resp = json!({
        "success": result.success,
        "applied": result.applied,
        "skipped": result.skipped,
        "errors": result.errors,
        "details": result.details,
    });
    if let Some(msg) = result.message {
        resp["message"] = json!(msg);
    }
    Ok(resp)
}

async fn handle_merge_entities(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let dry_run = p["dry_run"].as_bool().unwrap_or(false);

    let params = corrections::MergeEntitiesParams {
        canonical_uuid: p["canonical_uuid"].as_str().map(|s| s.to_string()),
        canonical_name: p["canonical_name"].as_str().map(|s| s.to_string()),
        alias_uuids: p["alias_uuids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        alias_names: p["alias_names"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        merge_all_by_name: p["merge_all_by_name"].as_bool().unwrap_or(false),
        group_id: p["group_id"]
            .as_str()
            .unwrap_or(DEFAULT_GROUP_ID)
            .to_string(),
        dry_run,
    };

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let gid_c = params.group_id.clone();
    let _guard = state.write_lock.write().await;
    let result = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        let ts = chrono::Utc::now().to_rfc3339();
        let merge_result = corrections::merge_entities(&conn, &params, &ts);
        if !dry_run {
            // Since #371, merge_entities_inner never writes into a group other than the one
            // owning the merge — this call's mutations all belong to gid_c, so (unlike the
            // FR-004-exempt sites) per-operation attribution names it directly.
            wal_exec::wal_flush_ungrouped(&state_c, &gid_c, conn.drain_mutations());
        }
        Ok::<_, Error>(merge_result)
    })
    .await??;
    drop(_guard);

    let mut resp = json!({
        "success": result.success,
        "canonical_uuid": result.canonical_uuid,
        "merged_count": result.merged_count,
        "skipped": result.skipped,
        "edges_rewritten": result.edges_rewritten,
        "edges_deduplicated": result.edges_deduplicated,
        "foreign_edges_skipped": result.foreign_edges_skipped,
        "errors": result.errors,
    });

    if let Some(plan) = result.plan {
        let aliases: Vec<Value> = plan
            .aliases
            .into_iter()
            .map(|a| {
                json!({
                    "uuid": a.uuid,
                    "name": a.name,
                    "active_edges": a.active_edges,
                    "duplicate_edges": a.duplicate_edges,
                    "foreign_edges_skipped": a.foreign_edges_skipped,
                })
            })
            .collect();
        resp["plan"] = json!({
            "aliases": aliases,
            "total_edges_rewritten": plan.total_edges_rewritten,
            "total_edges_collapsed": plan.total_edges_collapsed,
            "total_foreign_edges_skipped": plan.total_foreign_edges_skipped,
        });
    }

    Ok(resp)
}

/// Parses one `EndpointSpec` from its JSON wire shape: `{"uuid": "..."}` for an endpoint
/// already known to live in the edge's own `group_id`, or `{"source_group_id": "...",
/// "endpoint_name": "..."}` for a foreign endpoint to be resolved (issue #369 FR-002).
fn parse_endpoint_spec(v: &Value) -> Result<EndpointSpec, Error> {
    let uuid = v["uuid"].as_str();
    let source_group_id = v["source_group_id"].as_str();
    let endpoint_name = v["endpoint_name"].as_str();
    match (uuid, source_group_id, endpoint_name) {
        (Some(uuid), None, None) => Ok(EndpointSpec::Uuid(uuid.to_string())),
        (None, Some(source_group_id), Some(endpoint_name)) => Ok(EndpointSpec::Foreign {
            source_group_id: source_group_id.to_string(),
            endpoint_name: endpoint_name.to_string(),
        }),
        // A request mixing 'uuid' with 'source_group_id'/'endpoint_name' is rejected rather
        // than silently preferring 'uuid' and discarding the foreign pointer fields — an
        // ambiguous request must not be interpreted as if it were a clean intra-group one
        // (FR-002/SC-005: "rejected loudly", not silently downgraded).
        _ => Err(Error::Ipc(
            "endpoint must have either 'uuid' alone, or both 'source_group_id' and \
             'endpoint_name' alone — not a mix, and not a partial foreign pair"
                .to_string(),
        )),
    }
}

fn pointer_to_json(p: &pointer::CrossGroupPointer) -> Value {
    serde_json::to_value(p).unwrap_or(Value::Null)
}

async fn handle_add_cross_group_edge(
    req: &IpcRequest,
    state: Arc<AppState>,
) -> Result<Value, Error> {
    let p = &req.params;
    let name = p["name"].as_str().unwrap_or("").to_string();
    let group_id = p["group_id"]
        .as_str()
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string();
    let fact = p["fact"].as_str().unwrap_or("").to_string();
    let valid_at = p["valid_at"].as_str().map(|s| s.to_string());
    let relation_type = p["relation_type"].as_str().map(|s| s.to_string());
    let source = parse_endpoint_spec(&p["source"])?;
    let target = parse_endpoint_spec(&p["target"])?;

    let fact_embedding = state.embedder.embed(&fact).await?;

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let gid_c = group_id.clone();
    let _guard = state.write_lock.write().await;
    let edge = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        let ts = chrono::Utc::now().to_rfc3339();
        let edge = cross_group::create_cross_group_edge(
            &conn,
            CreateCrossGroupEdgeParams {
                name,
                source,
                target,
                group_id,
                fact,
                fact_embedding,
                valid_at,
                relation_type,
            },
            &ts,
        )?;
        // The new RelatesToNode_ edge is owned by gid_c alone — its two endpoints may point into
        // other groups, but the mutation itself belongs to exactly one group (FR-004).
        wal_exec::wal_flush_ungrouped(&state_c, &gid_c, conn.drain_mutations());
        Ok::<_, Error>(edge)
    })
    .await??;
    drop(_guard);

    let pointers = pointer::read_pointers(&edge.attributes);
    Ok(json!({
        "uuid": edge.uuid,
        "name": edge.name,
        "group_id": edge.group_id,
        "source_node_uuid": edge.source_node_uuid,
        "target_node_uuid": edge.target_node_uuid,
        "fact": edge.fact,
        "cross_group_pointers": {
            "src": pointers.get(EndpointSide::Src).map(pointer_to_json),
            "dst": pointers.get(EndpointSide::Dst).map(pointer_to_json),
        }
    }))
}

async fn handle_rebind_pointers(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let source_group_id = req.params["source_group_id"]
        .as_str()
        .ok_or_else(|| Error::Ipc("source_group_id is required".to_string()))?
        .to_string();

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let _guard = state.write_lock.write().await;
    let counts = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        let ts = chrono::Utc::now().to_rfc3339();
        let counts = cross_group::rebind_pointers(&conn, &source_group_id, &ts)?;
        // Rebinds every pointer whose source_group_id matches, regardless of which group owns
        // the RelatesToNode_ row carrying it — genuinely multi-group, so per-operation
        // attribution can't name one group (FR-004). Routes through the default group's writer.
        wal_exec::wal_flush_ungrouped(&state_c, DEFAULT_GROUP_ID, conn.drain_mutations());
        Ok::<_, Error>(counts)
    })
    .await??;
    drop(_guard);

    Ok(json!({
        "checked": counts.checked,
        "bound": counts.bound,
        "unbound": counts.unbound,
        "ambiguous": counts.ambiguous,
        "invalidated_self_loop": counts.invalidated_self_loop,
        "invalidated_duplicate": counts.invalidated_duplicate,
    }))
}

/// Extracts a JSON object param as its serialized string form, defaulting to `"{}"` when
/// absent or not an object — the shared `attributes` convention both assert tools use (issue
/// #379 FR-005/FR-020), matching the existing `attributes: String` field on entity/edge rows.
fn attributes_param_to_string(p: &Value) -> String {
    match p {
        Value::Object(_) => p.to_string(),
        _ => "{}".to_string(),
    }
}

/// `knowledge_assert_entity` (issue #379): creates or updates a single entity identified by
/// `name` (or, if `entity_uuid` is supplied, by that strict group-scoped UUID lookup — FR-007)
/// within `group_id`. Resolution (including forward-through-`Merged` per FR-008/FR-009) is
/// delegated to `assert::resolve_entity_by_name`/`resolve_entity_by_uuid`; this handler owns
/// only param parsing, the embed-with-fallback step (FR-012/FR-026), and the create-vs-update
/// branch (FR-010/FR-011). `summary` always fully replaces the prior value — a re-assert that
/// omits it clears it, matching `attributes`/`labels`.
async fn handle_assert_entity(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let name = p["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::Ipc("name is required".to_string()))?
        .to_string();
    let entity_uuid = p["entity_uuid"].as_str().map(|s| s.to_string());
    let group_id = p["group_id"]
        .as_str()
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string();
    let labels: Vec<String> = p["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["Entity".to_string()]);
    let summary = p["summary"].as_str().unwrap_or("").to_string();
    let attributes = attributes_param_to_string(&p["attributes"]);

    let (name_embedding, embed_err) = match state.embedder.embed(&name).await {
        Ok(v) => (v, None),
        // `Entity.name_embedding` is a fixed-size `FLOAT[N]` column (Kuzu ARRAY, not a
        // variable-length LIST) — a literal zero-length vector fails to bind with a
        // Conversion exception ("Unsupported casting LIST with incorrect list entry to
        // ARRAY"). A same-dimension zero vector is the only physically valid stand-in for
        // "no embedding" this schema can store on create; an update leaves the existing
        // stored embedding untouched instead (see `update_entity_core`), so the warning text
        // is finalized below once we know which branch ran.
        Err(e) => (vec![0.0f32; state.embedder.dim()], Some(e.to_string())),
    };

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let gid_wal = group_id.clone();
    let _guard = state.write_lock.write().await;
    let (entity_uuid_out, created) = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;

        let resolved = if let Some(ref eu) = entity_uuid {
            Some(assert::resolve_entity_by_uuid(&conn, &group_id, eu)?)
        } else {
            match assert::resolve_entity_by_name(&conn, &group_id, &name)? {
                assert::Resolved::Existing(existing) => Some(*existing),
                assert::Resolved::NotFound => None,
            }
        };

        let (uuid, created) = match resolved {
            Some(existing) => {
                // Resolution (by uuid, or by name forwarded through a Merged tombstone) can
                // land `existing` on an entity whose current name differs from the
                // caller-supplied `name` — an implicit rename. Guard against that rename
                // silently colliding with a *different active* entity already holding the
                // target name in this group: `get_entity_by_name_ci`/`_with_scan_fallback`
                // return one deterministic winner regardless of `Merged` status, so an
                // unguarded rename could make one of two same-named active entities
                // unreachable by future by-name resolution (including
                // `knowledge_assert_relationship`'s endpoint resolution), undermining FR-011's
                // "(name, group_id)" idempotency key. `count_active_entities_by_name_ci`
                // excludes `Merged` tombstones (matching `cross_group::resolve_endpoint`'s own
                // ambiguity-detection convention), so reusing a still-Merged tombstone's old
                // name — as the by-name Merged-forwarding path routinely does — is correctly
                // not treated as a collision.
                if existing.name.trim().to_lowercase() != name.trim().to_lowercase()
                    && conn.count_active_entities_by_name_ci(&name, &group_id)? > 0
                {
                    return Err(Error::Ipc(format!(
                        "cannot rename entity '{}' to '{name}': an active entity in group \
                         '{group_id}' already uses that name",
                        existing.uuid
                    )));
                }
                conn.update_entity_core(&existing, &name, &labels, &summary, &attributes)?;
                (existing.uuid, false)
            }
            None => {
                let ts = chrono::Utc::now().to_rfc3339();
                let row = EntityRow {
                    uuid: Uuid::new_v4().to_string(),
                    name,
                    group_id,
                    labels,
                    created_at: ts,
                    name_embedding,
                    summary,
                    attributes,
                    episode_uuids: vec![],
                    source_descriptions: vec![],
                };
                conn.insert_entity(&row)?;
                (row.uuid, true)
            }
        };

        wal_exec::wal_flush_ungrouped(&state_c, &gid_wal, conn.drain_mutations());
        Ok::<_, Error>((uuid, created))
    })
    .await??;
    drop(_guard);

    let embedding_warning = embed_err.map(|e| {
        if created {
            format!("embedder unavailable; stored a zero-vector name_embedding: {e}")
        } else {
            format!("embedder unavailable; existing name_embedding left unchanged: {e}")
        }
    });

    Ok(json!({
        "entity_uuid": entity_uuid_out,
        "created": created,
        "embedding_warning": embedding_warning,
    }))
}

/// `knowledge_assert_relationship` (issue #379): creates or updates a single directed edge
/// between two entities resolved by name — strictly within the call's own `group_id`
/// (FR-013/FR-014), never falling back to a cross-group search — naming
/// `knowledge_add_cross_group_edge` in the error when a name doesn't resolve (FR-015). The
/// upsert match is `(source_node_uuid, predicate, target_node_uuid, group_id)` via
/// `find_active_relates_to_uuid` (FR-016/FR-017): `predicate` is bound to `RelatesToEdge.name`,
/// the same field `has_directed_edge`/`knowledge_add_cross_group_edge` already treat as an
/// edge's identity label, not a free-text description. `valid_at`, when supplied, is validated
/// and normalized before any write (FR-022).
async fn handle_assert_relationship(
    req: &IpcRequest,
    state: Arc<AppState>,
) -> Result<Value, Error> {
    let p = &req.params;
    let source_name = p["source_name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::Ipc("source_name is required".to_string()))?
        .to_string();
    let target_name = p["target_name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::Ipc("target_name is required".to_string()))?
        .to_string();
    let predicate = p["predicate"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::Ipc("predicate is required".to_string()))?
        .to_string();
    let group_id = p["group_id"]
        .as_str()
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string();
    let fact = p["fact"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{source_name} {predicate} {target_name}"));
    let attributes = attributes_param_to_string(&p["attributes"]);
    let relation_type = p["relation_type"].as_str().map(|s| s.to_string());
    let valid_at = match p["valid_at"].as_str() {
        Some(raw) => Some(db::validate_and_normalize_valid_at(raw)?),
        None => None,
    };

    let (fact_embedding, embed_err) = match state.embedder.embed(&fact).await {
        Ok(v) => (v, None),
        // See the matching comment in handle_assert_entity: RelatesToNode_.fact_embedding
        // is a fixed-size FLOAT[N] column, so a same-dimension zero vector — not a
        // literal empty Vec — is the only physically valid "no embedding" stand-in on create;
        // an update leaves the existing stored embedding untouched (see
        // `update_relates_to_core`), so the warning text is finalized below once we know
        // which branch ran.
        Err(e) => (vec![0.0f32; state.embedder.dim()], Some(e.to_string())),
    };

    let db = load_db(&state)?;
    let state_c = Arc::clone(&state);
    let gid_wal = group_id.clone();
    let _guard = state.write_lock.write().await;
    let (edge_uuid, created) = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;

        let resolve_or_err = |n: &str| -> Result<EntityRow, Error> {
            match assert::resolve_entity_by_name(&conn, &group_id, n)? {
                assert::Resolved::Existing(existing) => Ok(*existing),
                assert::Resolved::NotFound => Err(Error::Ipc(format!(
                    "'{n}' does not exist in group '{group_id}' — \
                     knowledge_assert_relationship only resolves endpoints within its own \
                     group_id and never falls back to a cross-group search; use \
                     knowledge_add_cross_group_edge to connect entities across groups"
                ))),
            }
        };
        let source = resolve_or_err(&source_name)?;
        let target = resolve_or_err(&target_name)?;

        let existing_uuid =
            conn.find_active_relates_to_uuid(&source.uuid, &target.uuid, &predicate, &group_id)?;

        let (uuid, created) = match existing_uuid {
            Some(uuid) => {
                conn.update_relates_to_core(
                    &uuid,
                    &fact,
                    valid_at.as_deref(),
                    relation_type.as_deref(),
                    &attributes,
                )?;
                (uuid, false)
            }
            None => {
                let ts = chrono::Utc::now().to_rfc3339();
                let edge = RelatesToEdge {
                    uuid: Uuid::new_v4().to_string(),
                    name: predicate,
                    source_node_uuid: source.uuid,
                    target_node_uuid: target.uuid,
                    group_id,
                    fact,
                    fact_embedding,
                    created_at: ts,
                    valid_at,
                    invalid_at: None,
                    attributes,
                    relation_type,
                    episode_uuids: vec![],
                    source_descriptions: vec![],
                };
                conn.insert_relates_to_edge(&edge)?;
                (edge.uuid, true)
            }
        };

        wal_exec::wal_flush_ungrouped(&state_c, &gid_wal, conn.drain_mutations());
        Ok::<_, Error>((uuid, created))
    })
    .await??;
    drop(_guard);

    let embedding_warning = embed_err.map(|e| {
        if created {
            format!("embedder unavailable; stored a zero-vector fact_embedding: {e}")
        } else {
            format!("embedder unavailable; existing fact_embedding left unchanged: {e}")
        }
    });

    Ok(json!({
        "edge_uuid": edge_uuid,
        "created": created,
        "embedding_warning": embedding_warning,
    }))
}

async fn handle_reprocess_entity_types(
    req: &IpcRequest,
    state: Arc<AppState>,
    progress_tx: Option<UnboundedSender<Value>>,
) -> Result<Value, Error> {
    state.workspace_root.as_ref().ok_or_else(|| {
        Error::Ipc("LIMINIS_WORKSPACE_ROOT not set; corrections unavailable".to_string())
    })?;

    let group_id = req.params["group_id"]
        .as_str()
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string();

    // Parse scope (default: untyped preserves pre-#177 behavior).
    let scope = match req.params["scope"].as_str().unwrap_or("untyped") {
        "untyped" => corrections::ReprocessScope::Untyped,
        "off_ontology" => corrections::ReprocessScope::OffOntology,
        "all" => corrections::ReprocessScope::All,
        other => {
            return Ok(json!({
                "success": false,
                "error": format!(
                    "unknown scope '{other}'; valid values: untyped, off_ontology, all"
                ),
            }));
        }
    };
    let dry_run = req.params["dry_run"].as_bool().unwrap_or(false);

    // Scopes that constrain classification to the ontology require an ontology to be loaded.
    let requires_ontology = matches!(
        scope,
        corrections::ReprocessScope::OffOntology | corrections::ReprocessScope::All
    );
    if requires_ontology && state.ontology.is_none() {
        return Ok(json!({
            "success": false,
            "error": format!(
                "scope '{scope_str}' requires an ontology to be configured",
                scope_str = if matches!(scope, corrections::ReprocessScope::OffOntology) {
                    "off_ontology"
                } else {
                    "all"
                }
            ),
        }));
    }

    // Pre-extract ancestor_map and allowed type names before async/spawn_blocking boundaries.
    let ancestor_map: HashMap<String, Vec<String>> = state
        .ontology
        .as_deref()
        .map(|o| o.ancestor_map.clone())
        .unwrap_or_default();
    let ontology_type_names: Option<std::collections::HashSet<String>> = if requires_ontology {
        let names = state.ontology.as_deref().unwrap().entity_type_names();
        if names.is_empty() {
            return Ok(json!({
                "success": false,
                "error": "the configured ontology declares no entity types; scope requires at \
                          least one declared entity type to constrain classification",
            }));
        }
        Some(names)
    } else {
        None
    };

    // Phase A (read lock): collect candidate entities based on scope.
    let db = load_db(&state)?;
    let group_id_a = group_id.clone();
    let ontology_type_names_a = ontology_type_names.clone();
    let _read_guard = state.write_lock.read().await;
    let entities = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        corrections::list_entities_for_scope(
            &conn,
            &group_id_a,
            scope,
            ontology_type_names_a.as_ref(),
        )
    })
    .await??;
    drop(_read_guard);

    if entities.is_empty() {
        if dry_run {
            return Ok(json!({ "would_reclassify_count": 0, "plan": [] }));
        }
        return Ok(json!({
            "success": true,
            "reclassified_count": 0,
            "unchanged_count": 0,
            "group_id": group_id,
        }));
    }

    // Build the allowed_types list for constrained scopes (sorted for deterministic prompts).
    let allowed_types: Option<Vec<String>> = ontology_type_names.map(|tn| {
        let mut v: Vec<String> = tn.into_iter().collect();
        v.sort_unstable();
        v
    });

    // Phase B (no lock): classify entities via LLM in batches.
    let pairs: Vec<(String, String)> = entities
        .iter()
        .map(|e| (e.name.clone(), e.summary.clone()))
        .collect();

    let total = entities.len();
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(json!({
            "type": "progress",
            "phase": "classifying",
            "processed": 0,
            "total": total,
        }));
    }
    let mut types: Vec<String> = Vec::with_capacity(total);
    for (i, chunk) in pairs.chunks(corrections::REPROCESS_BATCH_SIZE).enumerate() {
        let processed = i * corrections::REPROCESS_BATCH_SIZE;
        if processed > 0 && processed.is_multiple_of(REPROCESS_PROGRESS_EVERY) {
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
            .map(|(n, s)| (n.as_str(), s.as_str()))
            .collect();
        match state
            .extractor
            .classify_entities(&refs, allowed_types.as_deref())
            .await
        {
            Ok(mut batch) => {
                batch.resize(chunk.len(), String::new());
                types.extend(batch);
            }
            Err(e) => {
                return Ok(json!({
                    "success": false,
                    "group_id": group_id,
                    "error": format!("Failed to reprocess entity types: {e}"),
                }));
            }
        }
    }

    // Build plan + updates, applying idempotency and low-confidence skips.
    let mut plan: Vec<serde_json::Value> = Vec::new();
    let mut updates: Vec<(String, String)> = Vec::new();
    let mut unchanged_count: usize = 0;
    for (entity, assigned_type) in entities.iter().zip(types.iter()) {
        if assigned_type.is_empty() {
            // LLM returned no assignment (FR-010): leave unchanged.
            unchanged_count += 1;
            continue;
        }
        let current_leaf = corrections::find_leaf_type(&entity.labels, &ancestor_map);
        if current_leaf.as_deref() == Some(assigned_type.as_str()) {
            // Already has the correct type (FR-009): no write needed.
            unchanged_count += 1;
            continue;
        }
        plan.push(json!({
            "entity_id": entity.uuid,
            "entity_name": entity.name,
            "old_type": current_leaf,
            "new_type": assigned_type,
        }));
        updates.push((entity.uuid.clone(), assigned_type.clone()));
    }

    if dry_run {
        return Ok(json!({
            "would_reclassify_count": plan.len(),
            "plan": plan,
        }));
    }

    // Phase C (batched write lock per ADR-0030): apply label mutations in batches.
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(json!({
            "type": "progress",
            "phase": "writing",
            "total_mutations": updates.len(),
        }));
    }

    let mut reclassified = 0usize;
    for (batch_idx, batch) in updates
        .chunks(corrections::REPROCESS_BATCH_SIZE)
        .enumerate()
    {
        let batch = batch.to_vec();
        let db = load_db(&state)?;
        let state_c = Arc::clone(&state);
        let gid_c = group_id.clone();
        let ancestor_map_c = ancestor_map.clone();
        let _write_guard = state.write_lock.write().await;

        let processed = batch_idx * corrections::REPROCESS_BATCH_SIZE;
        if processed > 0 && processed.is_multiple_of(REPROCESS_PROGRESS_EVERY) {
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
            let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
            let count = corrections::apply_entity_type_labels(&conn, &batch, &ancestor_map_c)?;
            wal_exec::wal_flush_ungrouped(&state_c, &gid_c, conn.drain_mutations());
            Ok(count)
        })
        .await??;
        reclassified += count;
        drop(_write_guard);
    }

    // Phase D (write lock): re-stamp typed entities that are missing ancestor labels.
    // Skip if no ontology is loaded or no hierarchy is declared.
    // `ancestor_map` has an entry per type (even with empty ancestor lists for flat types),
    // so we check whether any type actually has declared ancestors rather than map emptiness.
    let restamped = if ancestor_map.values().any(|v| !v.is_empty()) {
        if let Some(ref tx) = progress_tx {
            let _ = tx.send(json!({
                "type": "progress",
                "phase": "restamping",
            }));
        }

        let db = load_db(&state)?;
        let state_d = Arc::clone(&state);
        let group_id_d = group_id.clone();
        let ancestor_map_d = ancestor_map.clone();
        let progress_tx_d = progress_tx.clone();
        let _write_guard_d = state.write_lock.write().await;
        tokio::task::spawn_blocking(move || -> Result<usize, Error> {
            let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
            let typed = corrections::list_all_typed_entities(&conn, &group_id_d)?;
            let total_typed = typed.len();
            let mut restamped_count = 0usize;
            for (idx, entity) in typed.iter().enumerate() {
                if idx > 0 && idx.is_multiple_of(REPROCESS_PROGRESS_EVERY) {
                    if let Some(ref tx) = progress_tx_d {
                        let _ = tx.send(json!({
                            "type": "progress",
                            "processed": idx,
                            "total": total_typed,
                            "phase": "restamping",
                        }));
                    }
                }
                // Collect non-Entity labels ("specific" labels).
                let specific: Vec<&str> = entity
                    .labels
                    .iter()
                    .filter(|l| l.as_str() != "Entity")
                    .map(|l| l.as_str())
                    .collect();
                // Find the leaf type: a declared type that is not an ancestor of any other
                // specific label on this entity. Minted types (not in ancestor_map_d) are skipped.
                let leaf_types: Vec<&str> = specific
                    .iter()
                    .copied()
                    .filter(|&t| {
                        ancestor_map_d.contains_key(t)
                            && !specific.iter().any(|&other| {
                                t != other
                                    && ancestor_map_d
                                        .get(other)
                                        .is_some_and(|anc| anc.iter().any(|a| a.as_str() == t))
                            })
                    })
                    .collect();
                if leaf_types.len() != 1 {
                    continue;
                }
                let entity_type = leaf_types[0];
                let mut expected = vec!["Entity".to_string()];
                if let Some(ancestors) = ancestor_map_d.get(entity_type) {
                    expected.extend(ancestors.iter().cloned());
                }
                expected.push(entity_type.to_string());
                let mut current = entity.labels.clone();
                current.sort_unstable();
                let mut exp_sorted = expected.clone();
                exp_sorted.sort_unstable();
                if current != exp_sorted {
                    conn.update_entity_labels(&entity.uuid, &expected)?;
                    restamped_count += 1;
                }
            }
            wal_exec::wal_flush_ungrouped(&state_d, &group_id_d, conn.drain_mutations());
            Ok(restamped_count)
        })
        .await??
    } else {
        0
    };

    Ok(json!({
        "success": true,
        "reclassified_count": reclassified,
        "unchanged_count": unchanged_count,
        "restamped_count": restamped,
        "group_id": group_id,
    }))
}

// ── Relation canonicalization handler ────────────────────────────────────────

/// Runs the relation canonicalization pass over all edges in the workspace graph.
///
/// Parameters (from req.params):
/// - `dry_run: bool` (default false) — report coverage without mutating the graph or WAL.
/// - `embedding_threshold: f32` (default 0.7) — cosine similarity threshold for embedding fallback.
///
/// Callers must add `knowledge_canonicalize_relations` to `service_protocol.py` in the
/// liminis-app repo (spec #163).
async fn handle_canonicalize_relations(
    req: &IpcRequest,
    state: Arc<AppState>,
    progress_tx: Option<UnboundedSender<Value>>,
) -> Result<Value, Error> {
    // FR-013: fail fast if no ontology with relation_types is loaded
    let ontology = state
        .ontology
        .as_ref()
        .ok_or_else(|| {
            Error::Ipc(
                "knowledge_canonicalize_relations requires a workspace ontology with relation_types \
                 defined in .lcg/ontology.yaml"
                    .to_string(),
            )
        })?
        .clone();
    if !ontology.has_relation_types() {
        return Err(Error::Ipc(
            "knowledge_canonicalize_relations requires at least one relation_type in the ontology"
                .to_string(),
        ));
    }

    let dry_run = req.params["dry_run"].as_bool().unwrap_or(false);
    let embedding_threshold = req.params["embedding_threshold"].as_f64().map(|v| v as f32);

    let params = canonicalize::CanonicalizeParams {
        dry_run,
        embedding_threshold,
    };

    canonicalize::canonicalize_relations(state, params, progress_tx, ontology).await
}

async fn handle_backfill_relation_types(
    req: &IpcRequest,
    state: Arc<AppState>,
    progress_tx: Option<UnboundedSender<Value>>,
) -> Result<Value, Error> {
    let dry_run = req.params["dry_run"].as_bool().unwrap_or(false);
    let params = backfill::BackfillParams { dry_run };
    backfill::backfill_relation_types(state, params, progress_tx).await
}

// ── Relation reprocessing handler (issue #210) ───────────────────────────────

/// Fact-based LLM relation classification: the relation-side twin of
/// `handle_reprocess_entity_types`. Unlike that handler, every scope value requires a declared
/// ontology relation-type menu (FR-002/A1) — there is no open-ended fallback.
async fn handle_reprocess_relation_types(
    req: &IpcRequest,
    state: Arc<AppState>,
    progress_tx: Option<UnboundedSender<Value>>,
) -> Result<Value, Error> {
    let group_id = req.params["group_id"]
        .as_str()
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string();

    let scope = match req.params["scope"].as_str().unwrap_or("untyped") {
        "untyped" => reprocess_relations::RelationReprocessScope::Untyped,
        "off_ontology" => reprocess_relations::RelationReprocessScope::OffOntology,
        "all" => reprocess_relations::RelationReprocessScope::All,
        other => {
            return Ok(json!({
                "success": false,
                "error": format!(
                    "unknown scope '{other}'; valid values: untyped, off_ontology, all"
                ),
            }));
        }
    };
    let dry_run = req.params["dry_run"].as_bool().unwrap_or(false);

    // FR-002: every scope requires a declared ontology relation-type menu — relation
    // classification has no open-ended/unconstrained mode (A1).
    let ontology = match state.ontology.as_ref() {
        Some(o) if o.has_relation_types() => Arc::clone(o),
        Some(_) => {
            return Ok(json!({
                "success": false,
                "error": "the configured ontology declares no relation types; \
                          knowledge_reprocess_relation_types requires at least one",
            }));
        }
        None => {
            return Ok(json!({
                "success": false,
                "error": "knowledge_reprocess_relation_types requires a workspace ontology with \
                          relation_types defined in .lcg/ontology.yaml",
            }));
        }
    };

    let params = reprocess_relations::ReprocessRelationsParams {
        group_id,
        scope,
        dry_run,
    };

    reprocess_relations::reprocess_relation_types(state, params, progress_tx, ontology).await
}

// ── Recovery handler ─────────────────────────────────────────────────────────

struct RecoverOutcome {
    db: Option<Db>,
    restart_required: bool,
}

async fn handle_knowledge_recover(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let strategy = req.params["strategy"].as_str().unwrap_or("").to_string();
    if strategy.is_empty() {
        return Err(Error::Ipc("strategy is required".to_string()));
    }

    // Serialize recovery via write_lock (try_write for concurrent callers)
    let _write_guard = state
        .write_lock
        .try_write()
        .map_err(|_| Error::Ipc("Recovery already in progress".to_string()))?;

    let db_path = state.db_path.clone();
    // "rebuild_from_workspace_wal" wipes and replays the *entire* embedded DB (every group's
    // data), so it needs the WAL root, not one group's own subdirectory — every group found
    // under it is replayed back in (issue #378: FR-009's single-group-parity guarantee must not
    // become "every non-default group's data silently vanishes" the moment a second group
    // exists).
    let wal_root = state.wal_root.clone();
    let embedding_dim = state.embedder.dim();

    let result = match strategy.as_str() {
        "drop_lbug_wal" => recover_drop_lbug_wal(&db_path, embedding_dim).await,
        "rebuild_from_workspace_wal" => {
            let wal_root =
                wal_root.ok_or_else(|| Error::Ipc("No WAL dir configured".to_string()))?;
            // Preset to false before attempting the rebuild: this strategy actively drops and
            // rebuilds the indices, and build_indices_and_constraints() below is fatal via `?`,
            // so there is no later success-path hook to store `false` from on failure. Scoped to
            // this arm only — drop_lbug_wal/restore_from_backup never touch indices, so a failed
            // call there must leave indices_built at its prior value, not force it to false.
            state.indices_built.store(false, Ordering::Release);
            recover_rebuild_from_workspace_wal(
                &db_path,
                &wal_root,
                embedding_dim,
                Arc::clone(&state.sink),
            )
            .await
        }
        "restore_from_backup" => recover_restore_from_backup(&db_path, embedding_dim).await,
        other => return Err(Error::Ipc(format!("Unknown strategy: {other}"))),
    };

    // Hold write guard through the db.store() call — see ADR-0002.
    // _write_guard drops at end of match arm (end of function scope).
    match result {
        Ok(RecoverOutcome {
            db: Some(new_db),
            restart_required: false,
        }) => {
            state.db.store(Some(Arc::new(new_db)));
            // Indices are known-good here for all three strategies: drop_lbug_wal/
            // restore_from_backup reopen an existing, already-indexed checkpoint or backup
            // (trust assumption, issue #297); rebuild_from_workspace_wal only reaches this arm
            // because its internal build_indices_and_constraints()? already succeeded.
            state.indices_built.store(true, Ordering::Release);
            // Clear degraded reason
            if let Ok(mut g) = state.degraded_reason.lock() {
                *g = None;
            }
            // Emit healthy telemetry
            state.sink.emit(TelemetryEvent::ServiceState {
                ts_ms: now_ms(),
                state: "healthy".to_string(),
                reason: None,
                detail: None,
            });
            Ok(json!({"strategy": strategy, "success": true, "restart_required": false}))
        }
        Ok(RecoverOutcome {
            restart_required: true,
            ..
        }) => {
            let resp = json!({"strategy": strategy, "success": true, "restart_required": true});
            // Exit after brief delay so the response can be sent first
            tokio::spawn(async {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                std::process::exit(0);
            });
            Ok(resp)
        }
        Ok(RecoverOutcome {
            db: None,
            restart_required: false,
        }) => Ok(json!({
            "strategy": strategy,
            "success": false,
            "error": "Recovery operation succeeded but DB did not reopen",
            "restart_required": false,
        })),
        Err(e) => Ok(json!({
            "strategy": strategy,
            "success": false,
            "error": e.to_string(),
            "restart_required": false,
        })),
    }
}

/// Single-call full recovery: checkpoint-drop → episode-cursor → resume-replay → reindex.
/// Idempotent: if the engine is already healthy, returns a no-op response (FR-007).
/// Exempt from the degraded-mode guard so it can be called when DB is None.
async fn handle_knowledge_recover_full(
    _req: &IpcRequest,
    state: Arc<AppState>,
) -> Result<Value, Error> {
    // Idempotency gate: if DB is healthy, return no-op.
    if let Some(arc_db) = state.db.load_full() {
        let conn = arc_db.connect()?;
        let episodes = conn.count_nodes("Episodic").unwrap_or(0);
        return Ok(json!({
            "success": true,
            "recovery_needed": false,
            "episodes_before": episodes,
            "mutations_replayed": 0,
            "episodes_after": episodes,
            "indexes_rebuilt": false,
        }));
    }

    // Serialize recovery via write_lock (try_write for concurrent callers).
    let _write_guard = state
        .write_lock
        .try_write()
        .map_err(|_| Error::Ipc("Recovery already in progress".to_string()))?;

    let db_path = state.db_path.clone();
    // The default group's own WAL directory is run_full_recovery_sequence's target for its
    // non-destructive checkpoint-drop path (issue #378, matching the startup backfill's scope,
    // FR-009 single-group parity) — but its fallback (used when checkpoint-drop itself fails)
    // wipes the *entire* embedded DB, every group's data, not just the default group's. Pass the
    // WAL root, not just the default group's subdirectory, so that fallback can replay every
    // group back in rather than silently losing every non-default group's data.
    let wal_root = state
        .wal_root
        .clone()
        .ok_or_else(|| Error::Ipc("No WAL dir configured".to_string()))?;
    let embedding_dim = state.embedder.dim();
    let sink = Arc::clone(&state.sink);

    // Preset to false before attempting the sequence: run_full_recovery_sequence rebuilds
    // indices via a fatal `build_indices_and_constraints()?`, so there is no later success-path
    // hook to store `false` from if it fails partway through.
    state.indices_built.store(false, Ordering::Release);

    let result = tokio::task::spawn_blocking(move || {
        crate::recovery::run_full_recovery_sequence(
            &db_path,
            DEFAULT_GROUP_ID,
            &wal_root,
            embedding_dim,
            sink,
        )
    })
    .await?;

    // Hold write guard through db.store() — see ADR-0002.
    match result {
        Ok((new_db, report)) => {
            state.db.store(Some(Arc::new(new_db)));
            // report.indexes_rebuilt is always true when Ok is reached (the function only
            // constructs RecoveryReport post-success), but storing the field rather than a bare
            // `true` keeps this coupled to that invariant if the build ever becomes non-fatal.
            state
                .indices_built
                .store(report.indexes_rebuilt, Ordering::Release);
            if let Ok(mut g) = state.degraded_reason.lock() {
                *g = None;
            }
            state.sink.emit(TelemetryEvent::ServiceState {
                ts_ms: now_ms(),
                state: "healthy".to_string(),
                reason: None,
                detail: None,
            });
            Ok(json!({
                "success": true,
                "recovery_needed": true,
                "episodes_before": report.episodes_before,
                "mutations_replayed": report.mutations_replayed,
                "episodes_after": report.episodes_after,
                "indexes_rebuilt": report.indexes_rebuilt,
            }))
        }
        Err(e) => Ok(json!({
            "success": false,
            "error": e.to_string(),
        })),
    }
}

async fn recover_drop_lbug_wal(
    db_path: &str,
    embedding_dim: usize,
) -> Result<RecoverOutcome, Error> {
    let db_path = db_path.to_string();
    tokio::task::spawn_blocking(move || -> Result<RecoverOutcome, Error> {
        let wal_path = format!("{}.wal", db_path);
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let corrupt_path = format!("{}.wal.corrupt-{}", db_path, ts);

        if std::path::Path::new(&wal_path).exists() {
            match std::fs::rename(&wal_path, &corrupt_path) {
                Ok(_) => {}
                Err(_) => {
                    // Can't rename (file locked?); must restart
                    return Ok(RecoverOutcome {
                        db: None,
                        restart_required: true,
                    });
                }
            }
        }

        // drop_lbug_wal's entire premise is reopening an existing, already-indexed checkpoint
        // (issue #297's trust assumption) — but Db::open has open-or-create semantics (see
        // Db::open_or_rebuild's doc comment), so if the checkpoint file itself is missing, it
        // would silently create a fresh, unindexed DB rather than fail, and init_schema() below
        // never calls build_indices_and_constraints(). handle_knowledge_recover's shared success
        // arm would then report indices_built: true for a DB with no indices at all. Fail fast
        // instead: no checkpoint to reopen means this strategy doesn't apply.
        if !std::path::Path::new(&db_path).is_file() {
            return Err(Error::Ipc(format!(
                "drop_lbug_wal requires an existing checkpoint file; none found at {db_path}"
            )));
        }

        let db = Db::open(&db_path)?;
        {
            let conn = db.connect()?;
            conn.init_schema(embedding_dim)?;
            // The reopened DB (checkpoint-only, WAL dropped) may already contain entities —
            // rebuild the name index from it (issue #219).
            conn.rebuild_name_index()?;
        }
        Ok(RecoverOutcome {
            db: Some(db),
            restart_required: false,
        })
    })
    .await?
}

/// Wipes and rebuilds the entire embedded DB from `wal_root`'s WAL content. `wal_root` is the
/// WAL **root**, not one group's own subdirectory (issue #378): the DB this function deletes and
/// reopens holds every group's data, so it must replay **every** group directory found under
/// `wal_root`, not only the default group's — otherwise every non-default group's data would
/// vanish from the live graph the moment this strategy runs on a multi-group instance, even
/// though its own WAL files were never touched (silently unavailable, not deleted, but a
/// correctness bug either way; FR-009's "additive, not a replacement" guarantee extends here).
async fn recover_rebuild_from_workspace_wal(
    db_path: &str,
    wal_root: &std::path::Path,
    embedding_dim: usize,
    sink: std::sync::Arc<dyn crate::telemetry::TelemetrySink>,
) -> Result<RecoverOutcome, Error> {
    let db_path = db_path.to_string();
    let wal_root = wal_root.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<RecoverOutcome, Error> {
        // Remove existing DB and siblings
        let path = std::path::Path::new(&db_path);
        if path.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else if path.exists() {
            std::fs::remove_file(path)?;
        }
        for ext in &[".wal", ".lock"] {
            let _ = std::fs::remove_file(format!("{}{}", db_path, ext));
        }
        // Ensure parent dir exists
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let db = Db::open(&db_path)?;
        {
            let conn = db.connect()?;
            conn.init_schema(embedding_dim)?;
            let replay_started_at = std::time::Instant::now();
            // TODO(follow-up): recover_rebuild_from_workspace_wal does not yet surface
            // fidelity_warning in RecoverOutcome — a high failure rate here is still silent.
            // See #128 for context; address in a follow-up issue.
            let (mut lines_replayed, mut unrecognised_lines) = (0u64, 0u64);
            let (mut failed_lines, mut unparseable_lines, mut legacy_skipped_lines) =
                (0u64, 0u64, 0u64);
            for (gid, dir) in wal_group::list_group_wal_dirs(&wal_root)? {
                let stats = crate::replay::WalReplayer::new(&dir).replay(&conn)?;
                if let Some(seq) = stats.last_committed_seq {
                    if let Err(e) = conn.set_applied_seq(&gid, seq) {
                        eprintln!(
                            "liminis-context-graph: rebuild_from_workspace_wal: failed to persist applied_seq={seq} for group {gid:?} (non-fatal): {e}"
                        );
                    }
                }
                lines_replayed += stats.lines_replayed;
                unrecognised_lines += stats.unrecognised_lines;
                failed_lines += stats.failed_lines;
                unparseable_lines += stats.unparseable_lines;
                legacy_skipped_lines += stats.legacy_skipped_lines;
            }
            sink.emit(TelemetryEvent::WalReplayComplete {
                ts_ms: now_ms(),
                mutations_replayed: lines_replayed,
                unrecognised_lines,
                failed_lines,
                unparseable_lines,
                legacy_skipped_lines,
                duration_ms: replay_started_at.elapsed().as_millis() as u64,
            });
            conn.build_indices_and_constraints()?;
            // WAL replay above bypassed insert_entity/update_entity_created_at (issue #219) —
            // rebuild the name index from the fully-loaded data.
            conn.rebuild_name_index()?;
        }
        Ok(RecoverOutcome {
            db: Some(db),
            restart_required: false,
        })
    })
    .await?
}

async fn recover_restore_from_backup(
    db_path: &str,
    embedding_dim: usize,
) -> Result<RecoverOutcome, Error> {
    let db_path = db_path.to_string();
    tokio::task::spawn_blocking(move || -> Result<RecoverOutcome, Error> {
        // Scan for backup files
        let db_dir = std::path::Path::new(&db_path)
            .parent()
            .ok_or_else(|| Error::Ipc("Cannot determine DB directory".to_string()))?;

        let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
        if let Ok(rd) = std::fs::read_dir(db_dir) {
            for entry in rd.flatten() {
                let fname = entry.file_name();
                let fname_str = fname.to_string_lossy();
                if fname_str.contains(".pre-") && fname_str.ends_with("-backup") {
                    if let Ok(meta) = entry.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            let is_better = best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true);
                            if is_better {
                                best = Some((mtime, entry.path()));
                            }
                        }
                    }
                }
            }
        }

        let backup_path = best
            .ok_or_else(|| Error::Ipc("No backup file found".to_string()))?
            .1;
        // Remove stale WAL/lock files before overwriting the DB file.
        // If a corrupt WAL is left in place lbug will attempt to replay it against
        // the restored checkpoint, causing immediate re-corruption on open.
        for ext in &[".wal", ".lock"] {
            let _ = std::fs::remove_file(format!("{}{}", db_path, ext));
        }
        std::fs::copy(&backup_path, &db_path)
            .map_err(|e| Error::Ipc(format!("Failed to restore backup: {e}")))?;

        let db = Db::open(&db_path)?;
        {
            let conn = db.connect()?;
            conn.init_schema(embedding_dim)?;
            // The restored backup file may already contain entities — rebuild the name
            // index from it (issue #219).
            conn.rebuild_name_index()?;
        }
        Ok(RecoverOutcome {
            db: Some(db),
            restart_required: false,
        })
    })
    .await?
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn validate_from_seq(v: &Value) -> Result<u64, Error> {
    match v {
        Value::Null => Ok(0),
        Value::Bool(_) => Err(Error::Ipc(
            "from_seq must be a non-negative integer, not a boolean".to_string(),
        )),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(u)
            } else if let Some(i) = n.as_i64() {
                Err(Error::Ipc(format!(
                    "from_seq must be non-negative, got {i}"
                )))
            } else {
                Err(Error::Ipc(
                    "from_seq must be a non-negative integer".to_string(),
                ))
            }
        }
        _ => Err(Error::Ipc(
            "from_seq must be a non-negative integer".to_string(),
        )),
    }
}

/// Unlike `validate_from_seq`, absence (`Value::Null`) maps to `None` (unbounded), not `Some(0)`
/// — `to_seq`'s absence is semantically distinct from a bound of `0` (FR-001).
fn validate_to_seq(v: &Value) -> Result<Option<u64>, Error> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(_) => Err(Error::Ipc(
            "to_seq must be a non-negative integer, not a boolean".to_string(),
        )),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(Some(u))
            } else if let Some(i) = n.as_i64() {
                Err(Error::Ipc(format!("to_seq must be non-negative, got {i}")))
            } else {
                Err(Error::Ipc(
                    "to_seq must be a non-negative integer".to_string(),
                ))
            }
        }
        _ => Err(Error::Ipc(
            "to_seq must be a non-negative integer".to_string(),
        )),
    }
}

fn has_jsonl_files(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        })
        .unwrap_or(false)
}

fn is_nonempty_dir(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .map(|mut rd| rd.next().is_some())
        .unwrap_or(false)
}

fn build_progress_fn(tx: Option<UnboundedSender<Value>>) -> Option<ProgressFn> {
    tx.map(|tx| {
        let f: ProgressFn = Box::new(move |p| {
            let val = json!({
                "type": "progress",
                "message": p.message,
                "mutations_replayed_so_far": p.mutations_replayed,
                "files_processed_so_far": p.files_processed,
                "files_total": p.files_total,
                "failed_lines_so_far": p.failed_lines_so_far,
                "legacy_skipped_lines_so_far": p.legacy_skipped_lines_so_far,
            });
            tx.send(val).is_ok()
        });
        f
    })
}

fn extract_group_ids(v: &Value) -> Vec<String> {
    match v {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|e| e.as_str().map(str::to_string))
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => vec![DEFAULT_GROUP_ID.to_string()],
    }
}

/// Returns None when group_ids is absent, null, or false — meaning "all groups".
/// Returns Some(vec) for an array or single string — meaning "these groups only".
/// Used by deletion methods, which differ from search handlers: absent = all groups,
/// not the default "liminis" group.
fn extract_optional_group_ids(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(arr) => {
            let gids: Vec<String> = arr
                .iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect();
            if gids.is_empty() {
                None
            } else {
                Some(gids)
            }
        }
        Value::String(s) => Some(vec![s.clone()]),
        _ => None,
    }
}

/// Returns a deduplicated list of entity UUIDs from edge source and target endpoints.
fn edge_endpoint_uuids(edges: &[crate::types::RelatesToEdge]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut uuids = Vec::new();
    for edge in edges {
        for uuid in [&edge.source_node_uuid, &edge.target_node_uuid] {
            if seen.insert(uuid.clone()) {
                uuids.push(uuid.clone());
            }
        }
    }
    uuids
}

/// Enriches an edge with episode info derived from its source and target entity UUIDs.
///
/// Uses either-endpoint semantics: any episode that mentions the source OR target entity
/// is attributed to the edge. Episodes appearing via both endpoints are deduplicated.
fn enrich_edge_from_entity_ep_info(
    edge: &mut crate::types::RelatesToEdge,
    ep_info: &HashMap<String, (Vec<String>, Vec<String>)>,
) {
    let mut seen_ep_uuids = std::collections::HashSet::new();
    let mut ep_uuids = Vec::new();
    let mut src_descs = Vec::new();
    for endpoint_uuid in [&edge.source_node_uuid, &edge.target_node_uuid] {
        if let Some((uuids, descs)) = ep_info.get(endpoint_uuid) {
            for (ep_uuid, src_desc) in uuids.iter().zip(descs.iter()) {
                if seen_ep_uuids.insert(ep_uuid.clone()) {
                    ep_uuids.push(ep_uuid.clone());
                    src_descs.push(src_desc.clone());
                }
            }
        }
    }
    edge.episode_uuids = ep_uuids;
    edge.source_descriptions = src_descs;
}
