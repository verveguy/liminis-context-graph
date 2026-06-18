mod migration;
mod sink;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use liminis_graph_core::{
    app_state::AppState,
    db::Db,
    embedder::{is_transport_error, Embedder, OaiEmbedder},
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
    // Sink is created first so migration events are captured before any other work.
    // TODO: LIMINIS_TELEMETRY_SOCKET — wire SocketSink here if env var is set
    let (stderr_sink, sink_drain_handle) = sink::StderrSink::start();
    let telemetry_sink: Arc<dyn liminis_graph_core::TelemetrySink> = stderr_sink;

    // Structured workspace migration: .graphiti/ → .lcg/ with file-layout restructuring.
    // Runs before path resolution so deprecated GRAPHITI_* env-var paths can be rewritten
    // below, preventing create_dir_all from crashing on the legacy file-as-dir layout.
    let (pre_migration_degraded, did_migrate) =
        match migration::migrate_workspace(Path::new("."), &*telemetry_sink) {
            Ok(migration::MigrationOutcome::Migrated) => (None, true),
            Ok(migration::MigrationOutcome::AlreadyMigrated) => (None, true),
            Ok(migration::MigrationOutcome::NothingToMigrate) => (None, false),
            Err(migration::MigrationError::Schism { guidance }) => {
                eprintln!("liminis-context-graph: FATAL workspace schism: {guidance}");
                drop(telemetry_sink);
                sink_drain_handle.await.ok();
                return Err(guidance.into());
            }
            Err(e) => {
                eprintln!("liminis-context-graph: migration failed, entering degraded mode: {e}");
                telemetry_sink.emit(TelemetryEvent::ServiceState {
                    ts_ms: now_ms(),
                    state: "degraded".to_string(),
                    reason: Some("migration_failed".to_string()),
                    detail: Some(serde_json::Value::String(e.to_string())),
                });
                (Some("migration_failed".to_string()), false)
            }
        };

    // deprecated: remove in Phase B (see #59)
    let socket_path = lcg_env_var("LCG_SOCKET_PATH", "GRAPHITI_SOCKET_PATH")
        .unwrap_or_else(|_| ".lcg/service.sock".to_string());
    // deprecated: remove in Phase B (see #59)
    let db_path = lcg_env_var("LCG_DB_PATH", "GRAPHITI_DB_PATH")
        .unwrap_or_else(|_| ".lcg/db/liminis.db".to_string());

