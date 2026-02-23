# Architecture

## Design Philosophy

**Unix Philosophy**: Do one thing well - transparent protocol proxying with intelligent caching.

The wrapper follows a simple principle: **cache what's static, proxy what's dynamic**.

Built on [rmcp](https://crates.io/crates/rmcp) 0.16 (official Rust MCP SDK), which handles all JSON-RPC protocol details.

## System Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                        mcp-wrapper-rs                            │
│                                                                  │
│  ┌─────────────┐    ┌──────────────────────────────────────┐    │
│  │   Stdin     │───▶│   McpProxy (ServerHandler trait)     │    │
│  │  (JSON-RPC) │    │                                      │    │
│  └─────────────┘    │   initialize → cached ServerInfo     │    │
│                     │   tools/list → cached ListToolsResult │    │
│                     │   prompts/list → cached               │    │
│                     │   resources/list → cached              │    │
│                     │   resources/templates/list → cached    │    │
│                     │                                      │    │
│                     │   tools/call → ensure_backend() ─────│──▶ MCP Server
│                     │              → peer.call_tool()      │    (persistent)
│                     └──────────────────────────────────────┘    │
│                                                                  │
│  ┌─────────────┐                                                 │
│  │   Stdout    │◀── rmcp handles response routing ──────────────│
│  │  (JSON-RPC) │                                                 │
│  └─────────────┘                                                 │
└──────────────────────────────────────────────────────────────────┘
```

## Request Flow

### Static Requests (Cached)

```
Client                  Wrapper (McpProxy)
  │                        │
  │─── initialize ────────▶│ → returns cached ServerInfo (instant)
  │◀── response ───────────│
  │                        │
  │─── tools/list ────────▶│ → returns cached ListToolsResult (instant)
  │◀── response ───────────│
```

**Latency**: < 1ms (rmcp handles serialization and id mapping)

### Dynamic Requests (Persistent Backend)

```
Client                  Wrapper                  Backend (persistent)
  │                        │                        │
  │─── tools/call ────────▶│                        │
  │                        │── ensure_backend() ──▶│ (spawns if needed)
  │                        │   rmcp client init    │
  │                        │── peer.call_tool() ──▶│
  │                        │◀── CallToolResult ────│
  │◀── response ───────────│                        │ (stays alive)
  │                        │                        │
  │─── tools/call ────────▶│── peer.call_tool() ──▶│ (reuses connection)
  │                        │◀── CallToolResult ────│
  │◀── response ───────────│                        │
```

**Latency**: Same as direct subprocess call (no re-init overhead after first call)

## Data Structures

### McpProxy

```rust
struct McpProxy {
    cmd: String,
    cmd_args: Vec<String>,
    // Cached from init phase
    cached_tools: ListToolsResult,
    cached_prompts: ListPromptsResult,
    cached_resources: ListResourcesResult,
    cached_resource_templates: ListResourceTemplatesResult,
    server_info: ServerInfo,
    // Persistent backend connection (lazy-spawned on first tool call)
    backend: tokio::sync::Mutex<Option<RunningService<RoleClient, ()>>>,
}
```

### ServerHandler Trait

McpProxy implements `rmcp::ServerHandler`, overriding only:
- `get_info()` → cached ServerInfo
- `list_tools()` → cached ListToolsResult
- `list_prompts()` → cached ListPromptsResult
- `list_resources()` → cached ListResourcesResult
- `list_resource_templates()` → cached ListResourceTemplatesResult
- `call_tool()` → forwarded via persistent backend

All other methods (ping, initialize, etc.) use rmcp's default implementations.

## Initialization Sequence

```
Wrapper                              Subprocess (init, temporary)
   │                                     │
   │─── rmcp serve_client() ────────────▶│ (rmcp handles handshake)
   │◀── RunningService<RoleClient> ──────│
   │                                     │
   │─── peer.list_all_tools() ─────────▶│ (handles pagination)
   │◀── Vec<Tool> ──────────────────────│
   │─── peer.list_all_prompts() ───────▶│
   │◀── Vec<Prompt> ────────────────────│
   │─── peer.list_all_resources() ─────▶│
   │◀── Vec<Resource> ──────────────────│
   │─── peer.list_all_resource_templates() ─▶│
   │◀── Vec<ResourceTemplate> ──────────│
   │                                     │
   │─── drop(client) → graceful_shutdown ▶✗
   │
   ▼ (cache populated, ready to serve on stdio)
```

## Backend Connection Management

```rust
async fn ensure_backend(&self) -> Result<Peer<RoleClient>, ErrorData> {
    let mut guard = self.backend.lock().await;
    if let Some(ref running) = *guard {
        if !running.is_closed() {
            return Ok(running.peer().clone()); // Reuse existing
        }
        // Dead connection, will re-spawn below
    }
    // Spawn new subprocess via rmcp client
    let transport = TokioChildProcess::builder(command).stderr(null).spawn()?;
    let running = ().serve(transport).await?; // rmcp handles handshake
    let peer = running.peer().clone();
    *guard = Some(running);
    Ok(peer)
}
```

Key decisions:
- `tokio::sync::Mutex` because lock guard spans `.await` points
- `is_closed()` detects dead subprocess → automatic re-spawn
- `Peer` cloned before releasing lock (cheap Arc-based clone)

## Subprocess Management

rmcp's `TokioChildProcess` handles:
- Process group management via `process-wrap` crate
- Kill-on-drop (no zombie processes)
- Graceful shutdown (close stdin → wait → kill on timeout)
- stderr suppressed via `Stdio::null()`

## Error Handling

| Scenario | Behavior |
|----------|----------|
| Init subprocess fails | Exit with error message |
| Backend spawn fails | Return ErrorData to client |
| Backend dies mid-session | Auto re-spawn on next call |
| Tool call fails | ErrorData propagated to client |
| rmcp protocol error | Handled by rmcp SDK |

## Limitations

1. **No streaming**: Responses collected before returning
2. **No subscriptions**: `listChanged` notifications not supported
3. **Cache invalidation**: Tools list cached at startup only
4. **Single backend**: One persistent subprocess for all tool calls

## Future Improvements

- [ ] Cache refresh on demand
- [ ] `tools/listChanged` notification support
- [ ] Configurable init timeout
