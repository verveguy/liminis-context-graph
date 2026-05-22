use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::{
    app_state::AppState,
    episode,
    error::Error,
    ipc::{IpcRequest, IpcResponse},
    search,
    telemetry::{now_ms, TelemetryEvent},
};

const DEFAULT_GROUP_ID: &str = "liminis";

/// Dispatches an IPC request to the appropriate library function. [IPC]
pub async fn dispatch(req: IpcRequest, state: Arc<AppState>) -> IpcResponse {
    let method = req.method.clone();
    let request_id = req.id.clone();
    let start = Instant::now();

    let (response, success) = match handle(&req, Arc::clone(&state)).await {
        Ok(result) => (IpcResponse::ok(req.id, result), true),
        Err(e) => (IpcResponse::err(req.id, -32000, e.to_string()), false),
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

async fn handle(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
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
        "knowledge_close" => Ok(json!({"status": "closed"})),
        "knowledge_search_passages" => handle_search_passages(req, state).await,
        "knowledge_list_entities" => handle_list_entities(req, state).await,
        "knowledge_list_relationships" => handle_list_relationships(req, state).await,
        "knowledge_get_entity_neighbors" => handle_get_entity_neighbors(req, state).await,
        "knowledge_get_entities_by_source" => handle_get_entities_by_source(req, state).await,
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
    let db = Arc::clone(&state.db);
    let _guard = state.write_lock.read().await;
    tokio::task::spawn_blocking(move || {
        let conn = db.connect().map_err(|e| Error::Ipc(format!("db: {e}")))?;
        conn.probe().map_err(|e| Error::Ipc(format!("db: {e}")))
    })
    .await??;
    drop(_guard);
    Ok(json!({ "ok": true, "healthy": true }))
}

async fn handle_knowledge_status(state: Arc<AppState>) -> Result<Value, Error> {
    let db = Arc::clone(&state.db);
    let db_path = state.db_path.clone();
    let embedding_model = state.embedding_model.clone();
    let embedding_dim = state.embedder.dim();
    let wal_dir = state.wal_dir.clone();

    let _guard = state.write_lock.read().await;
    let (entity_count, episode_count, edge_count, wal_exists, wal_file_count, wal_byte_size) =
        tokio::task::spawn_blocking(move || -> Result<(u64, u64, u64, bool, u64, u64), crate::error::Error> {
            let conn = db.connect()?;
            let entity_count = conn.count_nodes("Entity")?;
            let episode_count = conn.count_nodes("Episodic")?;
            let edge_count = conn.count_relates_to_edges()?;
            let (wal_exists, wal_file_count, wal_byte_size) = scan_wal_dir(wal_dir.as_deref());
            Ok((entity_count, episode_count, edge_count, wal_exists, wal_file_count, wal_byte_size))
        })
        .await??;
    drop(_guard);

    Ok(json!({
        "database_path": db_path,
        "embedding_model": embedding_model,
        "embedding_dim": embedding_dim,
        "entity_count": entity_count,
        "edge_count": edge_count,
        "episode_count": episode_count,
        "wal": {
            "exists": wal_exists,
            "file_count": wal_file_count,
            "byte_size": wal_byte_size,
        },
    }))
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
        if entry
            .path()
            .extension()
            .and_then(|x| x.to_str())
            == Some("jsonl")
        {
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
    let result = episode::add_episode(
        state,
        &chunk_id,
        &chunk_text,
        "text",
        &chunk_id,
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

    let entities = search::hybrid_entity_search(
        Arc::clone(&state.db),
        Arc::clone(&state.embedder),
        &query,
        group_ids,
        limit,
    )
    .await?;
    Ok(serde_json::to_value(entities)?)
}

async fn handle_find_relationships(req: &IpcRequest, state: Arc<AppState>) -> Result<Value, Error> {
    let p = &req.params;
    let query = p["query"].as_str().unwrap_or("").to_string();
    let group_ids = extract_group_ids(&p["group_ids"]);
    let limit = p["num_results"].as_u64().unwrap_or(10) as usize;

    let edges = search::hybrid_edge_search(
        Arc::clone(&state.db),
        Arc::clone(&state.embedder),
        &query,
        group_ids,
        limit,
    )
    .await?;
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

    let db = Arc::clone(&state.db);
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

    let db = Arc::clone(&state.db);
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

    let db = Arc::clone(&state.db);
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

    let db = Arc::clone(&state.db);
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

    let db = Arc::clone(&state.db);
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

    let db = Arc::clone(&state.db);
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
    let db = Arc::clone(&state.db);
    let _guard = state.write_lock.write().await;
    tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.build_indices_and_constraints()
    })
    .await??;
    drop(_guard);

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

    let passages = search::search_passages(
        Arc::clone(&state.db),
        Arc::clone(&state.embedder),
        &query,
        group_ids,
        num_results,
        min_score,
    )
    .await?;
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

    let db = Arc::clone(&state.db);
    let _guard = state.write_lock.read().await;
    let nodes = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids
            .as_deref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        conn.list_entities(group_ids.as_deref().map(|_| gid_refs.as_slice()), num_results)
    })
    .await??;
    drop(_guard);

    let count = nodes.len();
    // TODO(#32): source-info enrichment per node deferred
    Ok(json!({ "nodes": nodes, "count": count }))
}

