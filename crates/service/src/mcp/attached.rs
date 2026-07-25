//! Attached-mode `McpBackend` (FR-006): forwards each `tools/call` as JSON-RPC over a Unix
//! socket to an already-running `liminis-context-graph` service, so this process never opens
//! the `.lcg` database itself and cannot contend for lbug's single-writer lock (SC-002).
//!
//! No async Rust client for the socket wire protocol existed before this — this is new
//! protocol-client code, not a port of anything. Calls are serialized on one persistent
//! connection (`tokio::sync::Mutex`-guarded): the wire protocol has no request-ID demuxing
//! for interleaved progress/response lines (see `crates/service/src/main.rs`'s
//! `handle_connection`), so with only one call ever in flight on this connection, any
//! interleaved `{"type":"progress"}` line unambiguously belongs to the current call.
//!
//! ## Reconnect and retry (issue #213)
//!
//! The connection can go dead at any time (remote restart, crash, deploy). Reconnect logic
//! lives entirely inside the single held `Mutex` guard — it is never dropped and re-acquired
//! mid-reconnect — so the "one call in flight at a time" invariant above holds by construction
//! even across a redial (FR-012).
//!
//! The retry boundary is safety-driven (see ADR-0040): a failure while *writing* the request
//! (`write_all`/`flush`) means the request provably never reached the remote, so it is safe to
//! redial and retry that write exactly once (FR-007/FR-008). A failure that surfaces only
//! *after* the request was fully written — while waiting for or reading the response — means
//! the remote's execution status is unknown, so that call is never retried; it fails with a
//! descriptive error and the connection is invalidated so the *next* call redials fresh rather
//! than reusing a stream in an unknown framing state (FR-010).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lcg_core::IpcResponse;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc::UnboundedSender, Mutex},
};

use crate::mcp::backend::McpBackend;

/// Default idle-read timeout for a single line off the attached socket (Copilot review finding
/// on PR #196): if the remote service hangs mid-call (e.g. crashes partway through a streaming
/// `knowledge_rebuild_from_wal`), the previous unbounded `read_line` loop would hold the
/// connection `Mutex` forever, permanently blocking every subsequent MCP call on this process.
/// Applied per-`read_line` (not to the call as a whole) so a legitimately long-running streaming
/// call that keeps emitting progress lines never trips it — only genuine silence does.
const DEFAULT_ATTACHED_CALL_TIMEOUT_MS: u64 = 30_000;

pub struct AttachedBackend {
    /// `None` means the connection is known-dead; the next call redials lazily before use.
    stream: Mutex<Option<BufReader<UnixStream>>>,
    socket_path: String,
    next_id: AtomicU64,
    call_timeout: Duration,
}

impl AttachedBackend {
    /// Connects once at startup. Fails fast (not hang) if the socket is missing or has no
    /// listener — `UnixStream::connect` returns immediately in both cases (ENOENT/ECONNREFUSED).
    pub async fn connect(socket_path: &str) -> Result<Self, String> {
        let stream = Self::dial(socket_path).await?;
        let call_timeout_ms: u64 = std::env::var("LCG_ATTACHED_CALL_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_ATTACHED_CALL_TIMEOUT_MS);
        Ok(Self {
            stream: Mutex::new(Some(stream)),
            socket_path: socket_path.to_string(),
            next_id: AtomicU64::new(1),
            call_timeout: Duration::from_millis(call_timeout_ms),
        })
    }

    /// Dials a fresh connection to `socket_path`. Used both by `connect()` (startup) and by
    /// `call()`'s reconnect paths (FR-008/FR-010).
    async fn dial(socket_path: &str) -> Result<BufReader<UnixStream>, String> {
        let stream = UnixStream::connect(socket_path).await.map_err(|e| {
            format!(
                "failed to connect to attached service at '{socket_path}': {e}. \
                 Ensure a liminis-context-graph socket service is running at this path."
            )
        })?;
        Ok(BufReader::new(stream))
    }

    /// Writes and flushes one request line. A failure here means the request is provably
    /// incomplete or unsent from this process's perspective (FR-007) — the flush-failure case
    /// is deliberately not distinguished from write_all failure, since a flush that fails after
    /// a successful write_all leaves genuine ambiguity about how many bytes reached the kernel,
    /// and both are treated as "safe to retry" per A1's conservative write-time boundary.
    async fn write_request(stream: &mut BufReader<UnixStream>, line: &str) -> Result<(), String> {
        stream
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("attached socket write failed: {e}"))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("attached socket flush failed: {e}"))?;
        Ok(())
    }
}

