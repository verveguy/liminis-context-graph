mod sink;

use std::sync::Arc;

use liminis_graph_core::{app_state::AppState, db::Db, handlers, ipc::IpcRequest, IpcResponse};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = std::env::var("GRAPHITI_SOCKET_PATH")
        .unwrap_or_else(|_| ".graphiti/service.sock".to_string());
    let db_path =
        std::env::var("GRAPHITI_DB_PATH").unwrap_or_else(|_| ".graphiti/db/liminis.db".to_string());
    let embedding_dim: usize = std::env::var("GRAPHITI_EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(768);

    // Ensure parent directory exists
    if let Some(parent) = std::path::Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Open database and initialise schema (idempotent).
    // Vector indexes are NOT created here — HNSW blocks in-place writes.
    // The caller must issue knowledge_build_indices after bulk ingestion.
    let db = Arc::new(Db::open(&db_path)?);
    {
        let conn = db.connect()?;
        conn.init_schema(embedding_dim)?;
    }

    // TODO: LIMINIS_TELEMETRY_SOCKET — wire SocketSink here if env var is set
    let telemetry_sink: Arc<dyn liminis_graph_core::TelemetrySink> = sink::StderrSink::start();

    let state = Arc::new(AppState::from_env(Arc::clone(&telemetry_sink), db, db_path.clone()));

    // Remove stale socket file
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    eprintln!("liminis-graph: listening on {socket_path}");

    loop {
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);

        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                let req: IpcRequest = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(e) => {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": null,
                            "error": {"code": -32700, "message": format!("Parse error: {e}")}
                        });
                        let json = serde_json::to_string(&response).unwrap_or_default();
                        let _ = writer.write_all(format!("{json}\n").as_bytes()).await;
                        continue;
                    }
                };

                let is_close = req.method == "knowledge_close";
                let is_streaming = req
                    .params
                    .get("_progress_token")
                    .map(|v| !v.is_null())
                    .unwrap_or(false);

                let resp = if is_streaming {
                    // Streaming: drain progress lines before writing terminal response
                    let (tx, mut rx) =
                        tokio::sync::mpsc::unbounded_channel::<Value>();
                    let state_clone = Arc::clone(&state);
                    let req_id = req.id.clone();
                    let dispatch_handle =
                        tokio::spawn(handlers::dispatch(req, state_clone, Some(tx)));

                    // Drain progress lines until channel closes (tx dropped inside dispatch)
                    let mut client_ok = true;
                    while let Some(val) = rx.recv().await {
                        let json = serde_json::to_string(&val).unwrap_or_default();
                        if writer
                            .write_all(format!("{json}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            // Client disconnected; drop rx so cancel_fn detects closed channel,
                            // then abort the dispatch task to clean up the async wrapper.
                            client_ok = false;
                            break;
                        }
                    }

                    if client_ok {
                        dispatch_handle.await.unwrap_or_else(|_| {
                            IpcResponse::err(req_id, -32000, "Internal error")
                        })
                    } else {
                        // Drop rx before aborting so cancel_fn sees the closed channel promptly
                        drop(rx);
                        dispatch_handle.abort();
                        continue;
                    }
                } else {
                    handlers::dispatch(req, Arc::clone(&state), None).await
                };

                let json = serde_json::to_string(&resp).unwrap_or_default();
                let _ = writer.write_all(format!("{json}\n").as_bytes()).await;

                if is_close {
                    // Exit process cleanly — matches Python service behaviour.
                    std::process::exit(0);
                }
            }
        });
    }
}
