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

    let episode_uuid = episode::add_episode(
        state,
        &name,
        &body,
        &source,
        &source_desc,
        &ref_time,
        &group_id,
    )
    .await?;

    Ok(json!({ "episode_uuid": episode_uuid }))
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
