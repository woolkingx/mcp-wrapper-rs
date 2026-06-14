//! MCP manager owner: session handshake, capabilities, discovery cache, and refresh rules.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::backend_manager::{Backend, BackendEvent, ChildPgids};
use crate::mcp_interface::{self, McpDataKey};
use crate::tools;

/// Timeout for individual list/* queries during init.
/// Separate from --init-timeout, which covers the initialize handshake.
const LIST_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Parsed MCP server capabilities from the initialize handshake.
/// Determines which list/* methods to query and cache.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub tools: bool,
    pub prompts: bool,
    pub resources: bool,
}

impl Capabilities {
    fn from_server_info(server_info: &Value) -> Self {
        let caps = server_info.get("capabilities").unwrap_or(&Value::Null);
        Self {
            tools: caps.get("tools").is_some(),
            prompts: caps.get("prompts").is_some(),
            resources: caps.get("resources").is_some(),
        }
    }

    pub fn supports(&self, key: &McpDataKey) -> bool {
        match key {
            McpDataKey::Initialize => true,
            McpDataKey::ToolsList => self.tools,
            McpDataKey::PromptsList => self.prompts,
            McpDataKey::ResourcesList | McpDataKey::ResourceTemplatesList => self.resources,
        }
    }
}

/// Cached MCP discovery results from the backend MCP server.
///
/// `responses` and `server_info` hold pure backend data; `discovery_hash` is
/// computed from them only, so the backend fingerprint stays clean. The
/// `*_view` fields are the externally served projections (backend data plus the
/// reserved `mcp.wrapper` tool / tools capability). They are derived hold-state:
/// rebuilt by `rebuild_views` whenever backend data is loaded or refreshed, so
/// `lookup` is a pure read with no per-request merge.
struct CachedData {
    responses: HashMap<McpDataKey, Value>,
    server_info: Value,
    capabilities: Capabilities,
    backend_generation: u64,
    cache_epoch: u64,
    capabilities_hash: String,
    discovery_hash: String,
    tools_list_view: Value,
    initialize_view: Value,
}

impl CachedData {
    /// Recompute the externally served views from current backend data. The
    /// cache owns the lifetime of these views: they include the reserved
    /// `mcp.wrapper` tool and the tools capability, and never feed back into
    /// `responses` or `discovery_hash`.
    fn rebuild_views(&mut self) {
        self.initialize_view = tools::merge_initialize_result(self.server_info.clone());
        self.tools_list_view = match self.responses.get(&McpDataKey::ToolsList).cloned() {
            Some(list) => tools::merge_tools_list(list),
            None => tools::merge_tools_list(serde_json::json!({"tools": []})),
        };
    }
}

/// Thread-safe discovery cache. Reads are fast and never cross await points.
pub struct Cache {
    data: std::sync::RwLock<CachedData>,
}

pub struct McpSnapshot {
    pub backend_generation: u64,
    pub cache_epoch: u64,
    pub server_info: Value,
    pub capabilities_hash: String,
    pub discovery_hash: String,
}

pub struct RefreshReport {
    pub old_cache_epoch: u64,
    pub new_cache_epoch: u64,
    pub old_discovery_hash: String,
    pub new_discovery_hash: String,
    pub changed: bool,
}

impl RefreshReport {
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "oldCacheEpoch": self.old_cache_epoch,
            "newCacheEpoch": self.new_cache_epoch,
            "oldDiscoveryHash": self.old_discovery_hash,
            "newDiscoveryHash": self.new_discovery_hash,
            "changed": self.changed,
        })
    }
}

impl McpSnapshot {
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "backendGeneration": self.backend_generation,
            "cacheEpoch": self.cache_epoch,
            "serverInfo": self.server_info,
            "capabilitiesHash": self.capabilities_hash,
            "discoveryHash": self.discovery_hash,
        })
    }
}

impl Cache {
    fn new(data: CachedData) -> Self {
        Self {
            data: std::sync::RwLock::new(data),
        }
    }

    /// Read a cached response for the given key. For `Initialize`, returns the
    /// backend server info from the successful initialize result.
    pub fn lookup(&self, key: &McpDataKey) -> Option<Value> {
        let guard = self.data.read().unwrap();
        // Pure read of pre-built hold-state: the wrapper-merged views are
        // rebuilt on load/refresh, never recomputed per request.
        match key {
            McpDataKey::Initialize => Some(guard.initialize_view.clone()),
            McpDataKey::ToolsList => Some(guard.tools_list_view.clone()),
            _ => guard.responses.get(key).cloned(),
        }
    }

