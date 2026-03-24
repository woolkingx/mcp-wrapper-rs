//! Cache store and initialization sequence.
//!
//! At startup, spawns a temporary backend to query the real MCP server's
//! capabilities and list/* responses. Serves cached results instantly;
//! supports invalidation via backend notifications.
//!
//! Cache scope is bounded by the server's declared capabilities from the
//! MCP initialize handshake. Only capabilities the server advertises are
//! queried; unsupported ones get spec-compliant empty results.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::proxy::{Backend, ChildPgids};
use crate::router::CacheKey;
use crate::transport;

/// Timeout for individual list/* queries during init.
/// Separate from --init-timeout (which covers the initialize handshake).
/// List queries target a local subprocess that already completed init,
/// so 5 seconds is generous.
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
    /// Parse from the `capabilities` field of an InitializeResult.
    fn from_server_info(server_info: &Value) -> Self {
        let caps = server_info.get("capabilities").unwrap_or(&Value::Null);
        Self {
            tools: caps.get("tools").is_some(),
            prompts: caps.get("prompts").is_some(),
            resources: caps.get("resources").is_some(),
        }
    }

    /// Whether the server supports the given cache key's capability.
    pub fn supports(&self, key: &CacheKey) -> bool {
        match key {
            CacheKey::Initialize => true,
            CacheKey::ToolsList => self.tools,
            CacheKey::PromptsList => self.prompts,
            CacheKey::ResourcesList | CacheKey::ResourceTemplatesList => self.resources,
        }
    }
}

/// Raw cached responses from the backend MCP server.
struct CachedData {
    responses: HashMap<CacheKey, Value>,
    server_info: Value,
    capabilities: Capabilities,
}

/// Thread-safe cache wrapper. Uses std::sync::RwLock because reads are fast
/// and never cross await points.
pub struct Cache {
    data: std::sync::RwLock<CachedData>,
}

impl Cache {
    fn new(data: CachedData) -> Self {
        Self {
            data: std::sync::RwLock::new(data),
        }
    }

    /// Read a cached response for the given key. Returns cloned JSON value.
    /// For CacheKey::Initialize, returns the full server_info.
    pub fn lookup(&self, key: &CacheKey) -> Option<Value> {
        let guard = self.data.read().unwrap();
        if *key == CacheKey::Initialize {
            return Some(guard.server_info.clone());
        }
        guard.responses.get(key).cloned()
    }

    /// Replace a cached entry (used on list_changed invalidation + refresh).
    /// Ignored for capabilities the server doesn't support.
    pub fn update(&self, key: &CacheKey, value: Value) {
        let mut guard = self.data.write().unwrap();
        if *key == CacheKey::Initialize {
            guard.server_info = value;
        } else if guard.capabilities.supports(key) {
            guard.responses.insert(*key, value);
        }
    }

}

/// Build the MCP initialize request.
pub fn build_initialize_request(id: Value) -> Value {
    transport::build_request(
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

/// Build a list/* request for any cacheable method.
fn build_list_request(id: Value, method: &str) -> Value {
    transport::build_request(id, method, None)
}

/// Spec-compliant empty results for unsupported capabilities.
fn empty_result_for(key: &CacheKey) -> Value {
    match key {
        CacheKey::ToolsList => serde_json::json!({"tools": []}),
        CacheKey::PromptsList => serde_json::json!({"prompts": []}),
        CacheKey::ResourcesList => serde_json::json!({"resources": []}),
        CacheKey::ResourceTemplatesList => serde_json::json!({"resourceTemplates": []}),
        CacheKey::Initialize => Value::Object(serde_json::Map::new()),
    }
}

/// Spawn a temporary backend, run the MCP initialize handshake, query
/// supported list methods, and return a populated Cache.
///
/// Only capabilities declared in the server's InitializeResult are queried.
/// Unsupported capabilities get spec-compliant empty results immediately.
/// Each list query uses a short timeout (LIST_QUERY_TIMEOUT) — these target
/// a local subprocess that already completed init.
pub async fn init_cache(
    cmd: &str,
    args: &[String],
    init_timeout: Duration,
    child_pgids: &ChildPgids,
) -> Result<Cache, Box<dyn std::error::Error>> {
    // Notification channel — we discard backend notifications during init
    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while notif_rx.recv().await.is_some() {}
    });

    let backend = Backend::spawn(cmd, args, notif_tx, child_pgids)?;
    debug!("init_cache: backend spawned");

    // --- Initialize handshake ---
    let init_req = build_initialize_request(backend.next_request_id());
    let init_resp = tokio::time::timeout(init_timeout, backend.send_request(init_req))
        .await
        .map_err(|_| "initialize handshake timeout")??;

    let server_info = init_resp
        .get("result")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    let capabilities = Capabilities::from_server_info(&server_info);
    info!(
        tools = capabilities.tools,
        prompts = capabilities.prompts,
        resources = capabilities.resources,
        "init_cache: server capabilities"
    );

    // Send initialized notification to complete the handshake
    let initialized_notif = transport::build_notification("notifications/initialized", None);
    backend.send_notification(&initialized_notif).await?;
    debug!("init_cache: handshake complete");

    // --- Query only supported list methods ---
    let list_keys = [
        CacheKey::ToolsList,
        CacheKey::PromptsList,
        CacheKey::ResourcesList,
        CacheKey::ResourceTemplatesList,
    ];

    let mut responses = HashMap::new();
    for key in &list_keys {
        if !capabilities.supports(key) {
            debug!(method = crate::router::list_method_for_key(key), "init_cache: skipped (not in capabilities)");
            responses.insert(*key, empty_result_for(key));
            continue;
        }

        let method = crate::router::list_method_for_key(key);
        let req = build_list_request(backend.next_request_id(), method);

        let result = match tokio::time::timeout(LIST_QUERY_TIMEOUT, backend.send_request(req)).await {
            Ok(Ok(resp)) => resp
                .get("result")
                .cloned()
                .unwrap_or(empty_result_for(key)),
            Ok(Err(e)) => {
                warn!(method = method, err = %e, "init_cache: list query failed");
                empty_result_for(key)
            }
            Err(_) => {
                warn!(method = method, "init_cache: list query timeout");
                empty_result_for(key)
            }
        };
        responses.insert(*key, result);
    }

    // Log what we cached
    let tool_count = responses
        .get(&CacheKey::ToolsList)
        .and_then(|v| v.get("tools"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let prompt_count = responses
        .get(&CacheKey::PromptsList)
        .and_then(|v| v.get("prompts"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let resource_count = responses
        .get(&CacheKey::ResourcesList)
        .and_then(|v| v.get("resources"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let template_count = responses
        .get(&CacheKey::ResourceTemplatesList)
        .and_then(|v| v.get("resourceTemplates"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    info!(
        tools = tool_count,
        prompts = prompt_count,
        resources = resource_count,
        resource_templates = template_count,
        "init_cache: cached"
    );

    // Kill the temporary init backend
    backend.kill().await;

    Ok(Cache::new(CachedData {
        responses,
        server_info,
        capabilities,
    }))
}
