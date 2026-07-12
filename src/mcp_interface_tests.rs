use crate::mcp_interface::{
    method_for_mcp_data_key, route, McpDataKey, Route, WrapperControlMethod,
};

#[test]
fn mcp_data_methods() {
    assert_eq!(route("initialize"), Route::McpData(McpDataKey::Initialize));
    assert_eq!(route("tools/list"), Route::McpData(McpDataKey::ToolsList));
    assert_eq!(
        route("prompts/list"),
        Route::McpData(McpDataKey::PromptsList)
    );
    assert_eq!(
        route("resources/list"),
        Route::McpData(McpDataKey::ResourcesList)
    );
    assert_eq!(
        route("resources/templates/list"),
        Route::McpData(McpDataKey::ResourceTemplatesList)
    );
}

#[test]
fn local_methods() {
    assert_eq!(route("ping"), Route::Local);
}

#[test]
fn wrapper_control_methods() {
    assert_eq!(
        route("mcp-wrapper/backend/status"),
        Route::WrapperControl(WrapperControlMethod::BackendStatus)
    );
    assert_eq!(
        route("mcp-wrapper/backend/refresh"),
        Route::WrapperControl(WrapperControlMethod::BackendRefresh)
    );
    assert_eq!(
        route("mcp-wrapper/backend/restart"),
        Route::WrapperControl(WrapperControlMethod::BackendRestart)
    );
    assert_eq!(
        route("mcp-wrapper/backend/stop"),
        Route::WrapperControl(WrapperControlMethod::BackendStop)
    );
    assert_eq!(
        route("mcp-wrapper/backend/ping"),
        Route::WrapperControl(WrapperControlMethod::BackendPing)
    );
}

#[test]
fn passthrough_methods() {
    assert_eq!(route("tools/call"), Route::PassThrough);
    assert_eq!(route("resources/read"), Route::PassThrough);
    assert_eq!(route("prompts/get"), Route::PassThrough);
    assert_eq!(route("completion/complete"), Route::PassThrough);
    assert_eq!(route("logging/setLevel"), Route::PassThrough);
    assert_eq!(route("resources/subscribe"), Route::PassThrough);
    assert_eq!(route("resources/unsubscribe"), Route::PassThrough);
    assert_eq!(route("some/future/method"), Route::PassThrough);
}

#[test]
fn roundtrip_mcp_data_keys() {
    let keys = [
        McpDataKey::Initialize,
        McpDataKey::ToolsList,
        McpDataKey::PromptsList,
        McpDataKey::ResourcesList,
        McpDataKey::ResourceTemplatesList,
    ];
    for key in &keys {
        let method = method_for_mcp_data_key(key);
        assert_eq!(route(method), Route::McpData(*key));
    }
}
