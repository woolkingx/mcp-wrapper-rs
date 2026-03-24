//! Conformance tests: validate wrapper-built JSON-RPC structures against
//! the official MCP schema using the vendored schema2object validator.
//!
//! These are unit-level checks — no subprocess spawning. We construct
//! response values using `transport::build_response` / `build_error_response`
//! and validate them against MCP schema definitions.

use std::path::PathBuf;

#[path = "support/mod.rs"]
mod support;
use support::validate::check_value_with_root;

// ── Schema helpers ───────────────────────────────────────────────────────────

fn load_mcp_schema() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-schema.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read mcp-schema.json: {e}"));
    serde_json::from_str(&content).expect("invalid mcp-schema.json")
}

/// Extract `definitions.<name>` and inject the root definitions for $ref resolution.
fn schema_for(root: &serde_json::Value, def_name: &str) -> serde_json::Value {
    let mut def = root["definitions"][def_name].clone();
    if let Some(obj) = def.as_object_mut() {
        obj.insert("definitions".to_string(), root["definitions"].clone());
    }
    def
}

fn assert_conforms(label: &str, data: &serde_json::Value, schema: &serde_json::Value) {
    if let Err(e) = check_value_with_root(schema, data, Some(schema)) {
        panic!(
            "{label} failed schema conformance:\n  data: {}\n  error: {e:?}",
            serde_json::to_string_pretty(data).unwrap()
        );
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn initialize_result_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "InitializeResult");

    let data = serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "test", "version": "0.1.0"}
    });
    assert_conforms("InitializeResult", &data, &schema);
}

#[test]
fn list_tools_result_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "ListToolsResult");

    let data = serde_json::json!({
        "tools": [{
            "name": "echo",
            "description": "Echo input",
            "inputSchema": {"type": "object", "properties": {"msg": {"type": "string"}}}
        }]
    });
    assert_conforms("ListToolsResult", &data, &schema);
}

#[test]
fn list_tools_result_empty_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "ListToolsResult");

    let data = serde_json::json!({"tools": []});
    assert_conforms("ListToolsResult (empty)", &data, &schema);
}

#[test]
fn list_prompts_result_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "ListPromptsResult");

    let data = serde_json::json!({"prompts": []});
    assert_conforms("ListPromptsResult", &data, &schema);
}

#[test]
fn list_resources_result_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "ListResourcesResult");

    let data = serde_json::json!({"resources": []});
    assert_conforms("ListResourcesResult", &data, &schema);
}

#[test]
fn list_resource_templates_result_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "ListResourceTemplatesResult");

    let data = serde_json::json!({"resourceTemplates": []});
    assert_conforms("ListResourceTemplatesResult", &data, &schema);
}

#[test]
fn call_tool_result_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "CallToolResult");

    let data = serde_json::json!({
        "content": [{"type": "text", "text": "hello"}]
    });
    assert_conforms("CallToolResult", &data, &schema);
}

#[test]
fn jsonrpc_response_envelope_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "JSONRPCResponse");

    // build_response equivalent structure
    let data = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"protocolVersion": "2025-03-26", "capabilities": {}, "serverInfo": {"name": "t", "version": "0.1"}}
    });
    assert_conforms("JSONRPCResponse", &data, &schema);
}

#[test]
fn jsonrpc_error_envelope_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "JSONRPCError");

    // build_error_response equivalent structure
    let data = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32603, "message": "internal error"}
    });
    assert_conforms("JSONRPCError", &data, &schema);
}

#[test]
fn jsonrpc_error_with_data_conforms() {
    let root = load_mcp_schema();
    let schema = schema_for(&root, "JSONRPCError");

    let data = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32601, "message": "method not found", "data": {"detail": "unknown"}}
    });
    assert_conforms("JSONRPCError (with data)", &data, &schema);
}
