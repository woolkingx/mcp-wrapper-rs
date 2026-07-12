use crate::event_plane::{
    find_mount, is_wrapper_tool_request, list_changed_data_keys, mount_table, notification_program,
    verify_mount_table, Dir, EventKind, EventProgramId, Outcome,
};
use crate::mcp_interface::McpDataKey;
use serde_json::json;

#[test]
fn verifier_accepts_static_mount_table() {
    verify_mount_table().expect("mount table should satisfy verifier gates");
}

#[test]
fn mount_table_has_explicit_spec_entries() {
    let expected = [
        (Dir::ClientToServer, EventKind::Request, "initialize"),
        (
            Dir::ClientToServer,
            EventKind::Notification,
            "notifications/initialized",
        ),
        (Dir::ClientToServer, EventKind::Request, "tools/list"),
        (Dir::ClientToServer, EventKind::Request, "tools/call"),
        (
            Dir::ServerToClient,
            EventKind::Request,
            "sampling/createMessage",
        ),
        (
            Dir::ServerToClient,
            EventKind::Notification,
            "notifications/resources/updated",
        ),
        (
            Dir::ClientToServer,
            EventKind::Notification,
            "notifications/roots/list_changed",
        ),
    ];

    for (dir, kind, method) in expected {
        assert!(
            find_mount(dir, kind, method, None).is_some(),
            "missing mount entry for {method}"
        );
    }
}

#[test]
fn wrapper_tool_selector_precedes_generic_tools_call() {
    let wrapper_index = mount_table()
        .iter()
        .position(|entry| entry.program == EventProgramId::InvokeWrapperTool)
        .expect("wrapper tool mount");
    let generic_index = mount_table()
        .iter()
        .position(|entry| {
            entry.key.method == "tools/call"
                && entry.program == EventProgramId::ClientRequestThenPass
        })
        .expect("generic tools/call mount");

    assert!(wrapper_index < generic_index);
    assert_eq!(
        find_mount(
            Dir::ClientToServer,
            EventKind::Request,
            "tools/call",
            Some("mcp.wrapper")
        )
        .map(|entry| entry.program),
        Some(EventProgramId::InvokeWrapperTool)
    );
}

#[test]
fn unknown_future_request_has_no_mount_and_passes_by_default() {
    assert!(find_mount(
        Dir::ClientToServer,
        EventKind::Request,
        "future/method",
        None
    )
    .is_none());
}

#[test]
fn notifications_do_not_respond() {
    for entry in mount_table()
        .iter()
        .filter(|entry| entry.key.kind == EventKind::Notification)
    {
        assert_ne!(entry.outcome, Outcome::Respond);
    }
}

#[test]
fn list_changed_projection_is_cache_refresh_program() {
    assert_eq!(
        list_changed_data_keys("notifications/tools/list_changed"),
        vec![McpDataKey::ToolsList]
    );
    assert_eq!(
        list_changed_data_keys("notifications/prompts/list_changed"),
        vec![McpDataKey::PromptsList]
    );
    assert_eq!(
        list_changed_data_keys("notifications/resources/list_changed"),
        vec![McpDataKey::ResourcesList, McpDataKey::ResourceTemplatesList]
    );
    assert!(find_mount(
        Dir::ServerToClient,
        EventKind::Notification,
        "notifications/resources/templates/list_changed",
        None
    )
    .is_none());
    assert!(list_changed_data_keys("notifications/other").is_empty());
}

#[test]
fn initialized_notification_is_local_drop_program() {
    assert!(crate::event_plane::should_drop_client_notification(
        "notifications/initialized"
    ));
    assert!(!crate::event_plane::should_drop_client_notification(
        "notifications/roots/list_changed"
    ));
}

#[test]
fn wrapper_tool_request_projects_from_mount_selector() {
    assert!(is_wrapper_tool_request(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "mcp.wrapper", "arguments": {}}
    })));
    assert!(!is_wrapper_tool_request(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "echo", "arguments": {}}
    })));
}

#[test]
fn lifetime_notifications_project_from_mount_table() {
    assert_eq!(
        notification_program(Dir::ClientToServer, "notifications/cancelled"),
        Some(EventProgramId::CancelRequest)
    );
    assert_eq!(
        notification_program(Dir::ServerToClient, "notifications/progress"),
        Some(EventProgramId::ProgressRequest)
    );
    assert_eq!(
        notification_program(Dir::ClientToServer, "notifications/roots/list_changed"),
        Some(EventProgramId::PassNotification)
    );
}
