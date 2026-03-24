/// Method dispatch: maps MCP JSON-RPC method strings to routing decisions.
/// Pure functions only — no state, no I/O, no async.

/// Which cache slot to read from or invalidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheKey {
    Initialize,
    ToolsList,
    PromptsList,
    ResourcesList,
    ResourceTemplatesList,
}

/// Where to send an incoming request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Serve from cached response.
    Cache(CacheKey),
    /// Forward raw JSON to the backend subprocess.
    PassThrough,
    /// Handle locally without backend (e.g., ping).
    Local,
}

/// Decide how to handle an incoming MCP method.
pub fn route(method: &str) -> Route {
    match method {
        "initialize" => Route::Cache(CacheKey::Initialize),
        "tools/list" => Route::Cache(CacheKey::ToolsList),
        "prompts/list" => Route::Cache(CacheKey::PromptsList),
        "resources/list" => Route::Cache(CacheKey::ResourcesList),
        "resources/templates/list" => Route::Cache(CacheKey::ResourceTemplatesList),
        "ping" => Route::Local,
        // tools/call, resources/read, prompts/get, completion/complete,
        // logging/setLevel, resources/subscribe, resources/unsubscribe,
        // and any unknown/future methods — all forwarded.
        _ => Route::PassThrough,
    }
}

/// Detect cache invalidation notifications from the backend.
/// Returns the cache key that should be refreshed, if applicable.
pub fn is_list_changed_notification(method: &str) -> Option<CacheKey> {
    match method {
        "notifications/tools/list_changed" => Some(CacheKey::ToolsList),
        "notifications/prompts/list_changed" => Some(CacheKey::PromptsList),
        "notifications/resources/list_changed" => Some(CacheKey::ResourcesList),
        _ => None,
    }
}

/// Reverse mapping: which MCP method to call to re-populate a cache slot.
pub fn list_method_for_key(key: &CacheKey) -> &'static str {
    match key {
        CacheKey::Initialize => "initialize",
        CacheKey::ToolsList => "tools/list",
        CacheKey::PromptsList => "prompts/list",
        CacheKey::ResourcesList => "resources/list",
        CacheKey::ResourceTemplatesList => "resources/templates/list",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_methods() {
        assert_eq!(route("initialize"), Route::Cache(CacheKey::Initialize));
        assert_eq!(route("tools/list"), Route::Cache(CacheKey::ToolsList));
        assert_eq!(route("prompts/list"), Route::Cache(CacheKey::PromptsList));
        assert_eq!(route("resources/list"), Route::Cache(CacheKey::ResourcesList));
        assert_eq!(
            route("resources/templates/list"),
            Route::Cache(CacheKey::ResourceTemplatesList)
        );
    }

    #[test]
    fn local_methods() {
        assert_eq!(route("ping"), Route::Local);
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
        // Unknown future methods also pass through
        assert_eq!(route("some/future/method"), Route::PassThrough);
    }

    #[test]
    fn list_changed_notifications() {
        assert_eq!(
            is_list_changed_notification("notifications/tools/list_changed"),
            Some(CacheKey::ToolsList)
        );
        assert_eq!(
            is_list_changed_notification("notifications/prompts/list_changed"),
            Some(CacheKey::PromptsList)
        );
        assert_eq!(
            is_list_changed_notification("notifications/resources/list_changed"),
            Some(CacheKey::ResourcesList)
        );
        assert_eq!(
            is_list_changed_notification("notifications/other"),
            None
        );
        assert_eq!(is_list_changed_notification("tools/call"), None);
    }

    #[test]
    fn roundtrip_cache_keys() {
        let keys = [
            CacheKey::Initialize,
            CacheKey::ToolsList,
            CacheKey::PromptsList,
            CacheKey::ResourcesList,
            CacheKey::ResourceTemplatesList,
        ];
        for key in &keys {
            let method = list_method_for_key(key);
            assert_eq!(route(method), Route::Cache(*key));
        }
    }
}
