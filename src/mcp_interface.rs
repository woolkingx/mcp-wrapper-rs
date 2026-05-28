//! MCP interface owner: JSON-RPC line protocol, MCP method routing, and
//! message builders.

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::debug;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum JsonRpcMessage {
    Request {
        id: Value,
        method: String,
        params: Option<Value>,
    },
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
}

pub async fn read_message<R>(reader: &mut BufReader<R>) -> Option<Value>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.ok()?;
        if n == 0 {
            return None;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => {
                if tracing::enabled!(tracing::Level::DEBUG) {
                    debug!(msg = %trimmed, "recv");
                }
                return Some(value);
            }
            Err(e) => {
                tracing::warn!(err = %e, line = %trimmed, "malformed JSON-RPC line, skipping");
                continue;
            }
        }
    }
}

pub async fn write_message<W>(writer: &mut W, msg: &Value) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let serialized = serde_json::to_string(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if tracing::enabled!(tracing::Level::DEBUG) {
        debug!(msg = %serialized, "send");
    }
    writer.write_all(serialized.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

pub fn classify(msg: &Value) -> JsonRpcMessage {
    let method = msg.get("method").and_then(|v| v.as_str()).map(String::from);
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned();

    match (method, id) {
        (Some(method), Some(id)) => JsonRpcMessage::Request { id, method, params },
        (Some(method), None) => JsonRpcMessage::Notification { method, params },
        (None, Some(id)) => JsonRpcMessage::Response {
            id,
            result: msg.get("result").cloned(),
            error: msg.get("error").cloned(),
        },
        (None, None) => JsonRpcMessage::Notification {
            method: String::new(),
            params: None,
        },
    }
}

pub fn build_request(id: Value, method: &str, params: Option<Value>) -> Value {
    let mut msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
    });
    if let Some(p) = params {
        msg["params"] = p;
    }
    msg
}

pub fn build_response(id: Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

pub fn build_error_response(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = serde_json::json!({
        "code": code,
        "message": message,
    });
    if let Some(d) = data {
        error["data"] = d;
    }
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error,
    })
}

pub fn build_notification(method: &str, params: Option<Value>) -> Value {
    let mut msg = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
    });
    if let Some(p) = params {
        msg["params"] = p;
    }
    msg
}

pub mod error_codes {
    pub const INTERNAL_ERROR: i64 = -32603;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpDataKey {
    Initialize,
    ToolsList,
    PromptsList,
    ResourcesList,
    ResourceTemplatesList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    McpData(McpDataKey),
    PassThrough,
    Local,
    WrapperControl(WrapperControlMethod),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperControlMethod {
    BackendStatus,
    BackendRefresh,
    BackendRestart,
    BackendStop,
    BackendPing,
}

pub fn route(method: &str) -> Route {
    match method {
        "initialize" => Route::McpData(McpDataKey::Initialize),
        "tools/list" => Route::McpData(McpDataKey::ToolsList),
        "prompts/list" => Route::McpData(McpDataKey::PromptsList),
        "resources/list" => Route::McpData(McpDataKey::ResourcesList),
        "resources/templates/list" => Route::McpData(McpDataKey::ResourceTemplatesList),
        "ping" => Route::Local,
        "mcp-wrapper/backend/status" => Route::WrapperControl(WrapperControlMethod::BackendStatus),
        "mcp-wrapper/backend/refresh" => {
            Route::WrapperControl(WrapperControlMethod::BackendRefresh)
        }
        "mcp-wrapper/backend/restart" => {
            Route::WrapperControl(WrapperControlMethod::BackendRestart)
        }
        "mcp-wrapper/backend/stop" => Route::WrapperControl(WrapperControlMethod::BackendStop),
        "mcp-wrapper/backend/ping" => Route::WrapperControl(WrapperControlMethod::BackendPing),
        _ => Route::PassThrough,
    }
}

pub fn is_list_changed_notification(method: &str) -> Option<McpDataKey> {
    match method {
        "notifications/tools/list_changed" => Some(McpDataKey::ToolsList),
        "notifications/prompts/list_changed" => Some(McpDataKey::PromptsList),
        "notifications/resources/list_changed" => Some(McpDataKey::ResourcesList),
        _ => None,
    }
}

pub fn method_for_mcp_data_key(key: &McpDataKey) -> &'static str {
    match key {
        McpDataKey::Initialize => "initialize",
        McpDataKey::ToolsList => "tools/list",
        McpDataKey::PromptsList => "prompts/list",
        McpDataKey::ResourcesList => "resources/list",
        McpDataKey::ResourceTemplatesList => "resources/templates/list",
    }
}
