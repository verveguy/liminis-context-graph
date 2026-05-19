use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::{
    db::Db,
    embedder::Embedder,
    episode,
    error::Error,
    extractor::Extractor,
    ipc::{IpcRequest, IpcResponse},
    search,
    telemetry::{now_ms, TelemetryEvent, TelemetrySink},
};

const DEFAULT_GROUP_ID: &str = "liminis";

/// Dispatches an IPC request to the appropriate library function (AD-2, T020). [HOT]
pub async fn dispatch(
    req: IpcRequest,
    db: Arc<Db>,
    embedder: Arc<Embedder>,
    extractor: Arc<Extractor>,
    sink: Arc<dyn TelemetrySink>,
) -> IpcResponse {
    let method = req.method.clone();
    let request_id = req.id.clone();
    let start = Instant::now();

    let (response, success) = match handle(&req, db, embedder, extractor).await {
        Ok(result) => (IpcResponse::ok(req.id, result), true),
        Err(e) => (IpcResponse::err(req.id, -32000, e.to_string()), false),
    };

    sink.emit(TelemetryEvent::IpcCall {
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
    db: Arc<Db>,
    embedder: Arc<Embedder>,
    extractor: Arc<Extractor>,
) -> Result<Value, Error> {
    match req.method.as_str() {
        "knowledge_add_episode" => handle_add_episode(req, db, embedder, extractor).await,
        "knowledge_find_entities" => handle_find_entities(req, db, embedder).await,
        "knowledge_find_relationships" => handle_find_relationships(req, db, embedder).await,
        "knowledge_get_episodes" => handle_get_episodes(req, db).await,
        "knowledge_delete_episode" => handle_delete_episode(req, db).await,
        "knowledge_get_nodes_by_group" => handle_get_nodes_by_group(req, db).await,
        "knowledge_get_edges_by_group" => handle_get_edges_by_group(req, db).await,
        "knowledge_get_edges_by_uuids" => handle_get_edges_by_uuids(req, db).await,
        "knowledge_query_cypher" => handle_query_cypher(req, db).await,
        "knowledge_build_indices" => handle_build_indices(db).await,
        "knowledge_close" => Ok(json!({"status": "closed"})),
        _ => Err(Error::Ipc(format!("Method not found: {}", req.method))),
    }
}

async fn handle_add_episode(
    req: &IpcRequest,
    db: Arc<Db>,
    embedder: Arc<Embedder>,
    extractor: Arc<Extractor>,
) -> Result<Value, Error> {
    let p = &req.params;
    let name = p["name"].as_str().unwrap_or("").to_string();
    let body = p["episode_body"].as_str().unwrap_or("").to_string();
    let source = p["source"].as_str().unwrap_or("text").to_string();
    let source_desc = p["source_description"].as_str().unwrap_or("").to_string();
    let ref_time = p["reference_time"].as_str().unwrap_or("").to_string();
    let group_id = p["group_id"].as_str().unwrap_or(DEFAULT_GROUP_ID).to_string();

    let episode_uuid = episode::add_episode(
        db, embedder, extractor, &name, &body, &source, &source_desc, &ref_time, &group_id,
    )
    .await?;

    Ok(json!({ "episode_uuid": episode_uuid }))
}

async fn handle_find_entities(
    req: &IpcRequest,
    db: Arc<Db>,
    embedder: Arc<Embedder>,
) -> Result<Value, Error> {
    let p = &req.params;
    let query = p["query"].as_str().unwrap_or("").to_string();
    let group_ids = extract_group_ids(&p["group_ids"]);
    let limit = p["num_results"].as_u64().unwrap_or(10) as usize;

    let entities = search::hybrid_entity_search(db, embedder, &query, group_ids, limit).await?;
    Ok(serde_json::to_value(entities)?)
}

async fn handle_find_relationships(
    req: &IpcRequest,
    db: Arc<Db>,
    embedder: Arc<Embedder>,
) -> Result<Value, Error> {
    let p = &req.params;
    let query = p["query"].as_str().unwrap_or("").to_string();
    let group_ids = extract_group_ids(&p["group_ids"]);
    let limit = p["num_results"].as_u64().unwrap_or(10) as usize;

    let edges = search::hybrid_edge_search(db, embedder, &query, group_ids, limit).await?;
    Ok(serde_json::to_value(edges)?)
}

async fn handle_get_episodes(req: &IpcRequest, db: Arc<Db>) -> Result<Value, Error> {
    let p = &req.params;
    let group_id = p["group_id"].as_str().unwrap_or(DEFAULT_GROUP_ID).to_string();
    let last_n = p["last_n"].as_u64().unwrap_or(50) as usize;

    let episodes = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.retrieve_episodes(&group_id, last_n)
    })
    .await??;

    Ok(serde_json::to_value(episodes)?)
}

async fn handle_delete_episode(req: &IpcRequest, db: Arc<Db>) -> Result<Value, Error> {
    let episode_uuid = req.params["episode_uuid"]
        .as_str()
        .unwrap_or("")
        .to_string();

    tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.remove_episode(&episode_uuid)
    })
    .await??;

    Ok(json!({"status": "deleted"}))
}

async fn handle_get_nodes_by_group(req: &IpcRequest, db: Arc<Db>) -> Result<Value, Error> {
    let group_ids = extract_group_ids(&req.params["group_ids"]);

    let nodes = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids.iter().map(String::as_str).collect();
        conn.get_entities_by_group_ids(&gid_refs)
    })
    .await??;

    Ok(serde_json::to_value(nodes)?)
}

async fn handle_get_edges_by_group(req: &IpcRequest, db: Arc<Db>) -> Result<Value, Error> {
    let group_ids = extract_group_ids(&req.params["group_ids"]);

    let edges = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids.iter().map(String::as_str).collect();
        conn.get_edges_by_group_ids(&gid_refs)
    })
    .await??;

    Ok(serde_json::to_value(edges)?)
}

async fn handle_get_edges_by_uuids(req: &IpcRequest, db: Arc<Db>) -> Result<Value, Error> {
    let uuids: Vec<String> = req.params["uuids"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    let edges = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        let uuid_refs: Vec<&str> = uuids.iter().map(String::as_str).collect();
        conn.get_edges_by_uuids(&uuid_refs)
    })
    .await??;

    Ok(serde_json::to_value(edges)?)
}

async fn handle_query_cypher(req: &IpcRequest, db: Arc<Db>) -> Result<Value, Error> {
    let query = req.params["query"].as_str().unwrap_or("").to_string();

    let rows = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.cypher_query(&query)
    })
    .await??;

    Ok(json!({"rows": rows}))
}

async fn handle_build_indices(db: Arc<Db>) -> Result<Value, Error> {
    tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.build_indices_and_constraints()
    })
    .await??;

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
