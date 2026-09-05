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
    embedder::{
        is_auth_error, is_transport_error, redact_url_userinfo, resolve_embedding_api_key,
        Embedder, OaiEmbedder, UnconfiguredEmbedder, EMBEDDER_UNREACHABLE_DEGRADED_REASON,
    },
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

/// A resolved embedder transport, ready to probe or to build the final embedder against once a
/// dimension is known. Shared between `resolve_and_probe_embedder` and `bootstrap_app_state`'s
/// post-probe embedder construction.
enum ResolvedTransport {
    Http(String),
    #[cfg(unix)]
    Uds(String),
}

/// Outcome of one attempt to resolve the embedder transport and probe it. Three-way rather than a
/// plain `Result` so callers (the bounded retry loop in `bootstrap_app_state`, issue #499 FR-002)
/// can distinguish "will never work" (`Fatal`, e.g. a malformed `--embedder-http` URL or a
/// rejected credential — never retried, in either launch mode) from "not reachable yet"
/// (`Retryable`, e.g. a UDS socket file that doesn't exist yet or a transport-classified probe
/// failure — retried only in standalone `--mcp-stdio` mode per FR-002/FR-004).
enum ProbeOutcome {
    Ready {
        resolved: ResolvedTransport,
        embedding_dim: usize,
        embedding_model_probed: String,
        embedding_api_key: Option<String>,
    },
    Fatal(String),
    Retryable(String),
}

