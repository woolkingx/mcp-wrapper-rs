//! JSON-RPC line protocol layer.
//!
//! Reads and writes newline-delimited JSON-RPC 2.0 messages over async streams.
//! Pure JSON-RPC — no MCP-specific logic.

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::debug;

/// Classified JSON-RPC message.
#[derive(Debug, Clone)]
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

/// Classify a raw JSON value into a JSON-RPC message type.
///
/// Rules:
/// - Has `method` + has `id` -> Request
/// - Has `method` + no `id` -> Notification
/// - Has `id` + no `method` (has `result` or `error`) -> Response
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
        // Malformed: no method, no id — treat as notification with empty method
        // so callers can detect and reject
        (None, None) => JsonRpcMessage::Notification {
            method: String::new(),
            params: None,
        },
    }
}

/// Read one newline-delimited JSON message from an async buffered reader.
///
/// Returns `None` on EOF. Skips empty lines.
/// Returns `Some(value)` on successful parse, or `None` if the stream ends.
pub async fn read_message<R>(reader: &mut BufReader<R>) -> Option<Value>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.ok()?;
        if n == 0 {
            return None; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue; // skip blank lines
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => {
                if tracing::enabled!(tracing::Level::DEBUG) {
                    debug!(msg = %trimmed, "recv");
                }
                return Some(value);
            }
            Err(e) => {
                // Log and skip malformed lines — don't break the stream
                tracing::warn!(err = %e, line = %trimmed, "malformed JSON-RPC line, skipping");
                continue;
            }
        }
    }
}

/// Write a JSON-RPC message as a newline-delimited line to an async writer.
///
/// Flushes after each write to ensure timely delivery.
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

/// Build a JSON-RPC 2.0 request object.
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

/// Build a JSON-RPC 2.0 success response object.
pub fn build_response(id: Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

/// Build a JSON-RPC 2.0 error response object.
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

/// Build a JSON-RPC 2.0 notification object (no id).
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

/// Standard JSON-RPC error codes.
pub mod error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}
