use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    corrections,
    db::Db,
    episode,
    error::Error,
    ipc::{IpcRequest, IpcResponse},
    rebuild_job::{JobStatus, RebuildJob},
    replay::{ProgressFn, ReplayOptions, ReplayProgress, WalReplayer},
    search,
    telemetry::{now_ms, TelemetryEvent},
};

const DEFAULT_GROUP_ID: &str = "liminis";

const MISSING_INDEX_USER_MSG: &str =
    "Knowledge graph indices not yet built. Call knowledge_build_indices to resolve.";

fn is_missing_index_error(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("Binder exception:") && s.contains("doesn't have an index with name")
}

/// Acquires the write lock and calls `build_indices_and_constraints`, then sets the
/// `indices_built` flag so subsequent searches skip the auto-heal path (FR-003).
/// Called at most once per session per DB lifecycle event.
async fn build_indices_once(state: &Arc<AppState>) -> Result<(), Error> {
    let _guard = state.write_lock.write().await;
    // Double-check inside the lock: another task may have completed the build while we waited.
    if state.indices_built.load(Ordering::Acquire) {
        return Ok(());
    }
    // Load DB after acquiring the lock so we build on the current instance, not a stale
    // snapshot that predates a concurrent clear_all swap.
    let db = load_db(state)?;
    let result = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.build_indices_and_constraints()
    })
    .await;
    match result {
        Ok(Ok(())) => {
            // Set flag while still holding the write lock to eliminate the window between
            // guard release and flag update that would allow redundant builds.
            state.indices_built.store(true, Ordering::Release);
            Ok(())
        }
        Ok(Err(e)) => Err(Error::Ipc(format!(
            "Auto-build of knowledge graph indices failed: {e}"
        ))),
        Err(e) => Err(Error::Ipc(format!(
            "Auto-build of knowledge graph indices failed: {e}"
        ))),
    }
}

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
    // when the DB is unavailable. See ADR-0046.
    let exempt_in_degraded = matches!(
        req.method.as_str(),
        "health_check" | "knowledge_status" | "knowledge_recover" | "knowledge_close"
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
        "knowledge_clear_all" => handle_clear_all(req, state).await,
        "knowledge_prepare_checkpoint" => handle_prepare_checkpoint(state).await,
        "knowledge_rebuild_from_wal" => handle_rebuild_from_wal(req, state, progress_tx).await,
        "knowledge_rebuild_status" => handle_rebuild_status(req, state).await,
        "knowledge_recover" => handle_knowledge_recover(req, state).await,
        "knowledge_close" => Ok(json!({"status": "closed"})),
        "knowledge_search_passages" => handle_search_passages(req, state).await,
        "knowledge_list_entities" => handle_list_entities(req, state).await,
        "knowledge_list_relationships" => handle_list_relationships(req, state).await,
        "knowledge_get_entity_neighbors" => handle_get_entity_neighbors(req, state).await,
        "knowledge_get_entities_by_source" => handle_get_entities_by_source(req, state).await,
        "knowledge_validate_corrections" => handle_validate_corrections(state).await,
        "knowledge_apply_corrections" => handle_apply_corrections(req, state).await,
        "knowledge_reprocess_entity_types" => handle_reprocess_entity_types(req, state).await,
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

    let result = episode::add_episode(
        state,
        &name,
        &body,
        &source,
        &source_desc,
        &ref_time,
        &group_id,
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

/// Aggregated counts + WAL metadata gathered inside one blocking task.
type StatusFields = (
    u64,
    u64,
    u64,
    bool,
    u64,
    u64,
    Option<String>,
    Option<String>,
);

async fn handle_knowledge_status(state: Arc<AppState>) -> Result<Value, Error> {
    let db_opt = state.db.load_full();
    if db_opt.is_none() {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        // Only advertise rebuild_from_workspace_wal when a WAL dir is actually configured;
        // otherwise clients would offer an option that always fails immediately.
        let mut recovery_available = vec!["drop_lbug_wal"];
        if state.wal_dir.is_some() {
            recovery_available.push("rebuild_from_workspace_wal");
        }
        return Ok(json!({
            "running": true,
            "degraded": true,
            "reason": reason,
            "context_graph_initialized": false,
            "connected": false,
            "initializing": false,
            "recovery_available": recovery_available,
        }));
    }
    let db = db_opt.unwrap();
    let db_path = state.db_path.clone();
    let embedding_model = state.embedding_model.clone();
    let embedding_dim = state.embedder.dim();
    let wal_dir = state.wal_dir.clone();

    let _guard = state.write_lock.read().await;
    let (
        entity_count,
        episode_count,
        edge_count,
        wal_exists,
        wal_file_count,
        wal_byte_size,
        last_index_time,
        index_created_at,
    ) = tokio::task::spawn_blocking(move || -> Result<StatusFields, crate::error::Error> {
        let conn = db.connect()?;
        let entity_count = conn.count_nodes("Entity")?;
        let episode_count = conn.count_nodes("Episodic")?;
        let edge_count = conn.count_relates_to_edges()?;
        let last_index_time = conn.get_latest_episode_time()?;
        let index_created_at = conn.get_earliest_episode_time()?;
        let (wal_exists, wal_file_count, wal_byte_size) = scan_wal_dir(wal_dir.as_deref());
        Ok((
            entity_count,
            episode_count,
            edge_count,
            wal_exists,
            wal_file_count,
            wal_byte_size,
            last_index_time,
            index_created_at,
        ))
    })
    .await??;
    drop(_guard);

    // startup sequence (Db::open → init_schema → bind socket) completes before any request
    // can arrive, so these lifecycle values are always true/true/false at handler time
    let mut result = json!({
        "database_path": db_path,
        "embedding_model": embedding_model,
        "embedding_dim": embedding_dim,
        "entity_count": entity_count,
        "relationship_count": edge_count,
        "episode_count": episode_count,
        "last_index_time": last_index_time,
        "context_graph_initialized": true,
        "connected": true,
        "initializing": false,
        "wal": {
            "exists": wal_exists,
            "file_count": wal_file_count,
            "byte_size": wal_byte_size,
        },
    });
    if let Some(t) = index_created_at {
        result["index_created_at"] = json!(t);
    }
    Ok(result)
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
    let result = episode::add_episode(
        state,
        &chunk_id,
        &chunk_text,
        "text",
        &source_desc,
        &ref_time,
        &group_id,
    )
    .await?;

    Ok(json!({
        "success": true,
        "chunk_id": chunk_id,
        "source_file": source_file,
        "episode_uuid": result.episode_uuid,
        "nodes_extracted": result.nodes_extracted,
        "edges_extracted": result.edges_extracted,
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

    Ok(serde_json::to_value(entities)?)
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

    Ok(serde_json::to_value(edges)?)
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

    Ok(serde_json::to_value(episodes)?)
}

async fn handle_delete_episode(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let episode_uuid = req.params["episode_uuid"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let db = load_db(&state)?;
    let _guard = state.write_lock.write().await;
    tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.remove_episode(&episode_uuid)
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

    Ok(serde_json::to_value(nodes)?)
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

    Ok(serde_json::to_value(edges)?)
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

    Ok(serde_json::to_value(edges)?)
}

async fn handle_query_cypher(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let query = req.params["query"].as_str().unwrap_or("").to_string();

    let db = load_db(&state)?;
    let _guard = state.write_lock.read().await;
    let rows = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.cypher_query(&query)
    })
    .await??;
    drop(_guard);

    Ok(json!({"rows": rows}))
}

async fn handle_build_indices(state: Arc<AppState>) -> Result<Value, Error> {
    let db = load_db(&state)?;
    let _guard = state.write_lock.write().await;
    tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.build_indices_and_constraints()
    })
    .await??;
    drop(_guard);

    state
        .indices_built
        .store(true, std::sync::atomic::Ordering::Release);
    Ok(json!({"status": "ok"}))
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
        for n in &nodes {
            if !all_entity_uuids_owned.contains(&n.uuid) {
                all_entity_uuids_owned.push(n.uuid.clone());
            }
        }
        let all_entity_uuid_refs: Vec<&str> = all_entity_uuids_owned
            .iter()
            .map(String::as_str)
            .collect();
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
    let node_count = nodes.len();
    Ok(json!({ "source": source_val, "nodes": nodes, "node_count": node_count }))
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
    let _guard = state.write_lock.write().await;
    let deleted_uuids = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Option<Vec<&str>> = group_ids_owned
            .as_ref()
            .map(|v| v.iter().map(String::as_str).collect());
        conn.remove_episodes_by_source(&source_file, gid_refs.as_deref())
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
    let _guard = state.write_lock.write().await;
    let deleted_uuids = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Option<Vec<&str>> = group_ids_owned
            .as_ref()
            .map(|v| v.iter().map(String::as_str).collect());
        conn.remove_episodes_by_chunk_id(&chunk_id, gid_refs.as_deref())
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
    let wal_dir = state.wal_dir.clone();
    let embedding_dim = state.embedder.dim();

    let _guard = state.write_lock.write().await;

    if !preserve_wal {
        // Reset wal_writer to None before deleting the WAL directory so that subsequent writes
        // lazily re-initialize the writer and don't target a deleted path.
        drop(
            state
                .wal_writer
                .lock()
                .map_err(|_| Error::Ipc("wal_writer lock poisoned".to_string()))?
                .take(),
        );
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
            if let Some(wal) = wal_dir {
                if wal.exists() {
                    std::fs::remove_dir_all(&wal).map_err(|e| {
                        Error::Ipc(format!("failed to delete WAL dir '{}': {e}", wal.display()))
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
    drop(_guard);

    Ok(json!({"success": true, "message": "Graph cleared and reinitialized successfully"}))
}

// ── WAL admin handlers ────────────────────────────────────────────────────────

async fn handle_prepare_checkpoint(state: Arc<AppState>) -> Result<Value, Error> {
    let wal_dir = state.wal_dir.clone();
    let wal_writer = Arc::clone(&state.wal_writer);

    // Serialize with in-flight writes (FR-006)
    let _write_guard = state.write_lock.write().await;

    let (files_flushed, files_total) =
        tokio::task::spawn_blocking(move || -> Result<(u32, u32), Error> {
            let mut w = wal_writer
                .lock()
                .map_err(|_| Error::Ipc("wal_writer lock poisoned".to_string()))?;
            if let Some(ref mut writer) = *w {
                let (r, t) = writer.rotate();
                Ok((r, t))
            } else {
                // No writer; count JSONL files in wal_dir if configured and present
                let total = wal_dir
                    .as_deref()
                    .map(|d| {
                        if d.exists() {
                            std::fs::read_dir(d)
                                .ok()
                                .map(|rd| {
                                    rd.filter_map(|e| e.ok())
                                        .filter(|e| {
                                            e.path().extension().and_then(|x| x.to_str())
                                                == Some("jsonl")
                                        })
                                        .count() as u32
                                })
                                .unwrap_or(0)
                        } else {
                            0
                        }
                    })
                    .unwrap_or(0);
                Ok((0, total))
            }
        })
        .await??;
    drop(_write_guard);

    Ok(json!({
        "success": true,
        "files_flushed": files_flushed,
        "files_total": files_total,
    }))
}

async fn handle_rebuild_from_wal(
    req: &IpcRequest,
    state: Arc<AppState>,
    progress_tx: Option<UnboundedSender<Value>>,
) -> Result<Value, Error> {
    let p = &req.params;

    let from_seq = validate_from_seq(&p["from_seq"])?;
    let dry_run = p["dry_run"].as_bool().unwrap_or(false);

    let wal_dir = state
        .wal_dir
        .clone()
        .ok_or_else(|| Error::Ipc("No WAL directory configured (set LCG_WAL_DIR)".to_string()))?;

    if !wal_dir.exists() || !has_jsonl_files(&wal_dir) {
        return Err(Error::Ipc(format!(
            "No WAL files found at {}",
            wal_dir.display()
        )));
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
        let shutdown_flag_cancel = Arc::clone(&state.shutdown);
        let shutdown_cancelled_inner = Arc::clone(&shutdown_cancelled);

        // Write lock held in async scope; guard released after spawn_blocking completes.
        let _write_guard = if !dry_run {
            Some(state.write_lock.write().await)
        } else {
            None
        };

        let stats =
            tokio::task::spawn_blocking(move || -> Result<crate::replay::ReplayStats, Error> {
                let conn = db.connect()?;
                // Composite cancel: fire on client disconnect OR service shutdown (R9).
                let cancel_fn: Option<crate::replay::CancelFn> = tx.as_ref().map(|t| {
                    let t = t.clone();
                    let flag = Arc::clone(&shutdown_flag_cancel);
                    let cancelled = shutdown_cancelled_inner;
                    let f: crate::replay::CancelFn = Box::new(move || {
                        let client_gone = t.is_closed();
                        let shutting_down = flag.load(Ordering::Relaxed);
                        if shutting_down {
                            cancelled.store(true, Ordering::Relaxed);
                        }
                        client_gone || shutting_down
                    });
                    f
                });
                WalReplayer::new(&wal_dir_c).replay_opts(
                    &conn,
                    ReplayOptions {
                        from_seq,
                        dry_run,
                        cancel_fn,
                        progress_fn: build_progress_fn(tx),
                    },
                )
            })
            .await??;
        drop(_write_guard);

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
                    "files_processed_so_far": stats.files_read,
                }));
            }
        }

        let mut result = json!({
            "success": true,
            "mutations_replayed": stats.lines_replayed,
            "wal_files_processed": stats.files_read,
            "indexes_created": stats.indexes_created,
        });
        if dry_run {
            result["dry_run"] = json!(true);
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
        let db = load_db(&state)?;
        let wal_dir_c = wal_dir.clone();
        let stats =
            tokio::task::spawn_blocking(move || -> Result<crate::replay::ReplayStats, Error> {
                let conn = db.connect()?;
                WalReplayer::new(&wal_dir_c).replay_opts(
                    &conn,
                    ReplayOptions {
                        from_seq,
                        dry_run: true,
                        progress_fn: None,
                        cancel_fn: None,
                    },
                )
            })
            .await??;
        return Ok(json!({
            "success": true,
            "mutations_replayed": stats.lines_replayed,
            "wal_files_processed": stats.files_read,
            "indexes_created": stats.indexes_created,
            "dry_run": true,
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

    let spawn_handle = tokio::spawn(async move {
        // OwnedRwLockWriteGuard is 'static + Send — safe to hold in a spawned task
        let _write_guard = if !dry_run {
            Some(write_lock.write_owned().await)
        } else {
            None
        };

        let jobs_ref = Arc::clone(&rebuild_jobs);
        let jid = job_id_task.clone();

        let result =
            tokio::task::spawn_blocking(move || -> Result<crate::replay::ReplayStats, Error> {
                let conn = db.connect()?;
                let progress_fn: Box<dyn Fn(&ReplayProgress) -> bool + Send> = Box::new(move |p| {
                    if let Ok(mut guard) = jobs_ref.lock() {
                        if let Some(job) = guard.get_mut(&jid) {
                            job.mutations_replayed = p.mutations_replayed;
                            job.wal_files_processed = p.files_processed;
                        }
                    }
                    true
                });
                WalReplayer::new(&wal_dir_c).replay_opts(
                    &conn,
                    ReplayOptions {
                        from_seq,
                        dry_run,
                        progress_fn: Some(progress_fn),
                        cancel_fn: None,
                    },
                )
            })
            .await;

        drop(_write_guard);

        if let Ok(mut jobs) = rebuild_jobs.lock() {
            if let Some(job) = jobs.get_mut(&job_id_task) {
                match result {
                    Ok(Ok(stats)) => {
                        job.status = JobStatus::Completed;
                        job.mutations_replayed = stats.lines_replayed;
                        job.wal_files_processed = stats.files_read;
                        job.result = Some(json!({
                            "mutations_replayed": stats.lines_replayed,
                            "wal_files_processed": stats.files_read,
                            "indexes_created": stats.indexes_created,
                            "dry_run": dry_run,
                        }));
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
    let _guard = state.write_lock.write().await;
    let result = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        Ok::<_, Error>(corrections::apply_corrections_file(
            &conn,
            &workspace_root,
            dry_run,
        ))
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

async fn handle_reprocess_entity_types(
    req: &IpcRequest,
    state: Arc<AppState>,
) -> Result<Value, Error> {
    state.workspace_root.as_ref().ok_or_else(|| {
        Error::Ipc("LIMINIS_WORKSPACE_ROOT not set; corrections unavailable".to_string())
    })?;

    let group_id = req.params["group_id"]
        .as_str()
        .unwrap_or(DEFAULT_GROUP_ID)
        .to_string();

    // Phase A (read lock): list all generic-only entities for the group
    let db = load_db(&state)?;
    let group_id_a = group_id.clone();
    let _read_guard = state.write_lock.read().await;
    let entities = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        corrections::list_all_generic_entities(&conn, &group_id_a)
    })
    .await??;
    drop(_read_guard);

    if entities.is_empty() {
        return Ok(json!({ "success": true, "reclassified_count": 0, "group_id": group_id }));
    }

    // Phase B (no lock): classify entities via LLM in batches to avoid token limit violations.
    let pairs: Vec<(String, String)> = entities
        .iter()
        .map(|e| (e.name.clone(), e.summary.clone()))
        .collect();

    let mut types: Vec<String> = Vec::with_capacity(entities.len());
    for chunk in pairs.chunks(corrections::REPROCESS_BATCH_SIZE) {
        let refs: Vec<(&str, &str)> = chunk
            .iter()
            .map(|(n, s)| (n.as_str(), s.as_str()))
            .collect();
        match state.extractor.classify_entities(&refs).await {
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

    // Phase C (write lock): apply label mutations
    let updates: Vec<(String, String)> = entities
        .iter()
        .zip(types.iter())
        .filter(|(_, t)| !t.is_empty())
        .map(|(e, t)| (e.uuid.clone(), t.clone()))
        .collect();

    let db = load_db(&state)?;
    let _write_guard = state.write_lock.write().await;
    let reclassified = tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        corrections::apply_entity_type_labels(&conn, &updates)
    })
    .await??;
    drop(_write_guard);

    Ok(json!({
        "success": true,
        "reclassified_count": reclassified,
        "group_id": group_id,
    }))
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
    let wal_dir = state.wal_dir.clone();
    let embedding_dim = state.embedder.dim();

    let result = match strategy.as_str() {
        "drop_lbug_wal" => recover_drop_lbug_wal(&db_path, embedding_dim).await,
        "rebuild_from_workspace_wal" => {
            let wal_dir = wal_dir.ok_or_else(|| Error::Ipc("No WAL dir configured".to_string()))?;
            recover_rebuild_from_workspace_wal(&db_path, &wal_dir, embedding_dim).await
        }
        "restore_from_backup" => recover_restore_from_backup(&db_path, embedding_dim).await,
        other => return Err(Error::Ipc(format!("Unknown strategy: {other}"))),
    };

    // Hold write guard through the db.store() call — see ADR-0042.
    // _write_guard drops at end of match arm (end of function scope).
    match result {
        Ok(RecoverOutcome {
            db: Some(new_db),
            restart_required: false,
        }) => {
            state.db.store(Some(Arc::new(new_db)));
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

        let db = Db::open(&db_path)?;
        {
            let conn = db.connect()?;
            conn.init_schema(embedding_dim)?;
        }
        Ok(RecoverOutcome {
            db: Some(db),
            restart_required: false,
        })
    })
    .await?
}

async fn recover_rebuild_from_workspace_wal(
    db_path: &str,
    wal_dir: &std::path::Path,
    embedding_dim: usize,
) -> Result<RecoverOutcome, Error> {
    let db_path = db_path.to_string();
    let wal_dir = wal_dir.to_path_buf();
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
            crate::replay::WalReplayer::new(&wal_dir).replay(&conn)?;
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
        }
        Ok(RecoverOutcome {
            db: Some(db),
            restart_required: false,
        })
    })
    .await?
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Extract `Arc<Db>` from the `ArcSwapOption`, returning `Error::DbUnavailable` if `None`.
///
/// The degraded-mode guard in `handle()` prevents most handlers from reaching this point
/// when the DB is unavailable, so this acts as a safety net for internal calls.
fn load_db(state: &AppState) -> Result<Arc<Db>, Error> {
    state.db.load_full().ok_or_else(|| {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        Error::DbUnavailable(reason)
    })
}

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

fn has_jsonl_files(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        })
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
