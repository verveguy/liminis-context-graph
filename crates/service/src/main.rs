mod cli;
mod mcp;
mod migration;
#[cfg(unix)]
mod sigterm_diag;
mod sink;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use cli::{CliMode, EmbedderFlag, ExtractorFlag};
use lcg_core::{
    app_state::AppState,
    cassette::{CassetteWriter, RecordingExtractor, ReplayingExtractor},
    db::Db,
    embedder::{is_transport_error, Embedder, OaiEmbedder},
    env::lcg_env_var,
    error::Error as CoreError,
    extraction_failures::{
        ExtractionFailureSink, ExtractionFailureWriter, DEFAULT_MAX_BYTES_PER_FILE,
    },
    extractor::{Extractor, OaiExtractor, UnconfiguredExtractor, ANTHROPIC_API_URL},
    handlers,
    ipc::IpcRequest,
    llm_router::LlmRouter,
    telemetry::{now_ms, TeeSink, TelemetryEvent, TelemetrySink},
    IpcResponse,
};
use rmcp::ServiceExt;
use serde_json::Value;
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    sync::Notify,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

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

/// Fans `telemetry_sink` out to also feed the `<cassette>.failures.jsonl` sidecar (#306
/// FR-001) for a `LCG_RECORD_LLM` cassette at `cassette_path`. `TelemetryEvent::ExtractionFailure`
/// is emitted from inside `AnthropicExtractor`/`OaiExtractor` themselves — a layer
/// `RecordingExtractor` (which only ever observes the whole `extract()` call's success value)
/// cannot see — so the combined sink must be passed to the *leaf* extractor's constructor, not
/// installed on `RecordingExtractor`.
fn recording_sink(
    telemetry_sink: &Arc<dyn TelemetrySink>,
    cassette_path: &str,
) -> Result<Arc<dyn TelemetrySink>, CoreError> {
    let failure_writer = ExtractionFailureWriter::open(cassette_path, DEFAULT_MAX_BYTES_PER_FILE)?;
    Ok(Arc::new(TeeSink::new(vec![
        Arc::clone(telemetry_sink),
        Arc::new(ExtractionFailureSink::new(failure_writer)) as Arc<dyn TelemetrySink>,
    ])))
}

