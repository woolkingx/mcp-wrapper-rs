//! eBPF-isomorphic event plane: spec event mount table, verifier, and legacy
//! route projections.

use serde_json::Value;

use crate::mcp_interface::{McpDataKey, Route, WrapperControlMethod};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    ClientToServer,
    ServerToClient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Request,
    Notification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    Any,
    ToolName(&'static str),
    LegacyMethodPrefix(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventKey {
    pub dir: Dir,
    pub kind: EventKind,
    pub method: &'static str,
    pub selector: Selector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramClass {
    Cache,
    Local,
    Lifetime,
    Control,
    Pass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventProgramId {
    CacheInitializeView,
    CacheToolsList,
    CachePromptsList,
    CacheResourcesList,
    CacheResourceTemplatesList,
    LocalPing,
    DropClientInitialized,
    InvokeWrapperTool,
    LegacyBackendHook,
    ClientRequestThenPass,
    PeerRequestThenPass,
    CancelRequest,
    ProgressRequest,
    RefreshCacheThenPass,
    PassNotification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheRefreshTarget {
    Tools,
    Prompts,
    Resources,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Outcome {
    Pass,
    Respond,
    Forward,
    Drop,
    RefreshThenForward,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountEntry {
    pub key: EventKey,
    pub program_class: ProgramClass,
    pub program: EventProgramId,
    pub outcome: Outcome,
    pub cache_refresh_target: Option<CacheRefreshTarget>,
}

const fn request(
    dir: Dir,
    method: &'static str,
    program_class: ProgramClass,
    program: EventProgramId,
    outcome: Outcome,
) -> MountEntry {
    MountEntry {
        key: EventKey {
            dir,
            kind: EventKind::Request,
            method,
            selector: Selector::Any,
        },
        program_class,
        program,
        outcome,
        cache_refresh_target: None,
    }
}

const fn notification(
    dir: Dir,
    method: &'static str,
    program_class: ProgramClass,
    program: EventProgramId,
    outcome: Outcome,
) -> MountEntry {
    MountEntry {
        key: EventKey {
            dir,
            kind: EventKind::Notification,
            method,
            selector: Selector::Any,
        },
        program_class,
        program,
        outcome,
        cache_refresh_target: None,
    }
}

pub const MOUNT_TABLE: &[MountEntry] = &[
    request(
        Dir::ClientToServer,
        "initialize",
        ProgramClass::Cache,
        EventProgramId::CacheInitializeView,
        Outcome::Respond,
    ),
    notification(
        Dir::ClientToServer,
        "notifications/initialized",
        ProgramClass::Local,
        EventProgramId::DropClientInitialized,
        Outcome::Drop,
    ),
    request(
        Dir::ClientToServer,
        "ping",
        ProgramClass::Local,
        EventProgramId::LocalPing,
        Outcome::Respond,
    ),
    request(
        Dir::ServerToClient,
        "ping",
        ProgramClass::Lifetime,
        EventProgramId::PeerRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ClientToServer,
        "tools/list",
        ProgramClass::Cache,
        EventProgramId::CacheToolsList,
        Outcome::Respond,
    ),
    request(
        Dir::ClientToServer,
        "prompts/list",
        ProgramClass::Cache,
        EventProgramId::CachePromptsList,
        Outcome::Respond,
    ),
    request(
        Dir::ClientToServer,
        "resources/list",
        ProgramClass::Cache,
        EventProgramId::CacheResourcesList,
        Outcome::Respond,
    ),
    request(
        Dir::ClientToServer,
        "resources/templates/list",
        ProgramClass::Cache,
        EventProgramId::CacheResourceTemplatesList,
        Outcome::Respond,
    ),
    MountEntry {
        key: EventKey {
            dir: Dir::ClientToServer,
            kind: EventKind::Request,
            method: "tools/call",
            selector: Selector::ToolName("mcp.wrapper"),
        },
        program_class: ProgramClass::Control,
        program: EventProgramId::InvokeWrapperTool,
        outcome: Outcome::Respond,
        cache_refresh_target: None,
    },
    request(
        Dir::ClientToServer,
        "tools/call",
        ProgramClass::Lifetime,
        EventProgramId::ClientRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ClientToServer,
        "prompts/get",
        ProgramClass::Lifetime,
        EventProgramId::ClientRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ClientToServer,
        "resources/read",
        ProgramClass::Lifetime,
        EventProgramId::ClientRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ClientToServer,
        "resources/subscribe",
        ProgramClass::Lifetime,
        EventProgramId::ClientRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ClientToServer,
        "resources/unsubscribe",
        ProgramClass::Lifetime,
        EventProgramId::ClientRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ClientToServer,
        "completion/complete",
        ProgramClass::Lifetime,
        EventProgramId::ClientRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ClientToServer,
        "logging/setLevel",
        ProgramClass::Lifetime,
        EventProgramId::ClientRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ServerToClient,
        "sampling/createMessage",
        ProgramClass::Lifetime,
        EventProgramId::PeerRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ServerToClient,
        "roots/list",
        ProgramClass::Lifetime,
        EventProgramId::PeerRequestThenPass,
        Outcome::Pass,
    ),
    request(
        Dir::ServerToClient,
        "elicitation/create",
        ProgramClass::Lifetime,
        EventProgramId::PeerRequestThenPass,
        Outcome::Pass,
    ),
    notification(
        Dir::ClientToServer,
        "notifications/cancelled",
        ProgramClass::Lifetime,
        EventProgramId::CancelRequest,
        Outcome::Forward,
    ),
    notification(
        Dir::ServerToClient,
        "notifications/cancelled",
        ProgramClass::Lifetime,
        EventProgramId::CancelRequest,
        Outcome::Forward,
    ),
    notification(
        Dir::ClientToServer,
        "notifications/progress",
        ProgramClass::Lifetime,
        EventProgramId::ProgressRequest,
        Outcome::Forward,
    ),
    notification(
        Dir::ServerToClient,
        "notifications/progress",
        ProgramClass::Lifetime,
        EventProgramId::ProgressRequest,
        Outcome::Forward,
    ),
    MountEntry {
        key: EventKey {
            dir: Dir::ServerToClient,
            kind: EventKind::Notification,
            method: "notifications/tools/list_changed",
            selector: Selector::Any,
        },
        program_class: ProgramClass::Cache,
        program: EventProgramId::RefreshCacheThenPass,
        outcome: Outcome::RefreshThenForward,
        cache_refresh_target: Some(CacheRefreshTarget::Tools),
    },
    MountEntry {
        key: EventKey {
            dir: Dir::ServerToClient,
            kind: EventKind::Notification,
            method: "notifications/prompts/list_changed",
            selector: Selector::Any,
        },
        program_class: ProgramClass::Cache,
        program: EventProgramId::RefreshCacheThenPass,
        outcome: Outcome::RefreshThenForward,
        cache_refresh_target: Some(CacheRefreshTarget::Prompts),
    },
    MountEntry {
        key: EventKey {
            dir: Dir::ServerToClient,
            kind: EventKind::Notification,
            method: "notifications/resources/list_changed",
            selector: Selector::Any,
        },
        program_class: ProgramClass::Cache,
        program: EventProgramId::RefreshCacheThenPass,
        outcome: Outcome::RefreshThenForward,
        cache_refresh_target: Some(CacheRefreshTarget::Resources),
    },
    notification(
        Dir::ServerToClient,
        "notifications/resources/updated",
        ProgramClass::Pass,
        EventProgramId::PassNotification,
        Outcome::Pass,
    ),
    notification(
        Dir::ServerToClient,
        "notifications/message",
        ProgramClass::Pass,
        EventProgramId::PassNotification,
        Outcome::Pass,
    ),
    notification(
        Dir::ClientToServer,
        "notifications/roots/list_changed",
        ProgramClass::Pass,
        EventProgramId::PassNotification,
        Outcome::Pass,
    ),
    MountEntry {
        key: EventKey {
            dir: Dir::ClientToServer,
            kind: EventKind::Request,
            method: "mcp-wrapper/backend/*",
            selector: Selector::LegacyMethodPrefix("mcp-wrapper/backend/"),
        },
        program_class: ProgramClass::Control,
        program: EventProgramId::LegacyBackendHook,
        outcome: Outcome::Respond,
        cache_refresh_target: None,
    },
];

#[cfg(test)]
pub fn mount_table() -> &'static [MountEntry] {
    MOUNT_TABLE
}

pub fn find_mount(
    dir: Dir,
    kind: EventKind,
    method: &str,
    tool_name: Option<&str>,
) -> Option<&'static MountEntry> {
    MOUNT_TABLE.iter().find(|entry| {
        entry.key.dir == dir
            && entry.key.kind == kind
            && selector_matches(entry.key.selector, entry.key.method, method, tool_name)
    })
}

fn selector_matches(
    selector: Selector,
    entry_method: &str,
    method: &str,
    tool_name: Option<&str>,
) -> bool {
    match selector {
        Selector::Any => entry_method == method,
        Selector::ToolName(name) => entry_method == method && tool_name == Some(name),
        Selector::LegacyMethodPrefix(prefix) => method.starts_with(prefix),
    }
}

pub fn route_for_client_request(method: &str) -> Route {
    match find_mount(Dir::ClientToServer, EventKind::Request, method, None).map(|e| e.program) {
        Some(EventProgramId::CacheInitializeView) => Route::McpData(McpDataKey::Initialize),
        Some(EventProgramId::CacheToolsList) => Route::McpData(McpDataKey::ToolsList),
        Some(EventProgramId::CachePromptsList) => Route::McpData(McpDataKey::PromptsList),
        Some(EventProgramId::CacheResourcesList) => Route::McpData(McpDataKey::ResourcesList),
        Some(EventProgramId::CacheResourceTemplatesList) => {
            Route::McpData(McpDataKey::ResourceTemplatesList)
        }
        Some(EventProgramId::LocalPing) => Route::Local,
        Some(EventProgramId::LegacyBackendHook) => legacy_control_route(method),
        _ => Route::PassThrough,
    }
}

pub fn is_wrapper_tool_request(raw: &Value) -> bool {
    let method = raw.get("method").and_then(|value| value.as_str());
    if method != Some("tools/call") {
        return false;
    }
    let tool_name = raw
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(|name| name.as_str());
    find_mount(
        Dir::ClientToServer,
        EventKind::Request,
        "tools/call",
        tool_name,
    )
    .map(|entry| entry.program == EventProgramId::InvokeWrapperTool)
    .unwrap_or(false)
}

fn legacy_control_route(method: &str) -> Route {
    match method {
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

pub fn notification_program(dir: Dir, method: &str) -> Option<EventProgramId> {
    find_mount(dir, EventKind::Notification, method, None).map(|entry| entry.program)
}

pub fn list_changed_data_keys(method: &str) -> Vec<McpDataKey> {
    let Some(entry) = find_mount(Dir::ServerToClient, EventKind::Notification, method, None) else {
        return Vec::new();
    };
    if entry.program != EventProgramId::RefreshCacheThenPass {
        return Vec::new();
    }
    match entry.cache_refresh_target {
        Some(CacheRefreshTarget::Tools) => vec![McpDataKey::ToolsList],
        Some(CacheRefreshTarget::Prompts) => vec![McpDataKey::PromptsList],
        Some(CacheRefreshTarget::Resources) => {
            vec![McpDataKey::ResourcesList, McpDataKey::ResourceTemplatesList]
        }
        None => Vec::new(),
    }
}

pub fn list_changed_notification_methods() -> impl Iterator<Item = &'static str> {
    MOUNT_TABLE
        .iter()
        .filter(|entry| entry.program == EventProgramId::RefreshCacheThenPass)
        .map(|entry| entry.key.method)
}

pub fn should_drop_client_notification(method: &str) -> bool {
    find_mount(Dir::ClientToServer, EventKind::Notification, method, None)
        .map(|entry| entry.outcome == Outcome::Drop)
        .unwrap_or(false)
}

#[cfg(test)]
pub fn verify_mount_table() -> Result<(), String> {
    verify_spec_completeness()?;
    verify_selector_uniqueness()?;
    verify_program_legality()?;
    verify_cache_refresh_targets()
}

#[cfg(test)]
fn verify_spec_completeness() -> Result<(), String> {
    for key in SPEC_EVENT_KEYS {
        if find_mount(key.dir, key.kind, key.method, None).is_none() {
            return Err(format!(
                "missing mount entry for {:?} {:?} {}",
                key.dir, key.kind, key.method
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn verify_selector_uniqueness() -> Result<(), String> {
    for (i, left) in MOUNT_TABLE.iter().enumerate() {
        for right in MOUNT_TABLE.iter().skip(i + 1) {
            if left.key == right.key {
                return Err(format!("duplicate mount entry for {}", left.key.method));
            }
        }
    }

    let wrapper_tool = MOUNT_TABLE
        .iter()
        .position(|entry| entry.key.selector == Selector::ToolName("mcp.wrapper"))
        .ok_or_else(|| "missing mcp.wrapper selector".to_string())?;
    let generic_tool_call = MOUNT_TABLE
        .iter()
        .position(|entry| {
            entry.key.dir == Dir::ClientToServer
                && entry.key.kind == EventKind::Request
                && entry.key.method == "tools/call"
                && entry.key.selector == Selector::Any
        })
        .ok_or_else(|| "missing generic tools/call selector".to_string())?;
    if wrapper_tool > generic_tool_call {
        return Err("mcp.wrapper selector must be mounted before generic tools/call".to_string());
    }
    Ok(())
}

#[cfg(test)]
fn verify_program_legality() -> Result<(), String> {
    for entry in MOUNT_TABLE {
        if entry.key.kind == EventKind::Notification
            && matches!(entry.outcome, Outcome::Respond | Outcome::Reject)
        {
            return Err(format!(
                "notification program responds: {}",
                entry.key.method
            ));
        }
        if entry.program_class == ProgramClass::Pass && entry.outcome != Outcome::Pass {
            return Err(format!(
                "pass program has non-pass outcome: {}",
                entry.key.method
            ));
        }
        if matches!(
            entry.program,
            EventProgramId::CacheInitializeView
                | EventProgramId::CacheToolsList
                | EventProgramId::CachePromptsList
                | EventProgramId::CacheResourcesList
                | EventProgramId::CacheResourceTemplatesList
        ) && entry.program_class != ProgramClass::Cache
        {
            return Err(format!(
                "cache program outside cache class: {}",
                entry.key.method
            ));
        }
        if matches!(
            entry.program,
            EventProgramId::CancelRequest
                | EventProgramId::ProgressRequest
                | EventProgramId::ClientRequestThenPass
                | EventProgramId::PeerRequestThenPass
        ) && entry.program_class != ProgramClass::Lifetime
        {
            return Err(format!(
                "lifetime program outside lifetime class: {}",
                entry.key.method
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn verify_cache_refresh_targets() -> Result<(), String> {
    for entry in MOUNT_TABLE {
        if entry.program == EventProgramId::RefreshCacheThenPass
            && entry.cache_refresh_target.is_none()
        {
            return Err(format!(
                "refresh program missing cache target for {}",
                entry.key.method
            ));
        }
        if entry.program != EventProgramId::RefreshCacheThenPass
            && entry.cache_refresh_target.is_some()
        {
            return Err(format!(
                "non-refresh program has cache target for {}",
                entry.key.method
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
const SPEC_EVENT_KEYS: &[EventKey] = &[
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "initialize",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Notification,
        method: "notifications/initialized",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "ping",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Request,
        method: "ping",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "tools/list",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "prompts/list",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "resources/list",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "resources/templates/list",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "tools/call",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "prompts/get",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "resources/read",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "resources/subscribe",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "resources/unsubscribe",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "completion/complete",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Request,
        method: "logging/setLevel",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Request,
        method: "sampling/createMessage",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Request,
        method: "roots/list",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Request,
        method: "elicitation/create",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Notification,
        method: "notifications/cancelled",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Notification,
        method: "notifications/cancelled",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Notification,
        method: "notifications/progress",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Notification,
        method: "notifications/progress",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Notification,
        method: "notifications/tools/list_changed",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Notification,
        method: "notifications/prompts/list_changed",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Notification,
        method: "notifications/resources/list_changed",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Notification,
        method: "notifications/resources/updated",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ServerToClient,
        kind: EventKind::Notification,
        method: "notifications/message",
        selector: Selector::Any,
    },
    EventKey {
        dir: Dir::ClientToServer,
        kind: EventKind::Notification,
        method: "notifications/roots/list_changed",
        selector: Selector::Any,
    },
];