/// Builds an `IpcResponse` from a raw JSON-RPC response `Value` read off the socket.
/// `IpcResponse` only derives `Serialize` (it's the socket server's *outgoing* type), so the
/// attached client's incoming responses are parsed into it manually here.
fn parse_ipc_response(value: &Value) -> IpcResponse {
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    if let Some(err) = value.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000) as i32;
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error from attached service")
            .to_string();
        match err.get("data").cloned() {
            Some(data) => IpcResponse::err_with_data(id, code, message, data),
            None => IpcResponse::err(id, code, message),
        }
    } else {
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        IpcResponse::ok(id, result)
    }
}

impl McpBackend for AttachedBackend {
    async fn call(
        &self,
        method: &str,
        mut params: Value,
        progress: Option<UnboundedSender<Value>>,
    ) -> IpcResponse {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        if progress.is_some() {
            match params {
                Value::Object(ref mut map) => {
                    map.entry("_progress_token").or_insert(Value::Bool(true));
                }
                Value::Null => {
                    params = json!({"_progress_token": true});
                }
                _ => {}
            }
        }

        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let line = match serde_json::to_string(&request) {
            Ok(s) => s,
            Err(e) => {
                return IpcResponse::err(
                    Value::Null,
                    -32000,
                    format!("failed to serialize request for attached service: {e}"),
                );
            }
        };

        // Held for the entire call, including any reconnect — FR-012 forbids a race between a
        // reconnecting call and a subsequent call, and never dropping this guard mid-reconnect
        // satisfies that by construction.
        let mut guard = self.stream.lock().await;

        // Lazy redial: a prior call may have invalidated the connection (FR-010) or this is
        // the first use after a `None` left by a failed retry (FR-009).
        if guard.is_none() {
            match Self::dial(&self.socket_path).await {
                Ok(stream) => *guard = Some(stream),
                Err(e) => return IpcResponse::err(Value::Null, -32000, e),
            }
        }

        // Write phase (FR-007): on failure, redial and retry the write exactly once (FR-008).
        if let Err(write_err) =
            Self::write_request(guard.as_mut().expect("just ensured Some"), &line).await
        {
            match Self::dial(&self.socket_path).await {
                Ok(new_stream) => {
                    *guard = Some(new_stream);
                    if let Err(retry_err) =
                        Self::write_request(guard.as_mut().expect("just dialed"), &line).await
                    {
                        *guard = None;
                        return IpcResponse::err(
                            Value::Null,
                            -32000,
                            format!(
                                "attached socket write failed ({write_err}); reconnected but \
                                 retry write also failed ({retry_err})"
                            ),
                        );
                    }
                }
                Err(dial_err) => {
                    *guard = None;
                    return IpcResponse::err(
                        Value::Null,
                        -32000,
                        format!(
                            "attached socket write failed ({write_err}); reconnect attempt \
                             failed ({dial_err})"
                        ),
                    );
                }
            }
        }

        // Read phase: any failure from here on means the request may already have reached the
        // remote, so this call is never retried (FR-010) — but the connection is always
        // invalidated on failure so the *next* call redials fresh instead of reusing a stream
        // whose byte-framing is now suspect.
        loop {
            let mut buf = String::new();
            let read_result = match tokio::time::timeout(
                self.call_timeout,
                guard
                    .as_mut()
                    .expect("connection established above")
                    .read_line(&mut buf),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    *guard = None;
                    return IpcResponse::err(
                        Value::Null,
                        -32000,
                        format!(
                            "attached service call timed out after {}ms with no response \
                             (no data received — the remote service may have crashed or \
                             hung mid-call)",
                            self.call_timeout.as_millis()
                        ),
                    );
                }
            };
            match read_result {
                Ok(0) => {
                    *guard = None;
                    return IpcResponse::err(
                        Value::Null,
                        -32000,
                        "attached service closed the connection unexpectedly".to_string(),
                    );
                }
                Ok(_) => {
                    let trimmed = buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let value: Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(e) => {
                            *guard = None;
                            return IpcResponse::err(
                                Value::Null,
                                -32000,
                                format!("malformed response from attached service: {e}"),
                            );
                        }
                    };
                    if value.get("type").and_then(|t| t.as_str()) == Some("progress") {
                        if let Some(tx) = &progress {
                            let _ = tx.send(value);
                        }
                        continue;
                    }
                    // A terminal response whose id doesn't match this call's request id is a
                    // stale reply to an earlier (likely timed-out) call that only now made it
                    // off the wire — the timeout above already returned an error for that call
                    // and released the mutex, so this line must not be misdelivered as *this*
                    // call's result (Copilot review finding on PR #196). Discard and keep
                    // reading; the loop's per-read timeout re-arms each iteration, so a
                    // straggling stale reply doesn't erode this call's own timeout budget.
                    if value.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    return parse_ipc_response(&value);
                }
                Err(e) => {
                    *guard = None;
                    return IpcResponse::err(
                        Value::Null,
                        -32000,
                        format!("attached socket read failed: {e}"),
                    );
                }
            }
        }
    }

    fn is_attached(&self) -> bool {
        true
    }
}
