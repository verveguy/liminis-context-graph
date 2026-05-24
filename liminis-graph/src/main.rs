mod sink;

use std::sync::Arc;

use liminis_graph_core::{
    app_state::AppState,
    db::Db,
    env::lcg_env_var,
    handlers,
    ipc::IpcRequest,
    telemetry::{now_ms, TelemetryEvent},
    IpcResponse,
};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // deprecated: remove in Phase B (see #59)
    let socket_path = lcg_env_var("LCG_SOCKET_PATH", "GRAPHITI_SOCKET_PATH")
        .unwrap_or_else(|_| ".lcg/service.sock".to_string());
    // deprecated: remove in Phase B (see #59)
    let db_path = lcg_env_var("LCG_DB_PATH", "GRAPHITI_DB_PATH")
        .unwrap_or_else(|_| ".lcg/db/liminis.db".to_string());
    // deprecated: remove in Phase B (see #59)
    let embedding_dim: usize = lcg_env_var("LCG_EMBEDDING_DIM", "GRAPHITI_EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(768);

    // Phase A workspace dir auto-migration: rename .graphiti/ → .lcg/ on first run.
    // Must execute before create_dir_all so the absence of .lcg/ is detectable.
    if std::path::Path::new(".graphiti").exists() && !std::path::Path::new(".lcg").exists() {
        match std::fs::rename(".graphiti", ".lcg") {
            Ok(()) => eprintln!(
                "[liminis-context-graph] DEPRECATED: workspace directory \".graphiti\" has been \
                 renamed to \".lcg\". Update your configuration to use LCG_* env vars. \
                 The .graphiti fallback will be removed in Phase B (see issue #59)."
            ),
            Err(e) => eprintln!(
                "[liminis-context-graph] WARNING: could not auto-migrate .graphiti/ → .lcg/: {e}. \
                 Continuing without migration."
            ),
        }
    }

    // Ensure parent directories exist
    if let Some(parent) = std::path::Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Bind socket FIRST — this allows health_check and recovery IPC to work even
    // when the DB is in a degraded state. See ADR-0046.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    eprintln!("liminis-context-graph: listening on {socket_path}");

    // TODO: LIMINIS_TELEMETRY_SOCKET — wire SocketSink here if env var is set
    let telemetry_sink: Arc<dyn liminis_graph_core::TelemetrySink> = sink::StderrSink::start();

    // Attempt to open database and initialize schema. Classify errors:
    //   - Recoverable (lbug WAL corruption, permission denied, missing file) → degraded mode
    //   - Fatal (everything else) → propagate via ? and let the process exit
    let (maybe_db, degraded_reason): (Option<Arc<Db>>, Option<String>) = {
        let open_result = (|| -> Result<Db, Box<dyn std::error::Error>> {
            let db = Db::open(&db_path)?;
            {
                let conn = db.connect()?;
                conn.init_schema(embedding_dim)?;
            }
            Ok(db)
        })();

        match open_result {
            Ok(db) => (Some(Arc::new(db)), None),
            Err(e) => {
                let msg = e.to_string();
                let is_recoverable = msg.contains("Corrupted wal file")
                    || msg.contains("Permission denied")
                    || msg.contains("No such file or directory");

                if is_recoverable {
                    let reason = "lbug_wal_corrupt".to_string();
                    telemetry_sink.emit(TelemetryEvent::ServiceState {
                        ts_ms: now_ms(),
                        state: "degraded".to_string(),
                        reason: Some(reason.clone()),
                        detail: Some(msg),
                    });
                    (None, Some(reason))
                } else {
                    return Err(e);
                }
            }
        }
    };

    let state = Arc::new(AppState::from_env(
        Arc::clone(&telemetry_sink),
        maybe_db,
        degraded_reason,
        db_path.clone(),
    ));

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
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
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
                        dispatch_handle
                            .await
                            .unwrap_or_else(|_| IpcResponse::err(req_id, -32000, "Internal error"))
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