    // ── CLI arg parsing: --embedder-uds <path> | --embedder-http <url> ───────────
    // Manual scan — only two mutually exclusive flags; clap would be overkill.
    let mut cli_uds: Option<String> = None;
    let mut cli_http: Option<String> = None;
    {
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--embedder-uds" => {
                    i += 1;
                    cli_uds = Some(
                        args.get(i)
                            .cloned()
                            .ok_or("--embedder-uds requires a socket path argument")?,
                    );
                }
                "--embedder-http" => {
                    i += 1;
                    cli_http = Some(
                        args.get(i)
                            .cloned()
                            .ok_or("--embedder-http requires a URL argument")?,
                    );
                }
                _ => {}
            }
            i += 1;
        }
    }
    if cli_uds.is_some() && cli_http.is_some() {
        return Err(
            "--embedder-uds and --embedder-http are mutually exclusive; specify only one".into(),
        );
    }

    // ── Transport resolution (FR-003/FR-004/FR-007) ───────────────────────────────
    // Priority: CLI flag > default UDS path (if socket exists) > LCG_EMBEDDING_URL env > error
    const DEFAULT_UDS_PATH: &str = "/tmp/liminis-inference.sock";
    let embedder_model = lcg_env_var("LCG_EMBEDDING_MODEL", "GRAPHITI_EMBEDDING_MODEL")
        .unwrap_or_else(|_| "bge-base-en-v1.5".to_string());

    // Dim override — used as fallback if probe fails (FR-008)
    let embedding_dim_override: Option<usize> =
        lcg_env_var("LCG_EMBEDDING_DIM", "GRAPHITI_EMBEDDING_DIM")
            .ok()
            .and_then(|s| s.parse().ok());

    enum ResolvedTransport {
        Http(String),
        #[cfg(unix)]
        Uds(String),
    }

    let resolved = if let Some(uds_path) = cli_uds {
        // FR-010: validate socket exists at startup
        #[cfg(unix)]
        {
            if !std::path::Path::new(&uds_path).exists() {
                return Err(format!(
                    "UDS socket not found at {uds_path}. \
                     Ensure the liminis-inference sidecar is running."
                )
                .into());
            }
            ResolvedTransport::Uds(uds_path)
        }
        #[cfg(not(unix))]
        {
            return Err("--embedder-uds is only supported on Unix platforms".into());
        }
    } else if let Some(http_url) = cli_http {
        // FR-011: validate URL format — must have a scheme and a non-empty host.
        let host_part = http_url
            .strip_prefix("https://")
            .or_else(|| http_url.strip_prefix("http://"));
        if host_part.map(|h| h.is_empty()).unwrap_or(true) {
            return Err(format!(
                "Invalid --embedder-http URL: {http_url:?}. \
                 Must start with http:// or https:// and include a host."
            )
            .into());
        }
        ResolvedTransport::Http(http_url)
    } else {
        // No CLI flag — apply default resolution order
        #[cfg(unix)]
        if std::path::Path::new(DEFAULT_UDS_PATH).exists() {
            ResolvedTransport::Uds(DEFAULT_UDS_PATH.to_string())
        } else if let Ok(url) = lcg_env_var("LCG_EMBEDDING_URL", "GRAPHITI_EMBEDDING_URL") {
            ResolvedTransport::Http(url)
        } else {
            return Err(format!(
                "No embedder configured: default UDS socket {DEFAULT_UDS_PATH} not found and \
                 LCG_EMBEDDING_URL is not set. Pass --embedder-uds or --embedder-http, or \
                 start the liminis-inference sidecar."
            )
            .into());
        }
        #[cfg(not(unix))]
        {
            // Non-Unix: fall back to HTTP only
            if let Ok(url) = lcg_env_var("LCG_EMBEDDING_URL", "GRAPHITI_EMBEDDING_URL") {
                ResolvedTransport::Http(url)
            } else {
                ResolvedTransport::Http("http://127.0.0.1:8765/v1/embeddings".to_string())
            }
        }
    };

    // After migration, rewrite deprecated GRAPHITI_* env-var paths to the new layout.
    // Use specific mappings rather than a generic prefix-swap: the legacy db path maps to
    // a different filename (.graphiti/db → .lcg/db/liminis.db), not just a new prefix.
    let socket_path = if did_migrate && socket_path == ".graphiti/service.sock" {
        ".lcg/service.sock".to_string()
    } else if did_migrate && socket_path.starts_with(".graphiti/") {
        format!(".lcg/{}", &socket_path[".graphiti/".len()..])
    } else {
        socket_path
    };
    let db_path = if did_migrate && db_path == ".graphiti/db" {
        ".lcg/db/liminis.db".to_string()
    } else if did_migrate && db_path.starts_with(".graphiti/") {
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
    // outer 60s budget (liminis-app SHUTDOWN_TIMEOUT_MS). Default: 5s — cancellation
    // makes fast exit the common case; this is a fallback for misbehaving handlers.
    let shutdown_timeout_ms: u64 = std::env::var("LCG_SHUTDOWN_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5_000);

    // Bind socket FIRST — this allows health_check and recovery IPC to work even
    // when the DB is in a degraded state. See ADR-0046.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    eprintln!("liminis-context-graph: listening on {socket_path}");

    // ── Build probe embedder, then final embedder with discovered dim ─────────
    // The probe runs before DB open so that a misconfigured embedder fails fast
    // at startup rather than on the first embed request (FR-010/FR-011).
    let probe_embedder = match &resolved {
        ResolvedTransport::Http(url) => {
            OaiEmbedder::new_http(url.clone(), embedder_model.clone(), 1)
        }
        #[cfg(unix)]
        ResolvedTransport::Uds(path) => {
            OaiEmbedder::new_uds(path.clone(), embedder_model.clone(), 1)
        }
    };

    let (transport_label, endpoint) = probe_embedder.transport_info();

    let (embedding_dim, embedding_model_probed) = match probe_embedder.probe().await {
        Ok(result) => result,
        Err(e) if is_transport_error(&e) => {
            // FR-011: transport/connectivity failures are always fatal at startup.
            // LCG_EMBEDDING_DIM cannot override an unreachable embedder.
            return Err(format!(
                "embedder unreachable at startup: {e}. \
                 Ensure the embedder sidecar is running before starting liminis-context-graph."
            )
            .into());
        }
        Err(e) => {
            // Non-transport probe failure (e.g., unexpected response shape).
            // LCG_EMBEDDING_DIM can override this per FR-008.
            if let Some(dim) = embedding_dim_override {
                eprintln!(
                    "liminis-context-graph: embedder probe failed ({e}), \
                     using LCG_EMBEDDING_DIM={dim} override"
                );
                (dim, embedder_model.clone())
            } else {
                return Err(
                    format!("embedder probe failed and LCG_EMBEDDING_DIM is not set: {e}").into(),
                );
            }
        }
    };

    eprintln!("embedder: transport={transport_label}, endpoint={endpoint}, dim={embedding_dim}");

    // Build the final embedder with the correct probed dim
    let embedder: Arc<dyn Embedder> = match &resolved {
        ResolvedTransport::Http(url) => Arc::new(OaiEmbedder::new_http(
            url.clone(),
            embedding_model_probed.clone(),
            embedding_dim,
        )),
        #[cfg(unix)]
        ResolvedTransport::Uds(path) => Arc::new(OaiEmbedder::new_uds(
            path.clone(),
            embedding_model_probed.clone(),
            embedding_dim,
        )),
    };

    // Derive wal_dir using the same env-var logic as AppState::from_env.
    // Available before DB open so startup recovery can use it without AppState.
    let startup_wal_dir = std::path::PathBuf::from(
        lcg_env_var("LCG_WAL_DIR", "GRAPHITI_WAL_DIR")
            .unwrap_or_else(|_| ".lcg/wal".to_string()),
    );

    // Attempt to open database and initialize schema. Classify errors:
    //   - Recoverable (lbug WAL corruption, permission denied, missing file) → autonomous
    //     startup self-recovery first; degraded mode only if recovery itself fails.
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
                    // Attempt autonomous self-recovery before entering degraded mode (FR-001).
                    let recovery_db_path = db_path.clone();
                    let recovery_wal_dir = startup_wal_dir.clone();
                    let recovery_sink = Arc::clone(&telemetry_sink);
                    let recovery_result = tokio::task::spawn_blocking(move || {
                        liminis_graph_core::recovery::run_full_recovery_sequence(
                            &recovery_db_path,
                            &recovery_wal_dir,
                            embedding_dim,
                            recovery_sink,
                        )
                    })
                    .await;

                    match recovery_result {
                        Ok(Ok((db, report))) => {
                            eprintln!(
                                "liminis-context-graph: startup self-recovery complete — \
                                 episodes_before={} mutations_replayed={} episodes_after={} \
                                 from_seq={} cursor={}",
                                report.episodes_before,
                                report.mutations_replayed,
                                report.episodes_after,
                                report.from_seq,
                                report.cursor_reason.as_str(),
                            );
                            telemetry_sink.emit(TelemetryEvent::ServiceState {
                                ts_ms: now_ms(),
                                state: "healthy".to_string(),
                                reason: Some("startup_auto_recovery".to_string()),
                                detail: None,
                            });
                            (Some(Arc::new(db)), None)
                        }
                        Ok(Err(recovery_err)) => {
                            // Recovery sequence failed — fall back to degraded mode.
                            let reason = "lbug_wal_corrupt".to_string();
                            eprintln!(
                                "liminis-context-graph: startup self-recovery failed: \
                                 {recovery_err} — entering degraded mode"
                            );
                            telemetry_sink.emit(TelemetryEvent::ServiceState {
                                ts_ms: now_ms(),
                                state: "degraded".to_string(),
                                reason: Some(reason.clone()),
                                detail: Some(serde_json::Value::String(msg)),
                            });
                            (None, Some(reason))
                        }
                        Err(join_err) => {
                            // spawn_blocking panicked — fall back to degraded mode.
                            let reason = "lbug_wal_corrupt".to_string();
                            eprintln!(
                                "liminis-context-graph: startup self-recovery task panicked: \
                                 {join_err} — entering degraded mode"
                            );
                            telemetry_sink.emit(TelemetryEvent::ServiceState {
                                ts_ms: now_ms(),
                                state: "degraded".to_string(),
                                reason: Some(reason.clone()),
                                detail: Some(serde_json::Value::String(msg)),
                            });
                            (None, Some(reason))
                        }
                    }
                } else {
                    return Err(e);
                }
            }
        }
    };

    // migration_failed takes precedence over db-open degraded reason.
    let degraded_reason = pre_migration_degraded.or(degraded_reason);

    let state = Arc::new(AppState::from_env(
        Arc::clone(&telemetry_sink),
        maybe_db,
        degraded_reason,
        db_path.clone(),
        embedder,
        embedding_model_probed,
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
    // Cancel all in-flight async work so tasks exit at the next phase boundary
    // rather than waiting out the full timeout on long HTTP calls.
    state.cancel_token.cancel();
    // R6: Emit shutting_down state.
    telemetry_sink.emit(TelemetryEvent::ServiceState {
        ts_ms: now_ms(),
        state: "shutting_down".to_string(),
        reason: None,
        detail: None,
    });

    // R2/R5: Await in-flight connection tasks under the inner timeout.
    let drained = {
        let drain_result =
            tokio::time::timeout(Duration::from_millis(shutdown_timeout_ms), async {
                let mut n = 0u64;
                while join_set.join_next().await.is_some() {
                    n += 1;
                }
                n
            })
            .await;

        match drain_result {
            Ok(n) => n,
            Err(_) => {
                eprintln!(
                    "liminis-context-graph: shutdown timeout ({shutdown_timeout_ms}ms) exceeded, aborting tasks"
                );
                join_set.abort_all();
                let mut n = 0u64;
                while join_set.join_next().await.is_some() {
                    n += 1;
                }
                n
            }
        }
    };

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

    // Clone cancelled_chunks before drop(state) so the count survives the state drop.
    let cancelled_chunks = Arc::clone(&state.cancelled_chunks);
    // R2: Drop AppState — drops Arc<Db>. If refcount reaches 0, the cxx::UniquePtr<ffi::Database>
    // destructor fires the LadybugDB WAL checkpoint. Connection tasks were awaited above.
    // spawn_blocking threads that hold Arc<Db> clones will release them when the tokio runtime
    // drops at the end of main() — guaranteed before process exit (see ADR-0049).
    drop(state);

    let cancelled = cancelled_chunks.load(std::sync::atomic::Ordering::Relaxed) as u64;
    // R6: Emit stopped state before exiting.
    telemetry_sink.emit(TelemetryEvent::ServiceState {
        ts_ms: now_ms(),
        state: "stopped".to_string(),
        reason: None,
        detail: Some(serde_json::json!({"drained": drained, "cancelled": cancelled})),
    });

    // Drop last sender so the drain task sees channel close and exits its loop.
    drop(telemetry_sink);
    // Await drain task to flush the "stopped" event to stderr before exit.
    sink_drain_handle.await.ok();

    Ok(())
}