async fn handle_list_relationships(
    req: &IpcRequest,
    state: Arc<AppState>,
) -> Result<Value, Error> {
    let p = &req.params;
    let num_results_raw = p["num_results"].as_i64().unwrap_or(1000);
    if num_results_raw <= 0 {
        return Err(Error::Ipc("num_results must be > 0".to_string()));
    }
    let num_results = num_results_raw as usize;
    let group_ids = extract_optional_group_ids(&p["group_ids"]);

    let db = Arc::clone(&state.db);
    let _guard = state.write_lock.read().await;
    let facts = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids
            .as_deref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        conn.list_relationships(group_ids.as_deref().map(|_| gid_refs.as_slice()), num_results)
    })
    .await??;
    drop(_guard);

    let count = facts.len();
    // TODO(#32): source-info enrichment per edge deferred
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

    let db = Arc::clone(&state.db);
    let _guard = state.write_lock.read().await;
    let (edges, nodes) = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids
            .as_deref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let (edges, neighbor_uuids) = conn.get_entity_neighbors(
            &entity_uuid,
            group_ids.as_deref().map(|_| gid_refs.as_slice()),
            num_results,
        )?;
        let nodes = conn.get_entities_by_uuids(&neighbor_uuids)?;
        Ok::<_, crate::error::Error>((edges, nodes))
    })
    .await??;
    drop(_guard);

    let center_uuid = p["entity_uuid"].as_str().unwrap_or("").to_string();
    let node_count = nodes.len();
    let edge_count = edges.len();
    // TODO(#32): source-info enrichment per node/edge deferred
    Ok(json!({
        "center_uuid": center_uuid,
        "nodes": nodes,
        "edges": edges,
        "node_count": node_count,
        "edge_count": edge_count,
    }))
}

// TODO(#32): per-result source-info enrichment (_serialize_nodes_with_sources) is deferred
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

    let db = Arc::clone(&state.db);
    let _guard = state.write_lock.read().await;
    let nodes = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids
            .as_deref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        conn.get_entities_by_source(
            &source,
            group_ids.as_deref().map(|_| gid_refs.as_slice()),
            num_results,
        )
    })
    .await??;
    drop(_guard);

    let source_val = p["source"].as_str().unwrap_or("").to_string();
    let node_count = nodes.len();
    // TODO(#32): source-info enrichment per node deferred
    Ok(json!({ "source": source_val, "nodes": nodes, "node_count": node_count }))
}

// ── helpers ───────────────────────────────────────────────────────────────────

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

/// Returns `None` when group_ids is absent/null/empty (= no filter = all groups).
fn extract_optional_group_ids(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(arr) if !arr.is_empty() => {
            let ids: Vec<String> = arr
                .iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect();
            if ids.is_empty() {
                None
            } else {
                Some(ids)
            }
        }
        _ => None,
    }
}