/// Resolves the embedder transport, probes it, opens the DB (with startup self-recovery per
/// ADR-0009), and builds `AppState`. Shared by the socket service and standalone MCP mode
/// (`--mcp-stdio` without `--connect`) so both reuse byte-for-byte the same bootstrap path —
/// attached MCP mode (`--mcp-stdio --connect <path>`) never calls this at all, since it forwards
/// every call to an already-running service instead of opening the DB itself (FR-006).
async fn bootstrap_app_state(
    telemetry_sink: Arc<dyn TelemetrySink>,
    pre_migration_degraded: Option<String>,
    db_path: String,
    embedder_flag: Option<EmbedderFlag>,
    extractor_flag: Option<ExtractorFlag>,
) -> Result<Arc<AppState>, Box<dyn std::error::Error>> {
    let (cli_uds, cli_http) = match embedder_flag {
        Some(EmbedderFlag::Uds(p)) => (Some(p), None),
        Some(EmbedderFlag::Http(u)) => (None, Some(u)),
        None => (None, None),
    };

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

    // ── Record/replay cassette resolution (#232, FR-002/FR-007) ────────────────────
    // Read before any provider resolution below: replay mode bypasses provider resolution
    // entirely (FR-002 — zero network access, no credentials needed, by construction — see
    // `ReplayingExtractor`), and record mode wraps whichever provider is resolved. Checked for
    // mutual exclusivity up front so a misconfigured combination fails fast and loudly rather
    // than silently picking one mode over the other.
    let record_llm_path = std::env::var("LCG_RECORD_LLM").ok();
    let replay_llm_path = std::env::var("LCG_REPLAY_LLM").ok();
    if record_llm_path.is_some() && replay_llm_path.is_some() {
        return Err(
            "LCG_RECORD_LLM and LCG_REPLAY_LLM cannot both be set — recording captures a live \
             extraction pass to a cassette, replay serves calls from one with no network access; \
             these are mutually exclusive modes."
                .into(),
        );
    }

    let extractor: Arc<dyn Extractor> = if let Some(path) = replay_llm_path {
        eprintln!("extractor: provider=cassette-replay, path={path}");
        Arc::new(ReplayingExtractor::load(&path)?)
    } else {
        // ── Extractor resolution (FR-006/FR-007) ───────────────────────────────────────
        // Priority: explicit CLI flag (always selects the local adapter, regardless of
        // ANTHROPIC_API_KEY) > ANTHROPIC_API_KEY set (Anthropic path, byte-for-byte unchanged,
        // FR-015) > LCG_EXTRACTION_URL env > fatal error (FR-011). Unlike the embedder, there is
        // no live probe here — extraction has no response shape to auto-detect, and a blocking
        // Foundation-Models warm-up call would regress startup latency for no benefit.
        // Reachability failures at call time surface through the normal `Extractor` error path
        // (FR-010), not here.
        //
        // Deliberately NOT mirrored from the embedder: no default-UDS-socket auto-detection
        // tier. The bundled macOS sidecar's /v1/chat/completions route (Apple Foundation
        // Models) was evaluated in prior work and found inadequate for entity/relationship
        // extraction — insufficient context window and capability (see #227/#228). Silently
        // selecting it just because the socket exists and no ANTHROPIC_API_KEY is set would
        // trade a false "requires a hosted key" claim for an equally misleading "fully local
        // extraction just works" one. Selecting that same sidecar remains possible — via
        // explicit `--extractor-uds` or `LCG_EXTRACTION_URL` — it is simply never the silent
        // default (see PR #223 review discussion).
        let extractor_model =
            std::env::var("LCG_EXTRACTION_MODEL").unwrap_or_else(|_| "local".to_string());

        enum ResolvedExtractor {
            Anthropic,
            Http(String),
            #[cfg(unix)]
            Uds(String),
            /// Nothing configured (#331 FR-001): no longer fatal at startup — extraction
            /// becomes fatal only if and when an extraction-dependent method is actually
            /// called (FR-002).
            Unconfigured,
        }

        let (extractor_cli_uds, extractor_cli_http) = match extractor_flag {
            Some(ExtractorFlag::Uds(p)) => (Some(p), None),
            Some(ExtractorFlag::Http(u)) => (None, Some(u)),
            None => (None, None),
        };

        let resolved_extractor = if let Some(uds_path) = extractor_cli_uds {
            #[cfg(unix)]
            {
                if !std::path::Path::new(&uds_path).exists() {
                    return Err(format!(
                        "extractor UDS socket not found at {uds_path}. \
                         Ensure the liminis-inference sidecar is running."
                    )
                    .into());
                }
                ResolvedExtractor::Uds(uds_path)
            }
            #[cfg(not(unix))]
            {
                return Err("--extractor-uds is only supported on Unix platforms".into());
            }
        } else if let Some(http_url) = extractor_cli_http {
            let host_part = http_url
                .strip_prefix("https://")
                .or_else(|| http_url.strip_prefix("http://"));
            // A leading '/' right after the scheme (e.g. "http:///path") means the host segment
            // is empty even though the raw suffix isn't — split off the host before checking.
            let has_host = host_part
                .map(|h| !h.split('/').next().unwrap_or("").is_empty())
                .unwrap_or(false);
            if !has_host {
                return Err(format!(
                    "Invalid --extractor-http URL: {http_url:?}. \
                     Must start with http:// or https:// and include a host."
                )
                .into());
            }
            ResolvedExtractor::Http(http_url)
        } else if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            // FR-007: an already-configured ANTHROPIC_API_KEY is never silently overridden by a
            // reachable local sidecar when no explicit local-endpoint flag was given.
            ResolvedExtractor::Anthropic
        } else if let Ok(url) = std::env::var("LCG_EXTRACTION_URL") {
            ResolvedExtractor::Http(url)
        } else {
            // #331 FR-001: a missing extraction provider no longer prevents startup — it only
            // becomes fatal if and when an extraction-dependent method is actually called (see
            // the `Unconfigured` arm below and `UnconfiguredExtractor`).
            ResolvedExtractor::Unconfigured
        };

        // record_llm_path, if set, is applied per-arm below: the Anthropic arm wraps each
        // LlmRouter leaf individually (via `from_env_with`) so a primary→fallback failover
        // still produces two distinguishable, correctly-attributed cassette entries (#232 User
        // Story 4); the local (Http/Uds) arms wrap the single bare `OaiExtractor` directly.
        match resolved_extractor {
            ResolvedExtractor::Anthropic => {
                eprintln!(
                    "extractor: provider=anthropic, transport=http, endpoint={ANTHROPIC_API_URL}"
                );
                match &record_llm_path {
                    Some(path) => {
                        let writer = Arc::new(CassetteWriter::open(path)?);
                        let leaf_sink = recording_sink(&telemetry_sink, path)?;
                        eprintln!("extractor: recording cassette to {path}");
                        Arc::new(LlmRouter::from_env_with(
                            leaf_sink,
                            move |inner, model_name| {
                                Arc::new(RecordingExtractor::new(
                                    inner,
                                    "anthropic",
                                    model_name,
                                    Arc::clone(&writer),
                                ))
                            },
                        ))
                    }
                    None => Arc::new(LlmRouter::from_env(Arc::clone(&telemetry_sink))),
                }
            }
            ResolvedExtractor::Http(url) => match &record_llm_path {
                Some(path) => {
                    let leaf_sink = recording_sink(&telemetry_sink, path)?;
                    let ext = OaiExtractor::new_http(url, extractor_model, leaf_sink);
                    let (transport_label, endpoint) = ext.transport_info();
                    let model_name = ext.model_name().to_string();
                    eprintln!(
                            "extractor: provider=local, transport={transport_label}, endpoint={endpoint}"
                        );
                    let writer = Arc::new(CassetteWriter::open(path)?);
                    eprintln!("extractor: recording cassette to {path}");
                    Arc::new(RecordingExtractor::new(
                        Arc::new(ext),
                        "local",
                        model_name,
                        writer,
                    ))
                }
                None => {
                    let ext =
                        OaiExtractor::new_http(url, extractor_model, Arc::clone(&telemetry_sink));
                    let (transport_label, endpoint) = ext.transport_info();
                    eprintln!(
                            "extractor: provider=local, transport={transport_label}, endpoint={endpoint}"
                        );
                    Arc::new(ext)
                }
            },
            #[cfg(unix)]
            ResolvedExtractor::Uds(uds_path) => match &record_llm_path {
                Some(path) => {
                    let leaf_sink = recording_sink(&telemetry_sink, path)?;
                    let ext = OaiExtractor::new_uds(uds_path, extractor_model, leaf_sink);
                    let (transport_label, endpoint) = ext.transport_info();
                    let model_name = ext.model_name().to_string();
                    eprintln!(
                            "extractor: provider=local, transport={transport_label}, endpoint={endpoint}"
                        );
                    let writer = Arc::new(CassetteWriter::open(path)?);
                    eprintln!("extractor: recording cassette to {path}");
                    Arc::new(RecordingExtractor::new(
                        Arc::new(ext),
                        "local",
                        model_name,
                        writer,
                    ))
                }
                None => {
                    let ext = OaiExtractor::new_uds(
                        uds_path,
                        extractor_model,
                        Arc::clone(&telemetry_sink),
                    );
                    let (transport_label, endpoint) = ext.transport_info();
                    eprintln!(
                            "extractor: provider=local, transport={transport_label}, endpoint={endpoint}"
                        );
                    Arc::new(ext)
                }
            },
            // #331: no provider configured — startup proceeds; every extraction-dependent
            // method fails clearly at call time via `UnconfiguredExtractor`. Not wrapped with
            // `RecordingExtractor` even if `LCG_RECORD_LLM` is set: there is no live call to
            // record, and recording a stub that never dials out is meaningless.
            ResolvedExtractor::Unconfigured => {
                eprintln!("extractor: provider=none, extraction calls will fail until configured");
                if record_llm_path.is_some() {
                    eprintln!(
                        "extractor: LCG_RECORD_LLM is set but ignored — no extraction provider \
                         is configured, so there is nothing to record"
                    );
                }
                Arc::new(UnconfiguredExtractor)
            }
        }
    };

    // Derive wal_dir using the same env-var logic as AppState::from_env.
    // Available before DB open so startup recovery can use it without AppState.
    let startup_wal_dir = std::path::PathBuf::from(
        lcg_env_var("LCG_WAL_DIR", "GRAPHITI_WAL_DIR").unwrap_or_else(|_| ".lcg/wal".to_string()),
    );

    // Attempt to open database and initialize schema. Classify errors:
    //   - Recoverable (lbug WAL corruption, permission denied, missing file) → autonomous
    //     startup self-recovery first; degraded mode only if recovery itself fails.
    //   - Fatal (everything else) → propagate via ? and let the process exit
    let (maybe_db, degraded_reason, indices_ready): (Option<Arc<Db>>, Option<String>, bool) = {
        let open_result = (|| -> Result<Db, Box<dyn std::error::Error>> {
            let db = Db::open(&db_path)?;
            {
                let conn = db.connect()?;
                conn.init_schema(embedding_dim)?;
                // Eager build (FR-001): build HNSW/FTS indices immediately after schema init,
                // before the socket accepts any request, so ingest never has to discover a
                // missing entity_name_embedding_idx mid-chunk (#208). Idempotent/cheap when
                // indices already exist (create_vector_indexes swallows "already exists");
                // a genuine build failure propagates fatally via `?` (FR-004).
                conn.build_indices_and_constraints()?;
                // Populate the in-process name-lookup index (issue #219) from a full scan
                // before the socket accepts requests — mirrors the eager index-build posture
                // above; a genuine failure propagates fatally via `?`.
                conn.rebuild_name_index()?;
                // Backfill the applied-WAL-seq position (issue #353, FR-007) once at startup,
                // before the socket accepts requests. No-op if a position is already recorded
                // (every boot after the first). Non-fatal: a missed backfill just leaves
                // knowledge_status reporting null (safe — the documented action is a full
                // rebuild), not a reason to fail startup.
                if let Err(e) = lcg_core::recovery::backfill_applied_seq_if_absent(
                    &conn,
                    &startup_wal_dir,
                ) {
                    eprintln!(
                        "liminis-context-graph: startup: applied_seq backfill failed (non-fatal): {e}"
                    );
                }
            }
            Ok(db)
        })();

        match open_result {
            Ok(db) => (Some(Arc::new(db)), None, true),
            Err(e) => {
                let msg = e.to_string();
                // lbug raises two distinct messages for a corrupted WAL depending on which
                // check trips: an invalid record type ("Corrupted wal file. Read out invalid
                // WAL record type.", wal_record.cpp) or a checksum mismatch ("Checksum
                // verification failed, the WAL file is corrupted.", wal_replayer.cpp — the
                // shape produced by a torn/garbage WAL tail, e.g. a crash mid-write). Both are
                // the same recoverable condition ADR-0009's self-recovery sequence handles.
                let is_recoverable = msg.contains("Corrupted wal file")
                    || msg.contains("the WAL file is corrupted")
                    || msg.contains("Permission denied")
                    || msg.contains("No such file or directory");

                if is_recoverable {
                    // Attempt autonomous self-recovery before entering degraded mode (FR-001).
                    let recovery_db_path = db_path.clone();
                    let recovery_wal_dir = startup_wal_dir.clone();
                    let recovery_sink = Arc::clone(&telemetry_sink);
                    let recovery_result = tokio::task::spawn_blocking(move || {
                        lcg_core::recovery::run_full_recovery_sequence(
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
                            // run_full_recovery_sequence already calls
                            // build_indices_and_constraints (recovery.rs) and propagates a
                            // genuine build failure fatally, so success here means indices are
                            // built (FR-002).
                            (Some(Arc::new(db)), None, true)
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
                            (None, Some(reason), false)
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
                            (None, Some(reason), false)
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
        extractor,
    ));
    // FR-008: reflect the eager build performed above (direct-open or post-recovery) so
    // knowledge_status reports indices_built: true before the socket accepts any request,
    // matching the flag's existing meaning for the search-handler/dedup-path auto-heal builds.
    state.indices_built.store(indices_ready, Ordering::Release);
    Ok(state)
}

/// Runs the Unix-socket JSON-RPC service (the pre-existing, default behavior). `listener` is
/// already bound by the caller — binding happens before DB open so `health_check`/recovery IPC
/// work even in degraded mode (ADR-0009).
async fn run_socket_service(
    telemetry_sink: Arc<dyn TelemetrySink>,
    sink_drain_handle: tokio::task::JoinHandle<()>,
    state: Arc<AppState>,
    listener: UnixListener,
    shutdown_timeout_ms: u64,
) -> Result<(), Box<dyn std::error::Error>> {
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
            eprintln!(
                "liminis-context-graph: received SIGTERM, shutting down (sender pid={})",
                sigterm_diag::sender_pid_display()
            );
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
    // drops at the end of main() — guaranteed before process exit (see ADR-0017).
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

/// Runs standalone MCP-over-stdio mode (FR-001/FR-006): this process opened the DB itself via
/// `bootstrap_app_state`, exactly like the socket service. `tools/call` is routed in-process
/// through `handlers::dispatch` — no socket is bound.
async fn run_mcp_standalone(
    telemetry_sink: Arc<dyn TelemetrySink>,
    sink_drain_handle: tokio::task::JoinHandle<()>,
    state: Arc<AppState>,
    scopes: Vec<mcp::scope::Scope>,
    allow_remote_close: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("liminis-context-graph: MCP-over-stdio (standalone), scope={scopes:?}");

    // Cancelled after a successful knowledge_close call so the serve loop unwinds and this
    // function can run the same cancel/drain/drop sequence the socket service's tail uses
    // (R2/R4/R5/R6), rather than std::process::exit.
    let shutdown_ct = CancellationToken::new();
    let backend = mcp::backend::StandaloneBackend::new(Arc::clone(&state));
    let server = mcp::server::LcgMcpServer::new(
        backend,
        scopes,
        allow_remote_close,
        Some(shutdown_ct.clone()),
    );

    // SIGTERM/SIGINT parity with run_socket_service's signal handling (R1): without this, an
    // external kill/supervisor stop would hit the default disposition and skip the WAL
    // checkpoint below entirely, risking corruption on the next open. `stdin` EOF is already
    // handled structurally — rmcp's serve loop treats it as QuitReason::Closed on its own.
    #[cfg(unix)]
    {
        let mut sigterm_stream = signal(SignalKind::terminate())?;
        let ct = shutdown_ct.clone();
        tokio::spawn(async move {
            sigterm_stream.recv().await;
            eprintln!(
                "liminis-context-graph: received SIGTERM, shutting down (sender pid={})",
                sigterm_diag::sender_pid_display()
            );
            ct.cancel();
        });
    }
    {
        let ct = shutdown_ct.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("liminis-context-graph: received SIGINT, shutting down");
            ct.cancel();
        });
    }

    let running = server
        .serve_with_ct(rmcp::transport::stdio(), shutdown_ct)
        .await?;
    running.waiting().await?;

    // ── Graceful shutdown (mirrors run_socket_service's tail, minus the connection JoinSet —
    // rmcp's serve loop above is already fully stopped by the time waiting() returns) ────────
    state.cancel_token.cancel();
    telemetry_sink.emit(TelemetryEvent::ServiceState {
        ts_ms: now_ms(),
        state: "shutting_down".to_string(),
        reason: None,
        detail: None,
    });

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
            let _ = handle.await;
        }
    }

    let cancelled_chunks = Arc::clone(&state.cancelled_chunks);
    drop(state);
    let cancelled = cancelled_chunks.load(std::sync::atomic::Ordering::Relaxed) as u64;
    telemetry_sink.emit(TelemetryEvent::ServiceState {
        ts_ms: now_ms(),
        state: "stopped".to_string(),
        reason: None,
        detail: Some(serde_json::json!({"drained": 0, "cancelled": cancelled})),
    });

    drop(telemetry_sink);
    sink_drain_handle.await.ok();

    // Do NOT call `std::process::exit` here — see ADR-0017: doing so before the tokio runtime
    // has drained its blocking pool can race ahead of a spawn_blocking task (e.g. an in-flight
    // knowledge_build_indices call) that still holds an `Arc<Db>` clone, skipping the WAL
    // checkpoint and corrupting the database. `main()` bounds the runtime's blocking-pool drain
    // with `Runtime::shutdown_timeout` instead of an unconditional indefinite wait or an
    // immediate `exit` — see ADR-0035 for why standalone MCP mode needs that bound at all
    // (rmcp's stdio transport's stdin reader thread cannot be cancelled and only unblocks on
    // EOF or process exit).
    Ok(())
}

/// Runs attached MCP-over-stdio mode (FR-006): this process never touches the workspace
/// filesystem or opens the DB. Every `tools/call` is forwarded as JSON-RPC over `socket_path`
/// to an already-running service, so it never contends for lbug's single-writer lock (SC-002).
async fn run_mcp_attached(
    socket_path: String,
    scopes: Vec<mcp::scope::Scope>,
    allow_remote_close: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "liminis-context-graph: MCP-over-stdio (attached to {socket_path}), scope={scopes:?}"
    );
    let backend = mcp::attached::AttachedBackend::connect(&socket_path).await?;
    let server = mcp::server::LcgMcpServer::new(backend, scopes, allow_remote_close, None);

    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// The real async entry point. Split out from `main()` (which is NOT `#[tokio::main]`, see
/// there for why) so `shutdown_timeout_ms` can be read once, before the runtime is built, and
/// reused both for the in-body join_set/task drain and for the runtime's own blocking-pool
/// drain bound after this function returns.
async fn async_main(
    shutdown_timeout_ms: u64,
    cli_mode: CliMode,
) -> Result<(), Box<dyn std::error::Error>> {
    // Sink is created first so migration events are captured before any other work.
    // TODO: LIMINIS_TELEMETRY_SOCKET — wire SocketSink here if env var is set
    let (stderr_sink, sink_drain_handle) = sink::StderrSink::start();
    let telemetry_sink: Arc<dyn lcg_core::TelemetrySink> = stderr_sink;

    // Attached MCP mode never touches the workspace filesystem or migration state: it only
    // forwards calls over the given socket to a service that already owns the DB (FR-006).
    // Matches against `&cli_mode` (not by value) so Socket/standalone-Mcp can still move
    // `cli_mode` into the final `match` below when this arm doesn't apply.
    if let CliMode::Mcp {
        connect: Some(socket_path),
        scopes,
        allow_remote_close,
        ..
    } = &cli_mode
    {
        let socket_path = socket_path.clone();
        let scopes = scopes.clone();
        let allow_remote_close = *allow_remote_close;
        drop(sink_drain_handle); // attached mode emits no telemetry events; nothing to drain
        return run_mcp_attached(socket_path, scopes, allow_remote_close).await;
    }

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

    // Ensure DB parent directory exists (socket parent is created only in socket mode, below).
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    match cli_mode {
        CliMode::Socket {
            embedder,
            extractor,
        } => {
            // Bind socket FIRST — this allows health_check and recovery IPC to work even
            // when the DB is in a degraded state. See ADR-0009.
            if let Some(parent) = std::path::Path::new(&socket_path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::remove_file(&socket_path);
            let listener = UnixListener::bind(&socket_path)?;
            eprintln!("liminis-context-graph: listening on {socket_path}");

            let state = bootstrap_app_state(
                Arc::clone(&telemetry_sink),
                pre_migration_degraded,
                db_path,
                embedder,
                extractor,
            )
            .await?;

            run_socket_service(
                telemetry_sink,
                sink_drain_handle,
                state,
                listener,
                shutdown_timeout_ms,
            )
            .await
        }
        CliMode::Mcp {
            embedder,
            extractor,
            connect: None,
            scopes,
            allow_remote_close,
        } => {
            let state = bootstrap_app_state(
                Arc::clone(&telemetry_sink),
                pre_migration_degraded,
                db_path,
                embedder,
                extractor,
            )
            .await?;

            run_mcp_standalone(
                telemetry_sink,
                sink_drain_handle,
                state,
                scopes,
                allow_remote_close,
            )
            .await
        }
        CliMode::Mcp {
            connect: Some(_), ..
        } => unreachable!("attached mode already returned earlier in main()"),
        CliMode::Help | CliMode::Version => {
            unreachable!("--help/--version handled in main() before the runtime is built")
        }
    }
}

/// Not `#[tokio::main]`: that macro's generated wrapper drops the `Runtime` via its default
/// (unbounded) `Drop` impl, which waits indefinitely for every blocking-pool thread — including
/// `rmcp`'s stdio transport's stdin reader, an uncancellable blocking `read()` that only
/// unblocks on EOF or process exit (see ADR-0035). Owning the `Runtime` here lets us bound that
/// wait with `shutdown_timeout` instead: long enough for any legitimate in-flight
/// `spawn_blocking` work (e.g. an index-build call's `Arc<Db>` clone, per ADR-0017) to finish
/// and fire its WAL checkpoint, but not indefinite.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Read once, before the runtime exists, so both the async body's internal task drain and
    // the runtime's own post-`block_on` blocking-pool drain use the same bound.
    let shutdown_timeout_ms: u64 = std::env::var("LCG_SHUTDOWN_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5_000);

    // Parse argv before building the runtime so --help/--version (and argument errors) exit
    // immediately without spinning up tokio or touching the workspace.
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let cli_mode = match cli::parse_args(&raw_args) {
        Ok(CliMode::Help) => {
            print!("{}", cli::usage());
            return Ok(());
        }
        Ok(CliMode::Version) => {
            println!("liminis-context-graph {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Ok(m) => m,
        Err(e) => {
            // Print the friendly message + usage ourselves and exit non-zero. Returning `Err`
            // here would make the default handler *also* Debug-print the string with escaped
            // newlines — an ugly second copy. exit(2) is the conventional usage-error code.
            eprintln!("liminis-context-graph: {e}");
            std::process::exit(2);
        }
    };

    // Registered once, before the runtime is built, so the sender-PID atomic it populates is
    // process-wide state visible to both run_socket_service and run_mcp_standalone (#247). This
    // is purely observe-only and runs alongside — never in place of — tokio's own SIGTERM
    // handling registered inside each of those functions.
    #[cfg(unix)]
    if let Err(e) = sigterm_diag::register() {
        eprintln!(
            "liminis-context-graph: failed to register SIGTERM sender-PID diagnostic handler: {e}"
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async_main(shutdown_timeout_ms, cli_mode));
    runtime.shutdown_timeout(Duration::from_millis(shutdown_timeout_ms));
    result
}
