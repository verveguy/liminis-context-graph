mod sink;

use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;

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
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    sync::Notify,
    task::JoinSet,
};

async fn handle_connection(stream: UnixStream, state: Arc<AppState>, shutdown_notify: Arc<Notify>) {
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
                write_parse_error(&mut writer, e).await;
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
            handle_streaming_request(req, Arc::clone(&state), &mut writer).await
        } else {
            Some(handlers::dispatch(req, Arc::clone(&state), None).await)
        };

        if let Some(resp) = resp {
            let json = serde_json::to_string(&resp).unwrap_or_default();
            let _ = writer.write_all(format!("{json}\n").as_bytes()).await;
        }

        if is_close {
            // Trigger graceful shutdown instead of std::process::exit(0) (R3).
            shutdown_notify.notify_one();
            return;
        }
    }
}

async fn write_parse_error(writer: &mut OwnedWriteHalf, e: serde_json::Error) {
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {"code": -32700, "message": format!("Parse error: {e}")}
    });
    let json = serde_json::to_string(&response).unwrap_or_default();
    let _ = writer.write_all(format!("{json}\n").as_bytes()).await;
}

/// Returns `Some(response)` if the streaming dispatch produced a final response, or `None` if the
/// client disconnected and the dispatch task was aborted.
async fn handle_streaming_request(
    req: IpcRequest,
    state: Arc<AppState>,
    writer: &mut OwnedWriteHalf,
) -> Option<IpcResponse> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let req_id = req.id.clone();
    let dispatch_handle = tokio::spawn(handlers::dispatch(req, state, Some(tx)));

    let mut client_ok = true;
    while let Some(val) = rx.recv().await {
        let json = serde_json::to_string(&val).unwrap_or_default();
        if writer
            .write_all(format!("{json}\n").as_bytes())
            .await
            .is_err()
        {
            client_ok = false;
            break;
        }
    }

    if client_ok {
        Some(
            dispatch_handle
                .await
                .unwrap_or_else(|_| IpcResponse::err(req_id, -32000, "Internal error")),
        )
    } else {
        drop(rx);
        dispatch_handle.abort();
        None
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Phase A workspace dir auto-migration: rename .graphiti/ → .lcg/ on first run.
    // Executed before path resolution so deprecated GRAPHITI_* paths that point at
    // .graphiti/... can be rewritten after migration, preventing create_dir_all from
    // silently recreating an empty .graphiti/ alongside the migrated .lcg/.
    let migrated = if std::path::Path::new(".graphiti").exists()
        && !std::path::Path::new(".lcg").exists()
    {
        match std::fs::rename(".graphiti", ".lcg") {
            Ok(()) => {
                eprintln!(
                    "[liminis-context-graph] DEPRECATED: workspace directory \".graphiti\" has been \
                     renamed to \".lcg\". Update your configuration to use LCG_* env vars. \
                     The .graphiti fallback will be removed in Phase B (see issue #59)."
                );
                true
            }
            Err(e) => {
                eprintln!(
                    "[liminis-context-graph] WARNING: could not auto-migrate .graphiti/ → .lcg/: {e}. \
                     Continuing without migration."
                );
                false
            }
        }
    } else {
        false
    };

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

    // After a successful migration, rewrite any .graphiti/-prefixed path to .lcg/.
    // This handles the case where deprecated GRAPHITI_* env vars pointed at .graphiti/...
    // paths — without this, create_dir_all would recreate an empty .graphiti/ dir.
    let socket_path = if migrated && socket_path.starts_with(".graphiti/") {
        format!(".lcg/{}", &socket_path[".graphiti/".len()..])
    } else {
        socket_path
    };
    let db_path = if migrated && db_path.starts_with(".graphiti/") {
        format!(".lcg/{}", &db_path[".graphiti/".len()..])
    } else {
        db_path
    };

    // Ensure parent directories exist
    if let Some(parent) = std::path::Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Inner shutdown timeout: caps in-flight task drain to leave headroom under the
    // outer 60s budget (liminis-app SHUTDOWN_TIMEOUT_MS). Default: 30s.
    let shutdown_timeout_ms: u64 = std::env::var("LCG_SHUTDOWN_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);

    // Bind socket FIRST — this allows health_check and recovery IPC to work even
    // when the DB is in a degraded state. See ADR-0046.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    eprintln!("liminis-context-graph: listening on {socket_path}");

    // TODO: LIMINIS_TELEMETRY_SOCKET — wire SocketSink here if env var is set
    let (stderr_sink, sink_drain_handle) = sink::StderrSink::start();
    let telemetry_sink: Arc<dyn liminis_graph_core::TelemetrySink> = stderr_sink;

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

    // ── Signal handler setup (R1: installed BEFORE the accept loop) ───────────
    // SIGTERM: captured via tokio's unix signal infrastructure. The OS-level handler
    // is registered synchronously when signal() is called — the async task just drains it.
    let shutdown_notify = Arc::new(Notify::new());

    #[cfg(unix)]
    {
        let mut sigterm_stream = signal(SignalKind::terminate())?;
        let notify = Arc::clone(&shutdown_notify);
        tokio::spawn(async move {
            sigterm_stream.recv().await;
            eprintln!("liminis-context-graph: received SIGTERM, shutting down");
            notify.notify_one();
        });
    }
    {
        let notify = Arc::clone(&shutdown_notify);
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("liminis-context-graph: received SIGINT, shutting down");
            notify.notify_one();
        });
    }

    // ── Accept loop ───────────────────────────────────────────────────────────
    let mut join_set: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let state_clone = Arc::clone(&state);
                let notify_clone = Arc::clone(&shutdown_notify);
                join_set.spawn(handle_connection(stream, state_clone, notify_clone));
            }
            _ = shutdown_notify.notified() => {
                break;
            }
            // Reap completed connection tasks so the JoinSet doesn't grow unbounded
            // over long uptimes with many short-lived connections.
            Some(_) = join_set.join_next() => {}
        }
    }

    // ── Graceful shutdown sequence (R2, R4, R5, R6) ───────────────────────────
    // R6: Emit shutting_down state.
    state.shutdown.store(true, Ordering::Relaxed);
    telemetry_sink.emit(TelemetryEvent::ServiceState {
        ts_ms: now_ms(),
        state: "shutting_down".to_string(),
        reason: None,
        detail: None,
    });

    // R2/R5: Await in-flight connection tasks under the inner timeout.
    let drain_result = tokio::time::timeout(Duration::from_millis(shutdown_timeout_ms), async {
        while join_set.join_next().await.is_some() {}
    })
    .await;

    if drain_result.is_err() {
        eprintln!(
            "liminis-context-graph: shutdown timeout ({shutdown_timeout_ms}ms) exceeded, aborting tasks"
        );
        join_set.abort_all();
        while join_set.join_next().await.is_some() {}
    }

    // Abort any background rebuild tasks (they hold Arc<Db> clones).
    {
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        if let Ok(mut jobs) = state.rebuild_jobs.lock() {
            for job in jobs.values_mut() {
                if let Some(handle) = job.spawn_handle.take() {
                    handles.push(handle);
                }
            }
        }
        for handle in handles {
            handle.abort();
            // Await to let the tokio runtime reclaim the task slot; JoinError expected on abort.
            let _ = handle.await;
        }
    }

    // R2: Drop AppState — drops Arc<Db>. If refcount reaches 0, the cxx::UniquePtr<ffi::Database>
    // destructor fires the LadybugDB WAL checkpoint. Connection tasks were awaited above.
    // Note: spawn_blocking threads inside aborted tasks hold Arc<Db> until they finish naturally;
    // the WAL checkpoint fires when those threads complete (best-effort per R5).
    drop(state);

    // R6: Emit stopped state before exiting.
    telemetry_sink.emit(TelemetryEvent::ServiceState {
        ts_ms: now_ms(),
        state: "stopped".to_string(),
        reason: None,
        detail: None,
    });

    // Drop last sender so the drain task sees channel close and exits its loop.
    drop(telemetry_sink);
    // Await drain task to flush the "stopped" event to stderr before exit.
    sink_drain_handle.await.ok();

    std::process::exit(0);
}