/// Resolves the embedder transport (CLI flag > default UDS path > `LCG_EMBEDDING_URL` env >
/// error, FR-003/FR-004/FR-007) and runs the startup probe once. Re-checks transport resolution
/// (UDS socket existence, `LCG_EMBEDDING_URL`) on every call rather than caching it from a first
/// attempt: the socket file itself may not exist until the sidecar finishes binding, which is
/// plausibly the *more* common shape of the sidecar/lcg simultaneous-launch race (issue #499) than
/// an already-existing-but-not-yet-accepting socket — so the retry loop in `bootstrap_app_state`
/// must re-run this whole sequence, not just re-probe an already-resolved transport.
async fn resolve_and_probe_embedder(
    cli_uds: Option<&str>,
    cli_http: Option<&str>,
    embedder_model: &str,
    embedding_dim_override: Option<usize>,
) -> ProbeOutcome {
    const DEFAULT_UDS_PATH: &str = "/tmp/liminis-inference.sock";

    let resolved = if let Some(uds_path) = cli_uds {
        // FR-010: validate socket exists at startup
        #[cfg(unix)]
        {
            if !std::path::Path::new(uds_path).exists() {
                return ProbeOutcome::Retryable(format!(
                    "UDS socket not found at {uds_path}. \
                     Ensure the liminis-inference sidecar is running."
                ));
            }
            ResolvedTransport::Uds(uds_path.to_string())
        }
        #[cfg(not(unix))]
        {
            return ProbeOutcome::Fatal(
                "--embedder-uds is only supported on Unix platforms".to_string(),
            );
        }
    } else if let Some(http_url) = cli_http {
        // FR-011: validate URL format — must have a scheme and a non-empty host.
        let host_part = http_url
            .strip_prefix("https://")
            .or_else(|| http_url.strip_prefix("http://"));
        if host_part.map(|h| h.is_empty()).unwrap_or(true) {
            return ProbeOutcome::Fatal(format!(
                "Invalid --embedder-http URL: {http_url:?}. \
                 Must start with http:// or https:// and include a host."
            ));
        }
        ResolvedTransport::Http(http_url.to_string())
    } else {
        // No CLI flag — apply default resolution order
        #[cfg(unix)]
        if std::path::Path::new(DEFAULT_UDS_PATH).exists() {
            ResolvedTransport::Uds(DEFAULT_UDS_PATH.to_string())
        } else if let Ok(url) = lcg_env_var("LCG_EMBEDDING_URL", "GRAPHITI_EMBEDDING_URL") {
            ResolvedTransport::Http(url)
        } else {
            return ProbeOutcome::Retryable(format!(
                "No embedder configured: default UDS socket {DEFAULT_UDS_PATH} not found and \
                 LCG_EMBEDDING_URL is not set. Pass --embedder-uds or --embedder-http, or \
                 start the liminis-inference sidecar."
            ));
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

    // Resolved once (HTTP only — FR-005: UDS never performs key lookup or credential
    // attachment, so resolution is skipped entirely for that transport, which also avoids
    // firing GRAPHITI_EMBEDDING_API_KEY's deprecation notice for a variable that would have no
    // effect on a UDS-configured setup) and applied identically to both the probe and final
    // embedder constructions (issue #497) — a partial application would authenticate one but
    // not the other, surfacing as "probe succeeds, every real embed call 401s."
    let embedding_api_key = match &resolved {
        ResolvedTransport::Http(url) => resolve_embedding_api_key(url),
        #[cfg(unix)]
        ResolvedTransport::Uds(_) => None,
    };

    // ── Probe (before DB open) so a misconfigured embedder fails fast at startup rather than
    // on the first embed request (FR-010/FR-011) — or, in standalone --mcp-stdio mode, so the
    // caller's retry loop can distinguish "not up yet" from "never will be" (issue #499).
    let probe_embedder = match &resolved {
        ResolvedTransport::Http(url) => {
            match OaiEmbedder::new_http(url.clone(), embedder_model.to_string(), 1) {
                Ok(e) => e.with_api_key(embedding_api_key.clone()),
                Err(e) => return ProbeOutcome::Fatal(format!("invalid embedder config: {e}")),
            }
        }
        #[cfg(unix)]
        ResolvedTransport::Uds(path) => {
            match OaiEmbedder::new_uds(path.clone(), embedder_model.to_string(), 1) {
                Ok(e) => e,
                Err(e) => return ProbeOutcome::Fatal(format!("invalid embedder config: {e}")),
            }
        }
    };

    let (transport_label, endpoint) = probe_embedder.transport_info();
    // Redaction substring for the raw configured URL (FR-007) — transport_info() already
    // redacts `endpoint` above, but a wrapped reqwest::Error's Display can independently
    // echo the raw URL, so error-message sites below scrub the same substring separately.
    let url_scrub = if let ResolvedTransport::Http(url) = &resolved {
        redact_url_userinfo(url).1
    } else {
        None
    };
    let scrub_url = |msg: String| -> String {
        match &url_scrub {
            Some(userinfo) => msg.replace(userinfo.as_str(), ""),
            None => msg,
        }
    };

    match probe_embedder.probe().await {
        Ok((embedding_dim, embedding_model_probed)) => {
            eprintln!(
                "embedder: transport={transport_label}, endpoint={endpoint}, dim={embedding_dim}"
            );
            ProbeOutcome::Ready {
                resolved,
                embedding_dim,
                embedding_model_probed,
                embedding_api_key,
            }
        }
        Err(e) if is_transport_error(&e) => {
            // FR-011/#499 FR-002: a transport/connectivity failure is always fatal on the
            // socket-service path (FR-001); on standalone --mcp-stdio it's retried with bounded
            // backoff before the caller decides to degrade instead. LCG_EMBEDDING_DIM cannot
            // override an unreachable embedder either way.
            ProbeOutcome::Retryable(scrub_url(format!(
                "embedder unreachable at startup: {e}. \
                 Ensure the embedder sidecar is running before starting liminis-context-graph."
            )))
        }
        Err(e) if is_auth_error(&e) => {
            // FR-008: an authentication failure (401/403) is always fatal at startup and is
            // never bypassable via LCG_EMBEDDING_DIM — a dimension override cannot paper over
            // a rejected credential. Not retried in either launch mode (issue #499).
            ProbeOutcome::Fatal(scrub_url(format!(
                "embedder authentication failed at startup: {e}. \
                 Check LCG_EMBEDDING_API_KEY (or GRAPHITI_EMBEDDING_API_KEY / OPENAI_API_KEY) \
                 against the configured embedder endpoint's expected credential."
            )))
        }
        Err(e) => {
            // Non-transport, non-auth probe failure (e.g., unexpected response shape). Not
            // retried (issue #499 scope is limited to transport-classified failures).
            // LCG_EMBEDDING_DIM can override this per FR-008.
            if let Some(dim) = embedding_dim_override {
                eprintln!(
                    "{}",
                    scrub_url(format!(
                        "liminis-context-graph: embedder probe failed ({e}), \
                         using LCG_EMBEDDING_DIM={dim} override"
                    ))
                );
                ProbeOutcome::Ready {
                    resolved,
                    embedding_dim: dim,
                    embedding_model_probed: embedder_model.to_string(),
                    embedding_api_key,
                }
            } else {
                ProbeOutcome::Fatal(scrub_url(format!(
                    "embedder probe failed and LCG_EMBEDDING_DIM is not set: {e}"
                )))
            }
        }
    }
}

/// Bounded retry ceiling for the embedder probe in standalone `--mcp-stdio` mode (issue #499
/// FR-002/SC-003): long enough to absorb typical sidecar process-spawn + socket-bind timing
/// (sub-second to low-single-digit seconds), short enough to stay well under typical MCP client
/// initialize timeouts (commonly 10s+). Not applied on the socket-service path at all (FR-001) —
/// see `bootstrap_app_state`'s `allow_embedder_degrade` parameter.
const EMBEDDER_RETRY_CEILING: Duration = Duration::from_secs(5);
const EMBEDDER_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const EMBEDDER_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(1);

/// Resolves the embedder transport, probes it, opens the DB (with startup self-recovery per
/// ADR-0009), and builds `AppState`. Shared by the socket service and standalone MCP mode
/// (`--mcp-stdio` without `--connect`) so both reuse byte-for-byte the same bootstrap path —
/// attached MCP mode (`--mcp-stdio --connect <path>`) never calls this at all, since it forwards
/// every call to an already-running service instead of opening the DB itself (FR-006).
///
/// `allow_embedder_degrade` distinguishes the two launch modes for issue #499's FR-002/FR-003:
/// `false` (socket-service, `CliMode::Socket`) preserves today's fail-fast behavior byte-for-byte
/// for the classified-failure case — the very first transport-resolution-or-probe failure returns
/// `Err` immediately, zero retries (FR-001). It is *not* fully unchanged, though: the per-attempt
/// `tokio::time::timeout` below (bounding a connection that stalls after being accepted, since
/// neither transport client configures a request timeout of its own) applies uniformly to both
/// modes, so socket mode's single attempt is now also bounded by `EMBEDDER_RETRY_CEILING` — a
/// previously-unbounded hang on a stalled connection becomes a bounded failure instead. `true`
/// (standalone `--mcp-stdio`, `CliMode::Mcp { connect: None, .. }`) retries a `Retryable` outcome
/// with bounded backoff (`EMBEDDER_RETRY_CEILING`), and if the retry window is exhausted, starts
/// in degraded mode without ever opening the database — see the early return below — rather than
/// exiting.
///
/// Precondition (#437): must only be called after `migration::migrate_workspace` has run for
/// the current workspace (see `async_main`) — this function's `migrate_wal_root_if_needed` call
/// below needs the configured WAL root (`startup_wal_root`) to already contain any legacy
/// content for a `.graphiti`-era workspace, or it silently no-ops and legacy WAL content is left
/// loose and invisible. As of #442, `async_main` resolves that same WAL root and passes it to
/// `migrate_workspace`, so the two migrations always agree on one destination regardless of any
/// `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` override.
async fn bootstrap_app_state(
    telemetry_sink: Arc<dyn TelemetrySink>,
    pre_migration_degraded: Option<String>,
    db_path: String,
    embedder_flag: Option<EmbedderFlag>,
    extractor_flag: Option<ExtractorFlag>,
    allow_embedder_degrade: bool,
) -> Result<Arc<AppState>, Box<dyn std::error::Error>> {
    let (cli_uds, cli_http) = match embedder_flag {
        Some(EmbedderFlag::Uds(p)) => (Some(p), None),
        Some(EmbedderFlag::Http(u)) => (None, Some(u)),
        None => (None, None),
    };

    let embedder_model = lcg_env_var("LCG_EMBEDDING_MODEL", "GRAPHITI_EMBEDDING_MODEL")
        .unwrap_or_else(|_| "bge-base-en-v1.5".to_string());

    // Dim override — used as fallback if probe fails (FR-008)
    let embedding_dim_override: Option<usize> =
        lcg_env_var("LCG_EMBEDDING_DIM", "GRAPHITI_EMBEDDING_DIM")
            .ok()
            .and_then(|s| s.parse().ok());

    // ── Transport resolution + probe, with bounded retry in standalone --mcp-stdio mode
    // (issue #499 FR-002/FR-004) ────────────────────────────────────────────────────────
    // Socket mode (`allow_embedder_degrade == false`) hits `Retryable`'s `return Err(..)` arm on
    // the very first iteration, exactly as the pre-#499 single-shot code did (FR-001/SC-002) — the
    // loop only actually iterates more than once when standalone `--mcp-stdio` is retrying.
    let retry_deadline = std::time::Instant::now() + EMBEDDER_RETRY_CEILING;
    let mut backoff = EMBEDDER_RETRY_INITIAL_BACKOFF;
    let (resolved, embedding_dim, embedding_model_probed, embedding_api_key) = loop {
        // Bound each attempt to the time remaining in the window, not just the inter-attempt
        // backoff: neither transport's client (reqwest::Client::new() nor the UDS hyper pool)
        // has a request timeout configured, so a connection that stalls after being accepted —
        // rather than refusing outright — could otherwise block a single attempt past the
        // advertised ceiling (review finding on #499/PR #514). Applying this uniformly also
        // bounds the same failure mode on the socket-service path, where it can only shorten an
        // unbounded hang, never shorten today's fast-refusal fail-fast case FR-001 covers.
        let attempt_budget = retry_deadline
            .saturating_duration_since(std::time::Instant::now())
            .max(Duration::from_millis(1));
        let outcome = match tokio::time::timeout(
            attempt_budget,
            resolve_and_probe_embedder(
                cli_uds.as_deref(),
                cli_http.as_deref(),
                &embedder_model,
                embedding_dim_override,
            ),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => ProbeOutcome::Retryable(format!(
                "embedder probe timed out after {:.1}s with no response",
                attempt_budget.as_secs_f64()
            )),
        };
        match outcome {
            ProbeOutcome::Ready {
                resolved,
                embedding_dim,
                embedding_model_probed,
                embedding_api_key,
            } => {
                break (
                    resolved,
                    embedding_dim,
                    embedding_model_probed,
                    embedding_api_key,
                )
            }
            ProbeOutcome::Fatal(msg) => return Err(msg.into()),
            ProbeOutcome::Retryable(msg) => {
                if !allow_embedder_degrade {
                    return Err(msg.into());
                }
                let now = std::time::Instant::now();
                if now >= retry_deadline {
                    // FR-003: retry window exhausted in standalone --mcp-stdio mode — start
                    // degraded rather than exit. Never opens the DB (Costing finding 3: the
                    // embedding dimension is never known here), so this returns directly rather
                    // than falling through to the DB-open logic below.
                    eprintln!(
                        "liminis-context-graph: embedder still unreachable after {:.1}s of \
                         retries ({msg}) — starting in degraded mode (standalone --mcp-stdio); \
                         see knowledge_status for degraded_reason={EMBEDDER_UNREACHABLE_DEGRADED_REASON:?}",
                        EMBEDDER_RETRY_CEILING.as_secs_f64()
                    );
                    telemetry_sink.emit(TelemetryEvent::ServiceState {
                        ts_ms: now_ms(),
                        state: "degraded".to_string(),
                        reason: Some(EMBEDDER_UNREACHABLE_DEGRADED_REASON.to_string()),
                        detail: Some(serde_json::Value::String(msg)),
                    });
                    // migration_failed (if any) takes precedence, matching the DB-open degraded
                    // branch below's own precedence rule.
                    let degraded_reason = pre_migration_degraded
                        .or_else(|| Some(EMBEDDER_UNREACHABLE_DEGRADED_REASON.to_string()));
                    let embedding_cache = Arc::new(lcg_core::EmbeddingCache::new());
                    let state = Arc::new(AppState::from_env(
                        telemetry_sink,
                        None,
                        degraded_reason,
                        db_path,
                        Arc::new(UnconfiguredEmbedder),
                        String::new(),
                        Arc::new(UnconfiguredExtractor),
                        embedding_cache,
                    ));
                    state.indices_built.store(false, Ordering::Release);
                    return Ok(state);
                }
                tokio::time::sleep(backoff.min(retry_deadline.saturating_duration_since(now)))
                    .await;
                backoff = (backoff * 2).min(EMBEDDER_RETRY_MAX_BACKOFF);
            }
        }
    };

    // Build the final embedder with the correct probed dim
    let embedder: Arc<dyn Embedder> = match &resolved {
        ResolvedTransport::Http(url) => Arc::new(
            OaiEmbedder::new_http(url.clone(), embedding_model_probed.clone(), embedding_dim)?
                .with_api_key(embedding_api_key.clone()),
        ),
        #[cfg(unix)]
        ResolvedTransport::Uds(path) => Arc::new(OaiEmbedder::new_uds(
            path.clone(),
            embedding_model_probed.clone(),
            embedding_dim,
        )?),
    };

    // Content-addressed embedding cache (issue #440, FR-003) — constructed once, right after the
    // embedder is resolved/probed, and threaded into both the startup recovery call below and
    // AppState::from_env, so cache warmth survives the recovery → serving transition rather than
    // starting cold on the first post-recovery rebuild.
    let embedding_cache = Arc::new(lcg_core::EmbeddingCache::new());

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

    // Derive the WAL root using the same env-var logic as AppState::from_env (issue #378:
    // LCG_WAL_DIR now names a root containing one subdirectory per group_id, not a single
    // shared stream). Available before DB open so startup recovery can use it without AppState.
    let startup_wal_root = std::path::PathBuf::from(
        lcg_env_var("LCG_WAL_DIR", "GRAPHITI_WAL_DIR").unwrap_or_else(|_| ".lcg/wal".to_string()),
    );
    // #437: relies on `migration::migrate_workspace` (async_main, above) having already moved
    // any `.graphiti/wal` content into `startup_wal_root` before this runs — see this function's
    // doc comment. `migrate_wal_root_if_needed` is idempotent and no-ops cleanly on a missing or
    // already-migrated root, so re-running it here (it's also called from
    // `AppState::from_env`, below) is safe; it just must never run *before* that workspace move.
    //
    // #437 FR-008: we deliberately do NOT add a post-migration scan for stray loose WAL content
    // left over after both migrations run. Both migrations are idempotent and now correctly
    // ordered (see above), and the un-ignored `binary_migrates_legacy_workspace_on_startup`
    // regression test guards that ordering going forward — a "scan for strays and warn" step
    // would be defense-in-depth for a defect that no longer exists, not a fix for one that does.
    if let Err(e) = lcg_core::wal_group::migrate_wal_root_if_needed(&startup_wal_root) {
        eprintln!(
            "liminis-context-graph: wal root migration failed for {startup_wal_root:?} \
             (non-fatal to startup, but every per-group path below resolves under \
             <wal_root>/<group_id> regardless — any pre-378 loose top-level WAL content at \
             {startup_wal_root:?} stays on disk untouched but becomes invisible to this process \
             until migration succeeds; it is not read as a fallback): {e}"
        );
    }
    // Startup eagerly backfills/recovers only the default group (issue #378) — matching
    // FR-009's "single-group instance unchanged" scope; other groups are backfilled lazily on
    // first touch (e.g. via knowledge_status's per-group map).
    let startup_wal_dir =
        lcg_core::wal_group::group_wal_dir(&startup_wal_root, lcg_core::DEFAULT_GROUP_ID)
            .unwrap_or_else(|_| startup_wal_root.join(lcg_core::DEFAULT_GROUP_ID));

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
                // Carry a pre-378 database's WalPosition {id: 'singleton'} row forward to the
                // default group's own row (issue #378 FR-001/FR-009) *before* the backfill check
                // below decides whether a position is already known — otherwise an upgraded
                // binary would find no row under the new key, silently discard an
                // already-durably-recorded position, and needlessly (or, in the worst case,
                // unsuccessfully) re-derive it from a WAL scan. No-op after the first boot
                // (idempotent) or on a fresh install (no legacy row to migrate).
                if let Err(e) =
                    conn.migrate_legacy_singleton_wal_position(lcg_core::DEFAULT_GROUP_ID)
                {
                    eprintln!(
                        "liminis-context-graph: startup: legacy singleton WalPosition migration failed (non-fatal): {e}"
                    );
                }
                // Backfill the applied-WAL-seq position (issue #353, FR-007) once at startup,
                // before the socket accepts requests. No-op if a position is already recorded
                // (every boot after the first, or immediately after the migration above). Non-fatal: a
                // missed backfill just leaves knowledge_status reporting null (safe — the
                // documented action is a full rebuild), not a reason to fail startup.
                if let Err(e) = lcg_core::recovery::backfill_applied_seq_if_absent(
                    &conn,
                    lcg_core::DEFAULT_GROUP_ID,
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
                    // Pass the WAL root, not the default group's own subdirectory: the fallback
                    // full-rebuild path inside run_full_recovery_sequence wipes the entire
                    // embedded DB (every group's data), so it must be able to replay every
                    // group's WAL directory back in, not only the default group's (issue #378).
                    let recovery_db_path = db_path.clone();
                    let recovery_wal_root = startup_wal_root.clone();
                    let recovery_sink = Arc::clone(&telemetry_sink);
                    let recovery_embedder_ctx = lcg_core::EmbedderContext {
                        embedder: Arc::clone(&embedder),
                        model: embedding_model_probed.clone(),
                        cache: Arc::clone(&embedding_cache),
                    };
                    let recovery_result = tokio::task::spawn_blocking(move || {
                        lcg_core::recovery::run_full_recovery_sequence(
                            &recovery_db_path,
                            lcg_core::DEFAULT_GROUP_ID,
                            &recovery_wal_root,
                            embedding_dim,
                            recovery_sink,
                            recovery_embedder_ctx,
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
        embedding_cache,
    ));
    // FR-008: reflect the eager build performed above (direct-open or post-recovery) so
    // knowledge_status reports indices_built: true before the socket accepts any request,
    // matching the flag's existing meaning for the search-handler/dedup-path auto-heal builds.
    state.indices_built.store(indices_ready, Ordering::Release);
    Ok(state)
}

/// Races `bootstrap_app_state` against the shared shutdown signal (FR-005/ADR-0500): without
/// this, a SIGTERM/SIGINT arriving while blocked on an unreachable embedder/extractor probe (or
/// any other unbounded step inside `bootstrap_app_state`) would only be *recorded* by
/// `shutdown_ct` — installed at the very top of `async_main`, before this call — but not acted
/// on until that probe itself resolves, which is exactly the condition issue #500's leaked
/// processes reproduced. Returns `Ok(None)` if the shutdown signal wins the race.
///
/// Dropping the losing `bootstrap_app_state` future here is safe: any `Db`/`Arc<Db>` it had
/// opened locally at that point is released through the normal Drop path (the same
/// WAL-checkpoint-on-drop guarantee ADR-0017 already relies on for the equivalent `drop(state)`
/// calls in `run_socket_service`/`run_mcp_standalone`'s own shutdown tails), and its startup
/// self-recovery branch's `spawn_blocking` task, if already running, is not aborted by this — it
/// keeps running detached, bounded by `main()`'s own `runtime.shutdown_timeout`, exactly as it
/// already is for a normal shutdown.
async fn bootstrap_or_exit_on_signal(
    shutdown_ct: &CancellationToken,
    telemetry_sink: Arc<dyn TelemetrySink>,
    pre_migration_degraded: Option<String>,
    db_path: String,
    embedder_flag: Option<EmbedderFlag>,
    extractor_flag: Option<ExtractorFlag>,
    allow_embedder_degrade: bool,
) -> Result<Option<Arc<AppState>>, Box<dyn std::error::Error>> {
    tokio::select! {
        biased;
        _ = shutdown_ct.cancelled() => Ok(None),
        result = bootstrap_app_state(
            telemetry_sink,
            pre_migration_degraded,
            db_path,
            embedder_flag,
            extractor_flag,
            allow_embedder_degrade,
        ) => result.map(Some),
    }
}

/// Installs the process's one real SIGTERM/SIGINT handling, at the very top of `async_main` —
/// before `migrate_workspace`/`bootstrap_app_state` run. See ADR-0500: previously each of
/// `run_socket_service`/`run_mcp_standalone` registered its own handler, and `run_mcp_attached`
/// registered none at all — so a signal received any time before whichever of those functions
/// was eventually reached had nothing to observe it but `sigterm_diag`'s diagnostic,
/// sender-PID-only handler (installed in `main()`, before the tokio runtime even exists). Once a
/// handler is registered for a signal, the OS does not requeue or redeliver it later — so that
/// window silently and permanently swallowed the signal, leaking the process (issue #500). Every
/// caller now shares this one `shutdown_ct`, cancelled at most once, instead of each registering
/// its own.
fn install_shutdown_signal_handlers(shutdown_ct: CancellationToken) -> std::io::Result<()> {
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
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("liminis-context-graph: received SIGINT, shutting down");
            shutdown_ct.cancel();
        });
    }
    Ok(())
}

/// Runs the Unix-socket JSON-RPC service (the pre-existing, default behavior). `listener` is
/// already bound by the caller — binding happens before DB open so `health_check`/recovery IPC
/// work even in degraded mode (ADR-0009). `shutdown_ct` is the one shared signal source
/// installed at the top of `async_main` (see `install_shutdown_signal_handlers`) — this function
/// no longer registers its own handler.
async fn run_socket_service(
    telemetry_sink: Arc<dyn TelemetrySink>,
    sink_drain_handle: tokio::task::JoinHandle<()>,
    state: Arc<AppState>,
    listener: UnixListener,
    shutdown_timeout_ms: u64,
    shutdown_ct: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    // `shutdown_notify` is also notified directly by `handle_connection` on a
    // knowledge_close-initiated shutdown (R3) — forward the shared shutdown signal into it
    // rather than replacing it, so both triggers feed the same accept-loop break below.
    let shutdown_notify = Arc::new(Notify::new());
    {
        let notify = Arc::clone(&shutdown_notify);
        tokio::spawn(async move {
            shutdown_ct.cancelled().await;
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
    shutdown_ct: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("liminis-context-graph: MCP-over-stdio (standalone), scope={scopes:?}");

    // `shutdown_ct` is the one shared signal source installed at the top of `async_main` (see
    // `install_shutdown_signal_handlers`) — also cancelled after a successful knowledge_close
    // call, so the serve loop unwinds either way and this function runs the same
    // cancel/drain/drop sequence the socket service's tail uses (R2/R4/R5/R6), rather than
    // std::process::exit. `stdin` EOF is handled structurally — rmcp's serve loop treats it as
    // QuitReason::Closed on its own.
    let backend = mcp::backend::StandaloneBackend::new(Arc::clone(&state));
    let server = mcp::server::LcgMcpServer::new(
        backend,
        scopes,
        allow_remote_close,
        Some(shutdown_ct.clone()),
    );

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
    shutdown_ct: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "liminis-context-graph: MCP-over-stdio (attached to {socket_path}), scope={scopes:?}"
    );
    let backend = mcp::attached::AttachedBackend::connect(&socket_path).await?;
    let server = mcp::server::LcgMcpServer::new(backend, scopes, allow_remote_close, None);

    // `shutdown_ct` is the one shared signal source installed at the top of `async_main` (see
    // `install_shutdown_signal_handlers`). Previously this mode registered no signal handling at
    // all — the same architectural gap `run_socket_service`/`run_mcp_standalone` had, in a more
    // severe form (SIGTERM/SIGINT were never honored, not just during a startup window). Attached
    // mode holds no DB/WAL state of its own to checkpoint, so `serve_with_ct` closing the serve
    // loop on cancellation is sufficient — there is no cancel/drain/drop tail to run afterward.
    let running = server
        .serve_with_ct(rmcp::transport::stdio(), shutdown_ct)
        .await?;
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

    // Installed first, before any workspace/migration/bootstrap work below — see ADR-0500 and
    // `install_shutdown_signal_handlers`'s doc comment for why this ordering is load-bearing
    // (issue #500): everything from here to `run_socket_service`/`run_mcp_standalone`/
    // `run_mcp_attached` shares this one `shutdown_ct` instead of each registering its own
    // handler later.
    let shutdown_ct = CancellationToken::new();
    install_shutdown_signal_handlers(shutdown_ct.clone())?;

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
        return run_mcp_attached(socket_path, scopes, allow_remote_close, shutdown_ct).await;
    }

    // Structured workspace migration: .graphiti/ → .lcg/ with file-layout restructuring.
    // Runs before path resolution so deprecated GRAPHITI_* env-var paths can be rewritten
    // below, preventing create_dir_all from crashing on the legacy file-as-dir layout.
    //
    // #437: this MUST also run before `bootstrap_app_state` (called from every `CliMode` arm
    // further down in this function — do not trust specific line numbers here, they drift;
    // grep for `bootstrap_app_state(` call sites instead), because `bootstrap_app_state` is
    // what performs the per-group WAL-root migration (`wal_group::migrate_wal_root_if_needed`,
    // see the comment at its call site).
    // For a `.graphiti`-era workspace, `.lcg/wal` doesn't exist until this migration moves it
    // there — if the per-group migration ran first, it would no-op on the missing directory and
    // never get another chance, leaving the relocated WAL files loose forever. The dependency
    // is enforced only by this straight-line call order, not by the type system: don't extract a
    // "resolve paths" helper or otherwise move `bootstrap_app_state`'s call ahead of this one.
    //
    // #442: `wal_root` is resolved here, once, using the exact same env-var precedence/fallback
    // `bootstrap_app_state`'s `startup_wal_root` (and `AppState::from_env`'s `wal_root`) also
    // use, so `migrate_workspace`'s Step 4 destination and the subsequent per-group scan always
    // agree on one directory. It's resolved independently at each of those call sites (not
    // threaded further) — the env vars don't change during the process's lifetime, so identical
    // resolution logic at each site is sufficient to keep them in agreement.
    let resolved_wal_root = std::path::PathBuf::from(
        lcg_env_var("LCG_WAL_DIR", "GRAPHITI_WAL_DIR").unwrap_or_else(|_| ".lcg/wal".to_string()),
    );
    let (pre_migration_degraded, did_migrate) =
        match migration::migrate_workspace(Path::new("."), &resolved_wal_root, &*telemetry_sink) {
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

            // FR-001/SC-002: socket-service (hand-started) mode keeps today's fail-fast
            // behavior unchanged — an unreachable embedder is never retried or degraded here.
            let Some(state) = bootstrap_or_exit_on_signal(
                &shutdown_ct,
                Arc::clone(&telemetry_sink),
                pre_migration_degraded,
                db_path,
                embedder,
                extractor,
                false,
            )
            .await?
            else {
                eprintln!(
                    "liminis-context-graph: shutdown signal received during startup, exiting"
                );
                drop(telemetry_sink);
                sink_drain_handle.await.ok();
                return Ok(());
            };

            run_socket_service(
                telemetry_sink,
                sink_drain_handle,
                state,
                listener,
                shutdown_timeout_ms,
                shutdown_ct,
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
            // FR-002/FR-003: standalone --mcp-stdio mode retries an unreachable embedder with
            // bounded backoff and, if the window is exhausted, starts in degraded mode instead
            // of exiting (issue #499) — nobody is watching this process's stderr.
            let Some(state) = bootstrap_or_exit_on_signal(
                &shutdown_ct,
                Arc::clone(&telemetry_sink),
                pre_migration_degraded,
                db_path,
                embedder,
                extractor,
                true,
            )
            .await?
            else {
                eprintln!(
                    "liminis-context-graph: shutdown signal received during startup, exiting"
                );
                drop(telemetry_sink);
                sink_drain_handle.await.ok();
                return Ok(());
            };

            run_mcp_standalone(
                telemetry_sink,
                sink_drain_handle,
                state,
                scopes,
                allow_remote_close,
                shutdown_ct,
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
