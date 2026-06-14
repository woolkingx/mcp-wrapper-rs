use serde_json::json;

use super::wrapper::{merge_initialize_result, merge_tools_list};

#[test]
fn merge_initialize_result_adds_tools_capability() {
    let result = merge_initialize_result(json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "serverInfo": {"name": "fixture", "version": "0.0.1"}
    }));

    assert!(result["capabilities"]["tools"].is_object());
}

#[test]
fn merge_tools_list_reserves_mcp_wrapper_name() {
    let result = merge_tools_list(json!({
        "tools": [
            {"name": "echo"},
            {"name": "mcp.wrapper"}
        ]
    }));

    let tools = result["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(
        tools
            .iter()
            .filter(|tool| tool["name"] == "mcp.wrapper")
            .count(),
        1
    );
    assert!(tools.iter().any(|tool| tool["name"] == "echo"));
}

#[test]
fn merge_tools_list_wrapper_descriptor_wins_collision() {
    // `tools` is the external entry / observe+hooks control point. A backend
    // that registers the reserved name must not overwrite or silently keep its
    // own impostor in place of the wrapper descriptor. This pins the collision
    // invariant so the cache internalize cannot regress it.
    let result = merge_tools_list(json!({
        "tools": [
            {"name": "echo"},
            {"name": "mcp.wrapper", "description": "backend impostor"}
        ]
    }));
    let tools = result["tools"].as_array().unwrap();
    let wrapper: Vec<_> = tools
        .iter()
        .filter(|t| t["name"] == "mcp.wrapper")
        .collect();
    assert_eq!(wrapper.len(), 1, "exactly one mcp.wrapper survives");
    assert!(
        wrapper[0]["inputSchema"]["required"]
            .as_array()
            .is_some_and(|r| r.iter().any(|v| v == "action")),
        "surviving mcp.wrapper is the wrapper descriptor, not the backend one"
    );
    assert_ne!(wrapper[0]["description"], "backend impostor");
}
