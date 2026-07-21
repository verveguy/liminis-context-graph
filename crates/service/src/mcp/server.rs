//! Hand-rolled `rmcp::ServerHandler` (not the `#[tool_router]` macro): FR-004 needs
//! `tools/list` to vary at runtime by `--scope` and by attached-mode `--allow-remote-close`,
//! but the macro produces a static tool list fixed at compile time. Implementing
//! `list_tools`/`call_tool` directly against `mcp::tools::registry()` gives full control at
//! negligible cost, since none of the 33 handlers have typed argument structs anyway
//! (FR-003: arguments pass straight through to `handlers::dispatch` as a raw `Value`).

use std::sync::Arc;

use lcg_core::IpcResponse;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, ErrorCode, ErrorData as McpError, Implementation,
        ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ServerCapabilities,
        ServerInfo, Tool,
    },
    service::{NotificationContext, RequestContext, RoleServer},
    ServerHandler,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::mcp::{
    backend::McpBackend,
    scope::Scope,
    tools::{self, ToolSpec},
};

pub struct LcgMcpServer<B: McpBackend> {
    backend: B,
    scopes: Vec<Scope>,
    allow_remote_close: bool,
    /// Cancelled after a successful standalone `knowledge_close` call, to unwind the MCP
    /// serve loop and let `main()` run the same graceful-shutdown sequence used by the
    /// socket service. `None` in attached mode: forwarding a close to the remote service
    /// never exits this MCP process (see FR-005 / the issue's Assumptions).
    shutdown_ct: Option<CancellationToken>,
}

impl<B: McpBackend> LcgMcpServer<B> {
    pub fn new(
        backend: B,
        scopes: Vec<Scope>,
        allow_remote_close: bool,
        shutdown_ct: Option<CancellationToken>,
    ) -> Self {
        Self {
            backend,
            scopes,
            allow_remote_close,
            shutdown_ct,
        }
    }

    /// FR-004 scope gating + FR-005 attached-mode `knowledge_close` visibility.
    fn is_tool_visible(&self, spec: &ToolSpec) -> bool {
        if !self.scopes.contains(&spec.scope) {
            return false;
        }
        if spec.name == "knowledge_close" && self.backend.is_attached() && !self.allow_remote_close
        {
            return false;
        }
        true
    }

    fn find_visible(&self, name: &str) -> Option<ToolSpec> {
        tools::registry()
            .into_iter()
            .find(|t| t.name == name && self.is_tool_visible(t))
    }

    fn to_mcp_tool(spec: &ToolSpec) -> Tool {
        let schema = (spec.input_schema)();
        let schema_obj = match schema {
            Value::Object(map) => map,
            other => unreachable!("tool schema must be a JSON object, got {other:?}"),
        };
        Tool::new(spec.name, spec.description, Arc::new(schema_obj))
    }
}

fn unknown_tool_error(name: &str) -> McpError {
    McpError::new(
        ErrorCode::METHOD_NOT_FOUND,
        format!("Unknown tool: '{name}'"),
        None,
    )
}

/// FR-008: core JSON-RPC errors (parse errors, generic -32000, DB-unavailable -32001,
/// degraded-mode rejections) surface as well-formed *tool-level* MCP errors — i.e.
/// `Ok(CallToolResult::error(...))`, not `Err(McpError)` — so the calling client actually sees
/// the message, per rmcp's own guidance on the two MCP failure modes.
fn ipc_response_to_call_tool_result(resp: IpcResponse) -> CallToolResult {
    match resp {
        IpcResponse::Ok { result, .. } => CallToolResult::structured(result),
        IpcResponse::Err { error, .. } => {
            let mut payload = json!({"code": error.code, "message": error.message});
            if let Some(data) = error.data {
                payload["data"] = data;
            }
            CallToolResult::structured_error(payload)
        }
    }
}

