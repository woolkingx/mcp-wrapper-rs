//! MCP manager owner: session handshake, capabilities, discovery cache, and refresh rules.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::backend_manager::{Backend, BackendEvent, BackendSlot, ChildPgids};
use crate::mcp_interface::{self, McpDataKey};
use crate::timeouts;
use crate::tools;

/// Timeout for individual list/* queries during init.
/// Separate from --init-timeout, which covers the initialize handshake.
const LIST_QUERY_TIMEOUT: Duration = Duration::from_secs(timeouts::CACHE_LIST_QUERY_TIMEOUT_SECS);
const CACHE_REFRESH_TIMEOUT: Duration = Duration::from_secs(timeouts::CACHE_REFRESH_TIMEOUT_SECS);

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
    refresh_lock: Mutex<()>,
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
            refresh_lock: Mutex::new(()),
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

    pub async fn refresh_all(&self, backend_slot: &BackendSlot) -> Result<RefreshReport, String> {
        self.refresh_keys(backend_slot, &discovery_data_keys())
            .await
    }

    pub async fn refresh_keys(
        &self,
        backend_slot: &BackendSlot,
        keys: &[McpDataKey],
    ) -> Result<RefreshReport, String> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let backend = backend_slot
            .live()
            .await
            .ok_or_else(|| "backend unavailable for cache refresh".to_string())?;
        let backend_generation = backend_slot.generation();
        let (
            old_backend_generation,
            old_cache_epoch,
            old_discovery_hash,
            old_server_info,
            old_responses,
        ) = {
            let guard = self.data.read().unwrap();
            (
                guard.backend_generation,
                guard.cache_epoch,
                guard.discovery_hash.clone(),
                guard.server_info.clone(),
                guard.responses.clone(),
            )
        };

        let server_info = backend
            .server_info_snapshot()
            .await
            .unwrap_or(old_server_info);
        let capabilities = Capabilities::from_server_info(&server_info);

        let deadline = cache_refresh_deadline();
        let mut responses = old_responses;
        for key in keys.iter().copied() {
            if !capabilities.supports(&key) {
                responses.insert(key, empty_result_for(&key));
            } else {
                let result = query_list_drained(&backend, key, Some(deadline)).await?;
                responses.insert(key, result);
            }
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
            if backend_slot.generation() != backend_generation {
                return Err(
                    "backend generation changed while refresh candidate was building".into(),
                );
            }
            if guard.cache_epoch != old_cache_epoch
                || guard.backend_generation != old_backend_generation
            {
                return Err("cache snapshot changed while refresh candidate was building".into());
            }
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

        Ok(RefreshReport {
            old_cache_epoch,
            new_cache_epoch,
            old_discovery_hash,
            new_discovery_hash,
            changed,
        })
    }
}

/// Build the MCP initialize request.
pub fn build_initialize_request(id: Value) -> Value {
    mcp_interface::build_request(
        id,
        "initialize",
        Some(serde_json::json!({
            "protocolVersion": protocol_version_today(),
            "capabilities": {},
            "clientInfo": {
                "name": "mcp-wrapper-rs",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
    )
}

pub(crate) fn protocol_version_today() -> String {
    let unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    protocol_version_for_unix_seconds_local(unix_seconds)
}

pub(crate) fn protocol_version_for_unix_seconds_local(unix_seconds: i64) -> String {
    let mut local_time = libc::tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 1,
        tm_mon: 0,
        tm_year: 70,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: -1,
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))]
        tm_gmtoff: 0,
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))]
        tm_zone: std::ptr::null_mut(),
    };
    let raw_time = unix_seconds as libc::time_t;
    let ok = unsafe { !libc::localtime_r(&raw_time, &mut local_time).is_null() };
    if ok {
        return format!(
            "{:04}-{:02}-{:02}",
            local_time.tm_year + 1900,
            local_time.tm_mon + 1,
            local_time.tm_mday
        );
    }
    protocol_version_for_unix_days(unix_seconds / 86_400)
}

