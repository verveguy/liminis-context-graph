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
    stream: Mutex<BufReader<UnixStream>>,
    next_id: AtomicU64,
    call_timeout: Duration,
}

impl AttachedBackend {
    /// Connects once at startup. Fails fast (not hang) if the socket is missing or has no
    /// listener — `UnixStream::connect` returns immediately in both cases (ENOENT/ECONNREFUSED).
    pub async fn connect(socket_path: &str) -> Result<Self, String> {
        let stream = UnixStream::connect(socket_path).await.map_err(|e| {
            format!(
                "failed to connect to attached service at '{socket_path}': {e}. \
                 Ensure a liminis-context-graph socket service is running at this path."
            )
        })?;
        let call_timeout_ms: u64 = std::env::var("LCG_ATTACHED_CALL_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_ATTACHED_CALL_TIMEOUT_MS);
        Ok(Self {
            stream: Mutex::new(BufReader::new(stream)),
            next_id: AtomicU64::new(1),
            call_timeout: Duration::from_millis(call_timeout_ms),
        })
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

        let mut guard = self.stream.lock().await;

        if let Err(e) = guard.write_all(format!("{line}\n").as_bytes()).await {
            return IpcResponse::err(
                Value::Null,
                -32000,
                format!("attached socket write failed: {e}"),
            );
        }
        if let Err(e) = guard.flush().await {
            return IpcResponse::err(
                Value::Null,
                -32000,
                format!("attached socket flush failed: {e}"),
            );
        }

        loop {
            let mut buf = String::new();
            let read_result =
                match tokio::time::timeout(self.call_timeout, guard.read_line(&mut buf)).await {
                    Ok(result) => result,
                    Err(_) => {
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
                    return parse_ipc_response(&value);
                }
                Err(e) => {
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
