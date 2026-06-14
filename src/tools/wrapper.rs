use serde_json::{json, Value};

use crate::tools::schema::{parse_invocation, to_call_tool_result, InvocationContext};

pub const WRAPPER_TOOL_NAME: &str = "mcp.wrapper";

pub fn is_wrapper_tool_call(raw: &Value) -> bool {
    raw.get("method").and_then(|v| v.as_str()) == Some("tools/call")
        && raw
            .get("params")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            == Some(WRAPPER_TOOL_NAME)
}

/// The declared `mcp.wrapper` schema. `ToolInvocation::from_arguments` /
/// `ToolAction::validate_params` are its runtime projection — enum variants and
/// the `force` type must match this descriptor. Built only when the cache
/// rebuilds its tools view (on load/refresh), not per request.
pub fn tool_descriptor() -> Value {
    json!({
        "name": WRAPPER_TOOL_NAME,
        "description": "Unified mcp-wrapper control tool for backend status, ping, refresh, restart, and stop actions.",
        "inputSchema": {
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "backend.status",
                        "backend.ping",
                        "backend.refresh",
                        "backend.restart",
                        "backend.stop"
                    ]
                },
                "target": {
                    "type": "string",
                    "default": "default",
                    "description": "Backend target. Phase 1 supports only default."
                },
                "params": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Action-specific parameters. backend.restart accepts force:boolean."
                }
            }
        }
    })
}

pub fn merge_tools_list(mut result: Value) -> Value {
    if !result.is_object() {
        result = json!({"tools": []});
    }
    if !result.get("tools").map(|v| v.is_array()).unwrap_or(false) {
        result["tools"] = json!([]);
    }

    if let Some(tools) = result.get_mut("tools").and_then(|v| v.as_array_mut()) {
        tools.retain(|tool| {
            tool.get("name")
                .and_then(|v| v.as_str())
                .map(|name| name != WRAPPER_TOOL_NAME)
                .unwrap_or(true)
        });
        tools.push(tool_descriptor());
    }

    result
}

pub fn merge_initialize_result(mut result: Value) -> Value {
    if !result.is_object() {
        result = json!({});
    }
    if !result
        .get("capabilities")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        result["capabilities"] = json!({});
    }
    if result
        .get("capabilities")
        .and_then(|v| v.get("tools"))
        .is_none()
    {
        result["capabilities"]["tools"] = json!({});
    }
    result
}

pub async fn invoke(raw: &Value, context: InvocationContext<'_>) -> Value {
    let arguments = raw.get("params").and_then(|v| v.get("arguments"));
    let invocation = match parse_invocation(arguments) {
        Ok(invocation) => invocation,
        Err(envelope) => return to_call_tool_result(envelope),
    };
    let envelope = crate::tools::backend::invoke(invocation, context).await;
    to_call_tool_result(envelope)
}