    /// Replace a cached entry after list_changed invalidation + refresh.
    /// Unsupported capability slots are intentionally ignored.
    pub fn update(&self, key: &McpDataKey, value: Value) {
        let mut guard = self.data.write().unwrap();
        if *key == McpDataKey::Initialize {
            guard.server_info = value;
            guard.capabilities = Capabilities::from_server_info(&guard.server_info);
            guard.capabilities_hash = hash_value(
                guard
                    .server_info
                    .get("capabilities")
                    .unwrap_or(&Value::Null),
            );
        } else if guard.capabilities.supports(key) {
            guard.responses.insert(*key, value);
        }
        guard.cache_epoch += 1;
        guard.discovery_hash = discovery_hash(&guard.server_info, &guard.responses);
        guard.rebuild_views();
    }

    pub fn snapshot(&self) -> McpSnapshot {
        let guard = self.data.read().unwrap();
        McpSnapshot {
            backend_generation: guard.backend_generation,
            cache_epoch: guard.cache_epoch,
            server_info: guard.server_info.clone(),
            capabilities_hash: guard.capabilities_hash.clone(),
            discovery_hash: guard.discovery_hash.clone(),
        }
    }

    pub async fn refresh_all(&self, backend: &Backend, backend_generation: u64) -> RefreshReport {
        let (old_backend_generation, old_cache_epoch, old_discovery_hash, old_server_info) = {
            let guard = self.data.read().unwrap();
            (
                guard.backend_generation,
                guard.cache_epoch,
                guard.discovery_hash.clone(),
                guard.server_info.clone(),
            )
        };

        let server_info = backend
            .server_info_snapshot()
            .await
            .unwrap_or(old_server_info);
        let capabilities = Capabilities::from_server_info(&server_info);

        let mut responses = HashMap::new();
        for key in discovery_data_keys() {
            let result = query_list_or_empty(backend, &capabilities, key).await;
            responses.insert(key, result);
        }

        let new_discovery_hash = discovery_hash(&server_info, &responses);
        let changed = new_discovery_hash != old_discovery_hash
            || backend_generation != old_backend_generation;
        let new_cache_epoch = if changed {
            old_cache_epoch + 1
        } else {
            old_cache_epoch
        };

        {
            let mut guard = self.data.write().unwrap();
            guard.responses = responses;
            guard.server_info = server_info;
            guard.capabilities = capabilities;
            guard.backend_generation = backend_generation;
            guard.discovery_hash = new_discovery_hash.clone();
            guard.cache_epoch = new_cache_epoch;
            guard.capabilities_hash = hash_value(
                guard
                    .server_info
                    .get("capabilities")
                    .unwrap_or(&Value::Null),
            );
            guard.rebuild_views();
        }

        RefreshReport {
            old_cache_epoch,
            new_cache_epoch,
            old_discovery_hash,
            new_discovery_hash,
            changed,
        }
    }
}

