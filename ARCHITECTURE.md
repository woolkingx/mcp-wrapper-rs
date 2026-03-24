# Architecture

## Design Philosophy

**Unix Philosophy**: Do one thing well - transparent protocol proxying with intelligent caching.

The wrapper follows a simple principle: **cache what's static, proxy what's dynamic**.

## System Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                        mcp-wrapper-rs                            │
│                                                                  │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────────────┐  │
│  │   Stdin     │───▶│   Router    │───▶│   Cache (static)    │  │
│  │  (JSON-RPC) │    │             │    │   - server_info     │  │
│  └─────────────┘    │             │    │   - capabilities    │  │
│                     │             │    │   - tools[]         │  │
│                     │             │    │   - prompts[]       │  │
│                     │             │    │   - resources[]     │  │
│                     │             │    └─────────────────────┘  │
│                     │             │                              │
│                     │             │    ┌─────────────────────┐  │
│                     │             │───▶│ Subprocess (dynamic)│  │
│                     │             │    │   - tools/call      │  │
│                     └─────────────┘    │   - spawn → exec →  │  │
│                                        │     response → kill │  │
│  ┌─────────────┐                       └─────────────────────┘  │
│  │   Stdout    │◀────────────────────────────────────────────── │
│  │  (JSON-RPC) │                                                 │
│  └─────────────┘                                                 │
└──────────────────────────────────────────────────────────────────┘
```

## Request Flow

### Static Requests (Cached)

```
Client                  Wrapper                 Subprocess
  │                        │                        │
  │─── initialize ────────▶│                        │
  │◀── cached response ────│                        │
  │                        │                        │
  │─── tools/list ────────▶│                        │
  │◀── cached response ────│                        │
```

**Latency**: < 1ms

### Dynamic Requests (Proxied)

```
Client                  Wrapper                 Subprocess
  │                        │                        │
  │─── tools/call ────────▶│                        │
  │                        │─── spawn ─────────────▶│
  │                        │─── initialize ────────▶│
  │                        │◀── response ───────────│
  │                        │─── notification ──────▶│
  │                        │─── tools/call ────────▶│
  │                        │◀── response ───────────│
  │                        │─── kill ──────────────▶│
  │◀── response ───────────│                        ✗
```

**Latency**: Same as direct subprocess call

## Data Structures

### McpCache

```rust
struct McpCache {
    server_info: Value,   // From initialize response
    capabilities: Value,  // From initialize response
    tools: Value,         // From tools/list response
    prompts: Value,       // From prompts/list response
    resources: Value,     // From resources/list response
}
```

### Request/Response

```rust
struct Request {
    method: Option<String>,
    id: Option<Value>,      // None = notification (no response needed)
    params: Option<Value>,
}

struct Response {
    jsonrpc: &'static str,  // Always "2.0"
    id: Value,
    result: Option<Value>,  // Success
    error: Option<Value>,   // Error
}
```

## Initialization Sequence

At startup, wrapper spawns subprocess once to populate cache:

```
Wrapper                              Subprocess
   │                                     │
   │─── spawn ──────────────────────────▶│
   │                                     │
   │─── {"id":0,"method":"initialize"}──▶│
   │─── {"method":"notifications/init"}─▶│
   │─── {"id":1,"method":"tools/list"}──▶│
   │─── {"id":2,"method":"prompts/list"}▶│
   │─── {"id":3,"method":"resources/list"}▶│
   │                                     │
   │◀── {"id":0,"result":{...}} ─────────│
   │◀── {"id":1,"result":{tools:[...]}}──│
   │◀── {"id":2,"result":{prompts:[...]}}│
   │◀── {"id":3,"result":{resources:[]}}─│
   │                                     │
   │─── kill ───────────────────────────▶✗
   │
   ▼ (cache populated, ready to serve)
```

## Subprocess Management

### Spawning

```rust
Command::new(cmd)
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())  // Discard stderr to prevent pollution
    .spawn()
```

### Response Collection

The wrapper waits for a specific number of responses:

```rust
fn run_subprocess(..., expected_responses: usize) -> Vec<Value> {
    // Write all requests at once
    stdin.write_all(requests.as_bytes());
    stdin.flush();

    // Read responses until:
    // 1. Got expected number of responses with `id` field, OR
    // 2. Timeout (60 seconds)
    for line in reader.lines() {
        if elapsed > timeout { break; }
        if response.has("id") {
            responses.push(response);
            if responses.len() >= expected_responses { break; }
        }
        // Skip notifications (no id field)
    }

    // Cleanup
    child.kill();
    child.wait();
}
```

### Why Not Close Stdin Early?

Some MCP servers (especially npx-based) exit immediately when stdin closes. We keep stdin open and use explicit kill for reliable termination.

## Method Routing

```rust
match method.as_str() {
    "initialize"     => respond_from_cache(),
    "tools/list"     => respond_from_cache(),
    "prompts/list"   => respond_from_cache(),
    "resources/list" => respond_from_cache(),
    "tools/call"     => spawn_subprocess_and_proxy(),
    _                => error_method_not_found(),
}
```

## Error Handling

| Scenario | Behavior |
|----------|----------|
| Subprocess spawn fails | Return empty response |
| Subprocess timeout | Kill and return error |
| Invalid JSON from subprocess | Skip line, continue |
| Subprocess dies mid-call | Return error response |

## Limitations

1. **No streaming**: Responses are collected before returning
2. **No subscriptions**: `listChanged` notifications not supported
3. **Cache invalidation**: Tools list cached at startup only
4. **Single subprocess**: No connection pooling

## Future Improvements

- [ ] Configurable timeout
- [ ] Optional subprocess pooling for high-frequency calls
- [ ] Cache refresh on demand
- [ ] Metrics/statistics endpoint
