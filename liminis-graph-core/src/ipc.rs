use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Incoming JSON-RPC 2.0 request (AD-2).
#[derive(Debug, Deserialize)]
pub struct IpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// Outgoing JSON-RPC 2.0 response (AD-2).
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum IpcResponse {
    Ok {
        jsonrpc: String,
        id: Value,
        result: Value,
    },
    Err {
        jsonrpc: String,
        id: Value,
        error: IpcError,
    },
}

#[derive(Debug, Serialize)]
pub struct IpcError {
    pub code: i32,
    pub message: String,
}

impl IpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        IpcResponse::Ok {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        }
    }

    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        IpcResponse::Err {
            jsonrpc: "2.0".to_string(),
            id,
            error: IpcError {
                code,
                message: message.into(),
            },
        }
    }
}