/// Build the MCP initialize request.
pub fn build_initialize_request(id: Value) -> Value {
    mcp_interface::build_request(
        id,
        "initialize",
        Some(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "mcp-wrapper-rs",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
    )
}

fn build_list_request(id: Value, method: &str) -> Value {
    mcp_interface::build_request(id, method, None)
}

fn empty_result_for(key: &McpDataKey) -> Value {
    match key {
        McpDataKey::ToolsList => serde_json::json!({"tools": []}),
        McpDataKey::PromptsList => serde_json::json!({"prompts": []}),
        McpDataKey::ResourcesList => serde_json::json!({"resources": []}),
        McpDataKey::ResourceTemplatesList => serde_json::json!({"resourceTemplates": []}),
        McpDataKey::Initialize => Value::Object(serde_json::Map::new()),
    }
}

/// Run the MCP initialize handshake against an already spawned backend.
///
/// This owns the protocol sequence only: initialize request, response checking,
/// and initialized notification. It does not spawn or kill a process.
pub async fn initialize_backend(
    backend: &Backend,
    init_timeout: Duration,
) -> Result<Value, String> {
    let init_req = build_initialize_request(backend.next_request_id());
    let init_resp = tokio::time::timeout(init_timeout, backend.send_request(init_req))
        .await
        .map_err(|_| "initialize handshake timeout".to_string())?
        .map_err(|e| format!("initialize handshake: {}", e))?;

    if init_resp.get("error").is_some() {
        return Err(format!(
            "initialize rejected: {}",
            init_resp.get("error").unwrap()
        ));
    }

    let initialized_notif = mcp_interface::build_notification("notifications/initialized", None);
    backend
        .send_notification(&initialized_notif)
        .await
        .map_err(|e| format!("send initialized notification: {}", e))?;

    let server_info = init_resp
        .get("result")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    backend.set_server_info(server_info.clone()).await;
    Ok(server_info)
}

async fn build_discovery_cache_from_backend(
    backend: &Backend,
    init_timeout: Duration,
    backend_generation: u64,
) -> Result<Cache, Box<dyn std::error::Error>> {
    let server_info = initialize_backend(backend, init_timeout).await.map_err(
        |e| -> Box<dyn std::error::Error> {
            std::io::Error::new(std::io::ErrorKind::Other, e).into()
        },
    )?;

    let capabilities = Capabilities::from_server_info(&server_info);
    info!(
        tools = capabilities.tools,
        prompts = capabilities.prompts,
        resources = capabilities.resources,
        "mcp_manager: server capabilities"
    );

    let mut responses = HashMap::new();
    for key in discovery_data_keys() {
        let result = query_list_or_empty(backend, &capabilities, key).await;
        responses.insert(key, result);
    }

    log_cache_counts(&responses);
    let capabilities_hash = hash_value(server_info.get("capabilities").unwrap_or(&Value::Null));
    let discovery_hash = discovery_hash(&server_info, &responses);

    let mut data = CachedData {
        responses,
        server_info,
        capabilities,
        backend_generation,
        cache_epoch: 1,
        capabilities_hash,
        discovery_hash,
        tools_list_view: Value::Null,
        initialize_view: Value::Null,
    };
    data.rebuild_views();
    Ok(Cache::new(data))
}

fn discovery_data_keys() -> [McpDataKey; 4] {
    [
        McpDataKey::ToolsList,
        McpDataKey::PromptsList,
        McpDataKey::ResourcesList,
        McpDataKey::ResourceTemplatesList,
    ]
}

async fn query_list_or_empty(
    backend: &Backend,
    capabilities: &Capabilities,
    key: McpDataKey,
) -> Value {
    if !capabilities.supports(&key) {
        debug!(
            method = mcp_interface::method_for_mcp_data_key(&key),
            "mcp_manager: skipped list query (not in capabilities)"
        );
        return empty_result_for(&key);
    }

    let method = mcp_interface::method_for_mcp_data_key(&key);
    let req = build_list_request(backend.next_request_id(), method);
    match tokio::time::timeout(LIST_QUERY_TIMEOUT, backend.send_request(req)).await {
        Ok(Ok(resp)) => resp
            .get("result")
            .cloned()
            .unwrap_or(empty_result_for(&key)),
        Ok(Err(e)) => {
            warn!(method = method, err = %e, "mcp_manager: list query failed");
            empty_result_for(&key)
        }
        Err(_) => {
            warn!(method = method, "mcp_manager: list query timeout");
            empty_result_for(&key)
        }
    }
}

fn log_cache_counts(responses: &HashMap<McpDataKey, Value>) {
    let tool_count = count_array(responses, McpDataKey::ToolsList, "tools");
    let prompt_count = count_array(responses, McpDataKey::PromptsList, "prompts");
    let resource_count = count_array(responses, McpDataKey::ResourcesList, "resources");
    let template_count = count_array(
        responses,
        McpDataKey::ResourceTemplatesList,
        "resourceTemplates",
    );

    info!(
        tools = tool_count,
        prompts = prompt_count,
        resources = resource_count,
        resource_templates = template_count,
        "mcp_manager: cached"
    );
}

fn count_array(responses: &HashMap<McpDataKey, Value>, key: McpDataKey, field: &str) -> usize {
    responses
        .get(&key)
        .and_then(|v| v.get(field))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

fn hash_value(value: &Value) -> String {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(value)
        .unwrap_or_default()
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn discovery_hash(server_info: &Value, responses: &HashMap<McpDataKey, Value>) -> String {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(server_info)
        .unwrap_or_default()
        .hash(&mut hasher);
    for key in discovery_data_keys() {
        let value = responses.get(&key).unwrap_or(&Value::Null);
        serde_json::to_string(value)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Spawn a temporary backend, build cache, then kill the backend.
pub async fn init_cache(
    cmd: &str,
    args: &[String],
    init_timeout: Duration,
    child_pgids: &ChildPgids,
) -> Result<Cache, Box<dyn std::error::Error>> {
    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while notif_rx.recv().await.is_some() {} });

    let backend = Backend::spawn(cmd, args, notif_tx, child_pgids)?;
    debug!("mcp_manager: init backend spawned");
    let cache = build_discovery_cache_from_backend(&backend, init_timeout, 0).await?;
    backend.kill().await;
    Ok(cache)
}

/// Spawn a backend, build cache, and keep the initialized backend alive.
pub async fn init_cache_with_backend(
    cmd: &str,
    args: &[String],
    init_timeout: Duration,
    child_pgids: &ChildPgids,
    notif_tx: mpsc::UnboundedSender<BackendEvent>,
) -> Result<(Cache, Backend), Box<dyn std::error::Error>> {
    let backend = Backend::spawn(cmd, args, notif_tx, child_pgids)?;
    debug!("mcp_manager: init backend spawned");
    let cache = build_discovery_cache_from_backend(&backend, init_timeout, 1).await?;
    Ok((cache, backend))
}