impl<B: McpBackend> ServerHandler for LcgMcpServer<B> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "liminis-context-graph",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Native MCP-over-stdio transport for the liminis-context-graph knowledge graph. \
                 Tool visibility is restricted by --scope; see the README's MCP section for the \
                 cypher and admin scope footguns.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = tools::registry()
            .iter()
            .filter(|t| self.is_tool_visible(t))
            .map(Self::to_mcp_tool)
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.find_visible(name).as_ref().map(Self::to_mcp_tool)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let Some(spec) = self.find_visible(&request.name) else {
            return Err(unknown_tool_error(&request.name));
        };

        let params: Value = request
            .arguments
            .clone()
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));

        // NOTE: `request.progress_token()` (via `RequestParamsMeta`) is always `None` here —
        // rmcp's `Request<M, R>` deserialization extracts the wire-level `_meta` object into
        // the request's `extensions`/`context.meta` *before* handing `params` to `call_tool`,
        // so `CallToolRequestParams::meta` never gets populated for an incoming request (only
        // for one this server constructs itself). The progress token must be read off
        // `context.meta`, which is what actually carries the client's `_meta.progressToken`.
        let progress_token = context.meta.get_progress_token();
        let wants_progress = progress_token.is_some() && tools::is_streaming_method(spec.name);

        let response = if wants_progress {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
            let peer = context.peer.clone();
            let token = progress_token.expect("checked by wants_progress");
            let forward = tokio::spawn(async move {
                let mut progress = 0.0_f64;
                while let Some(val) = rx.recv().await {
                    progress += 1.0;
                    let mut param = ProgressNotificationParam::new(token.clone(), progress);
                    if let Some(message) = val.get("message").and_then(|m| m.as_str()) {
                        param = param.with_message(message.to_string());
                    }
                    let _ = peer.notify_progress(param).await;
                }
            });
            let response = self.backend.call(spec.name, params, Some(tx)).await;
            // Dropping the response's originating tx (already dropped inside backend.call)
            // closes the channel, so `forward` drains any remaining buffered events and exits.
            let _ = forward.await;
            response
        } else {
            self.backend.call(spec.name, params, None).await
        };

        if spec.name == "knowledge_close" {
            if let (IpcResponse::Ok { .. }, Some(ct)) = (&response, &self.shutdown_ct) {
                ct.cancel();
            }
        }

        Ok(ipc_response_to_call_tool_result(response))
    }

    async fn on_cancelled(
        &self,
        _notification: rmcp::model::CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) {
        // No requirement to make an in-flight operation cancellable (issue's Edge Cases):
        // the underlying handlers::dispatch call runs to completion regardless. This override
        // exists only to document that a client disconnecting mid-stream is a known, handled
        // no-op rather than an oversight — the transport does not crash or leak the operation,
        // it simply keeps running to completion and its result (and any further progress
        // notifications) is discarded once the peer channel is closed.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::backend::StandaloneBackend;
    use lcg_core::{app_state::AppState, telemetry::NoopSink};
    use std::sync::Arc as StdArc;

    fn test_state() -> StdArc<AppState> {
        StdArc::new(AppState::from_env(
            StdArc::new(NoopSink),
            None,
            Some("test".to_string()),
            "unused".to_string(),
            StdArc::new(lcg_core::MockEmbedder::new(8)),
            "mock".to_string(),
        ))
    }

    fn server(scopes: Vec<Scope>) -> LcgMcpServer<StandaloneBackend> {
        LcgMcpServer::new(
            StandaloneBackend::new(test_state()),
            scopes,
            false,
            Some(CancellationToken::new()),
        )
    }

    #[test]
    fn read_scope_hides_write_and_cypher_and_admin_tools() {
        let s = server(vec![Scope::Read]);
        assert!(s.find_visible("knowledge_status").is_some());
        assert!(s.find_visible("knowledge_add_episode").is_none());
        assert!(s.find_visible("knowledge_query_cypher").is_none());
        assert!(s.find_visible("knowledge_build_indices").is_none());
    }

    #[test]
    fn admin_scope_exposes_wal_lifecycle_tools() {
        let s = server(vec![Scope::Admin]);
        for name in [
            "knowledge_dump_wal",
            "knowledge_prepare_checkpoint",
            "knowledge_rebuild_from_wal",
            "knowledge_recover",
            "knowledge_recover_full",
            "knowledge_close",
            "knowledge_build_indices",
        ] {
            assert!(s.find_visible(name).is_some(), "{name} should be visible");
        }
    }

    #[test]
    fn cypher_scope_exposes_only_query_cypher() {
        let s = server(vec![Scope::Cypher]);
        assert!(s.find_visible("knowledge_query_cypher").is_some());
        assert!(s.find_visible("knowledge_find_entities").is_none());
    }

    #[test]
    fn attached_mode_hides_close_without_allow_remote_close() {
        // AttachedBackend needs a live socket to construct; exercise the same gating logic
        // directly via a fake backend instead of a real connection.
        struct FakeAttached;
        impl McpBackend for FakeAttached {
            async fn call(
                &self,
                _method: &str,
                _params: Value,
                _progress: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
            ) -> IpcResponse {
                IpcResponse::ok(Value::Null, json!({}))
            }
            fn is_attached(&self) -> bool {
                true
            }
        }
        let s = LcgMcpServer::new(FakeAttached, Scope::ALL.to_vec(), false, None);
        assert!(s.find_visible("knowledge_close").is_none());
        assert!(s.find_visible("knowledge_recover").is_some());

        let s_allowed = LcgMcpServer::new(FakeAttached, Scope::ALL.to_vec(), true, None);
        assert!(s_allowed.find_visible("knowledge_close").is_some());
    }

    #[test]
    fn standalone_mode_always_exposes_close_under_admin() {
        let s = server(vec![Scope::Admin]);
        assert!(s.find_visible("knowledge_close").is_some());
    }
}
