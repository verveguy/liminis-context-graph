//! `McpBackend`: the seam between the MCP tool surface (`crate::mcp::server`) and how a call
//! actually executes. `StandaloneBackend` routes in-process through `lcg_core::handlers::dispatch`
//! (FR-006 standalone mode); `crate::mcp::attached::AttachedBackend` forwards over a Unix
//! socket (FR-006 attached mode). The `rmcp::ServerHandler` in `server.rs` is written once,
//! generic over this trait.

use std::sync::Arc;

use lcg_core::{app_state::AppState, handlers, ipc::IpcRequest, IpcResponse};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

/// Executes one `knowledge_*` (or `health_check`) call and returns the raw JSON-RPC response.
///
/// `progress` is `Some` when the caller attached an MCP progress token to a streaming method
/// (FR-007); implementations forward any bridged `{"type":"progress"}` events to it.
pub trait McpBackend: Send + Sync + 'static {
    fn call(
        &self,
        method: &str,
        params: Value,
        progress: Option<UnboundedSender<Value>>,
    ) -> impl std::future::Future<Output = IpcResponse> + Send;

    /// True for `AttachedBackend` (this process does not own the DB). Used by `server.rs` to
    /// decide whether `knowledge_close` is visible/callable per FR-005.
    fn is_attached(&self) -> bool;
}

/// Standalone mode (FR-006): this process opened the `.lcg` database itself, exactly as the
/// socket service does. Calls route straight through the shared core dispatch — no new graph
/// logic, per the issue's explicit "core dispatch untouched" scope.
pub struct StandaloneBackend {
    pub state: Arc<AppState>,
}

impl StandaloneBackend {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

impl McpBackend for StandaloneBackend {
    async fn call(
        &self,
        method: &str,
        params: Value,
        progress: Option<UnboundedSender<Value>>,
    ) -> IpcResponse {
        let req = IpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Value::Null,
            method: method.to_string(),
            params,
        };
        handlers::dispatch(req, Arc::clone(&self.state), progress).await
    }

    fn is_attached(&self) -> bool {
        false
    }
}