pub(crate) fn protocol_version_for_unix_days(unix_days: i64) -> String {
    let (year, month, day) = civil_from_unix_days(unix_days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_unix_days(unix_days: i64) -> (i64, u32, u32) {
    let z = unix_days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month as u32, day as u32)
}

fn build_list_request(id: Value, method: &str, cursor: Option<String>) -> Value {
    let params = cursor.map(|cursor| serde_json::json!({ "cursor": cursor }));
    mcp_interface::build_request(id, method, params)
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

    query_list_drained_or_empty(backend, key).await
}

pub async fn query_list_drained_or_empty(backend: &Backend, key: McpDataKey) -> Value {
    query_list_drained(backend, key, None)
        .await
        .unwrap_or_else(|_| empty_result_for(&key))
}

pub fn cache_refresh_deadline() -> Instant {
    Instant::now() + CACHE_REFRESH_TIMEOUT
}

async fn query_list_drained(
    backend: &Backend,
    key: McpDataKey,
    deadline: Option<Instant>,
) -> Result<Value, String> {
    let method = mcp_interface::method_for_mcp_data_key(&key);
    let field = list_field_for(&key);
    let mut merged = empty_result_for(&key);
    let mut cursor: Option<String> = None;
    for page_index in 0..timeouts::CACHE_PAGINATION_MAX_PAGES {
        let request_timeout = match deadline {
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(remaining) if remaining > Duration::ZERO => remaining.min(LIST_QUERY_TIMEOUT),
                _ => {
                    warn!(
                        method = method,
                        page_index = page_index,
                        "mcp_manager: cache refresh timeout"
                    );
                    return Err(format!("{method}: cache refresh deadline exceeded"));
                }
            },
            None => LIST_QUERY_TIMEOUT,
        };
        let req = build_list_request(backend.next_request_id(), method, cursor.clone());
        let result = match backend
            .send_request_with_timeout(req, request_timeout)
            .await
        {
            Ok(resp) => validate_list_response(&resp, field, method, page_index)?,
            Err(e) => {
                warn!(
                    method = method,
                    page_index = page_index,
                    err = %e,
                    "mcp_manager: list query failed"
                );
                return Err(format!("{method}: list query failed: {e}"));
            }
        };

        append_page_items(&mut merged, &result, field);
        cursor = result
            .get("nextCursor")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        if cursor.is_none() {
            return Ok(merged);
        }
    }

    warn!(
        method = method,
        max_pages = timeouts::CACHE_PAGINATION_MAX_PAGES,
        page_index = timeouts::CACHE_PAGINATION_MAX_PAGES - 1,
        "mcp_manager: pagination drain cap reached"
    );
    Err(format!("{method}: pagination drain cap reached"))
}

pub(crate) fn validate_list_response(
    response: &Value,
    field: &str,
    method: &str,
    page_index: usize,
) -> Result<Value, String> {
    if let Some(error) = response.get("error") {
        return Err(format!(
            "{method}: backend error on page {page_index}: {error}"
        ));
    }
    let result = response
        .get("result")
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| format!("{method}: missing object result on page {page_index}"))?;
    if !result.get(field).is_some_and(Value::is_array) {
        return Err(format!(
            "{method}: result.{field} is not an array on page {page_index}"
        ));
    }
    if result
        .get("nextCursor")
        .is_some_and(|cursor| !cursor.is_null() && !cursor.is_string())
    {
        return Err(format!(
            "{method}: result.nextCursor is not a string on page {page_index}"
        ));
    }
    Ok(result)
}

pub fn cached_list_request_has_cursor(key: &McpDataKey, raw: &Value) -> bool {
    !matches!(key, McpDataKey::Initialize)
        && raw
            .get("params")
            .and_then(|params| params.get("cursor"))
            .map(|cursor| !cursor.is_null())
            .unwrap_or(false)
}

fn list_field_for(key: &McpDataKey) -> &'static str {
    match key {
        McpDataKey::ToolsList => "tools",
        McpDataKey::PromptsList => "prompts",
        McpDataKey::ResourcesList => "resources",
        McpDataKey::ResourceTemplatesList => "resourceTemplates",
        McpDataKey::Initialize => "",
    }
}

fn append_page_items(merged: &mut Value, page: &Value, field: &str) {
    if field.is_empty() {
        return;
    }
    if !merged
        .get(field)
        .map(|value| value.is_array())
        .unwrap_or(false)
    {
        merged[field] = serde_json::json!([]);
    }
    let Some(target) = merged.get_mut(field).and_then(|value| value.as_array_mut()) else {
        return;
    };
    if let Some(items) = page.get(field).and_then(|value| value.as_array()) {
        target.extend(items.iter().cloned());
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
