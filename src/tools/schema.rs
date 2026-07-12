use std::sync::Arc;

use serde_json::{json, Value};

use crate::{backend_manager, mcp_manager};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAction {
    BackendStatus,
    BackendPing,
    BackendRefresh,
    BackendRestart,
    BackendStop,
}

impl ToolAction {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "backend.status" => Some(Self::BackendStatus),
            "backend.ping" => Some(Self::BackendPing),
            "backend.refresh" => Some(Self::BackendRefresh),
            "backend.restart" => Some(Self::BackendRestart),
            "backend.stop" => Some(Self::BackendStop),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::BackendStatus => "backend.status",
            Self::BackendPing => "backend.ping",
            Self::BackendRefresh => "backend.refresh",
            Self::BackendRestart => "backend.restart",
            Self::BackendStop => "backend.stop",
        }
    }

    /// Validate action-specific params. The action owns its param constraint
    /// because it owns the action's data shape: only `BackendRestart`
    /// constrains `force` to a boolean. Returns the invalid-params message when
    /// the constraint is violated. Declared counterpart: `tool_descriptor()`
    /// inputSchema in `wrapper.rs`.
    pub fn validate_params(self, params: &Value) -> Result<(), &'static str> {
        match self {
            Self::BackendRestart => {
                if params.get("force").is_some_and(|v| !v.is_boolean()) {
                    return Err("`params.force` must be a boolean");
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

pub struct ToolInvocation {
    pub action: ToolAction,
    pub target: String,
    pub params: Value,
}

pub struct InvocationContext<'a> {
    pub cache: &'a Arc<mcp_manager::Cache>,
    pub backend_slot: &'a backend_manager::BackendSlot,
    pub active_calls: usize,
}

impl ToolInvocation {
    /// Build a validated invocation from the tool-call arguments object. The
    /// invocation owns its invariant: a successfully constructed
    /// `ToolInvocation` is always schema-valid (legal action, legal target,
    /// well-formed params, action-specific params valid). Returns an error
    /// envelope when the arguments violate the constraint.
    pub fn from_arguments(arguments: Option<&Value>) -> Result<Self, Value> {
        let args = arguments.and_then(|v| v.as_object()).ok_or_else(|| {
            error_envelope(
                "unknown",
                "default",
                "invalidParams",
                "`arguments` must be an object with an action field",
            )
        })?;

        let action_name = args.get("action").and_then(|v| v.as_str()).ok_or_else(|| {
            error_envelope(
                "unknown",
                "default",
                "invalidParams",
                "`action` is required",
            )
        })?;
        let action = ToolAction::from_name(action_name).ok_or_else(|| {
            error_envelope(
                action_name,
                "default",
                "unknownAction",
                "unsupported mcp.wrapper action",
            )
        })?;
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        if target != "default" {
            return Err(error_envelope(
                action.name(),
                &target,
                "unsupportedTarget",
                "only target `default` is supported in single-backend mode",
            ));
        }
        let params = args
            .get("params")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if !params.is_object() {
            return Err(error_envelope(
                action.name(),
                &target,
                "invalidParams",
                "`params` must be an object when present",
            ));
        }
        action
            .validate_params(&params)
            .map_err(|msg| error_envelope(action.name(), &target, "invalidParams", msg))?;

        Ok(Self {
            action,
            target,
            params,
        })
    }
}

pub fn parse_invocation(arguments: Option<&Value>) -> Result<ToolInvocation, Value> {
    ToolInvocation::from_arguments(arguments)
}

pub fn ok_envelope(action: &str, target: &str, result: Value) -> Value {
    json!({
        "ok": true,
        "action": action,
        "target": target,
        "result": result,
        "error": Value::Null,
        "evidence": {}
    })
}

pub fn error_envelope(action: &str, target: &str, code: &str, message: &str) -> Value {
    json!({
        "ok": false,
        "action": action,
        "target": target,
        "result": Value::Null,
        "error": {
            "code": code,
            "message": message
        },
        "evidence": {}
    })
}

pub fn to_call_tool_result(envelope: Value) -> Value {
    let is_error = !envelope
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let text = serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{
            "type": "text",
            "text": text
        }],
        "isError": is_error,
        "_meta": {
            "mcpWrapper": envelope
        }
    })
}
