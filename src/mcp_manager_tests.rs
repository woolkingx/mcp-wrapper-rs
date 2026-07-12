use serde_json::json;

use crate::mcp_manager;

#[test]
fn protocol_version_for_unix_days_formats_calendar_dates() {
    assert_eq!(mcp_manager::protocol_version_for_unix_days(0), "1970-01-01");
    assert_eq!(
        mcp_manager::protocol_version_for_unix_days(10_957),
        "2000-01-01"
    );
    assert_eq!(
        mcp_manager::protocol_version_for_unix_days(19_782),
        "2024-02-29"
    );
    assert_eq!(
        mcp_manager::protocol_version_for_unix_days(20_626),
        "2026-06-22"
    );
}

#[test]
fn initialize_request_uses_today_protocol_version() {
    let req = mcp_manager::build_initialize_request(json!(1));
    assert_eq!(
        req["params"]["protocolVersion"],
        json!(mcp_manager::protocol_version_today())
    );
    assert_ne!(req["params"]["protocolVersion"], json!("2024-11-05"));
}

#[test]
fn cached_list_cursor_null_is_not_a_frontend_cursor() {
    assert!(!mcp_manager::cached_list_request_has_cursor(
        &crate::mcp_interface::McpDataKey::ToolsList,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {"cursor": null}
        })
    ));
    assert!(mcp_manager::cached_list_request_has_cursor(
        &crate::mcp_interface::McpDataKey::ToolsList,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {"cursor": "page-2"}
        })
    ));
}

#[test]
fn list_refresh_rejects_error_and_malformed_envelopes() {
    assert!(mcp_manager::validate_list_response(
        &json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001, "message": "no"}}),
        "tools",
        "tools/list",
        0,
    )
    .is_err());
    assert!(mcp_manager::validate_list_response(
        &json!({"jsonrpc": "2.0", "id": 1}),
        "tools",
        "tools/list",
        0,
    )
    .is_err());
    assert!(mcp_manager::validate_list_response(
        &json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": {}}}),
        "tools",
        "tools/list",
        0,
    )
    .is_err());
    assert!(mcp_manager::validate_list_response(
        &json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [], "nextCursor": 2}}),
        "tools",
        "tools/list",
        0,
    )
    .is_err());
    assert!(mcp_manager::validate_list_response(
        &json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}}),
        "tools",
        "tools/list",
        0,
    )
    .is_ok());
}
