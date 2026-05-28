# Architecture

Current architecture truth lives in [docs/handbook/index.html](docs/handbook/index.html).
This file remains a compact legacy note and quick orientation surface.

## Design Philosophy

**Unix Philosophy**: Do one thing well — transparent protocol proxying with intelligent caching.

The wrapper follows a simple principle: **cache what's static, proxy what's dynamic**.

Handles raw JSON-RPC 2.0 directly — no SDK dependency for protocol handling.

## System Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                        mcp-wrapper-rs                            │
│                                                                  │
│  ┌─────────┐   ┌────────┐   ┌───────────────────────────┐      │
│  │  Stdin  │──▶│ Router │──▶│ Cached     → Cache        │      │
│  │(JSON-RPC)│   │        │   │ PassThru   → Backend  ────│──▶ MCP Server
│  └─────────┘   └────────┘   │ Notify     → relay        │   (on-demand)
│                              └───────────────────────────┘      │
│  ┌─────────┐                                                     │
│  │ Stdout  │◀── mcp_interface.rs writes JSON-RPC responses ─────│
│  │(JSON-RPC)│                                                     │
│  └─────────┘                                                     │
└──────────────────────────────────────────────────────────────────┘
```

## Modules

| module | responsibility |
|--------|---------------|
| `mcp_interface.rs` | Line-delimited JSON-RPC read/write, message builders, method route classification |
| `mcp_manager.rs` | Initialize handshake, capability-aware discovery cache, cache refresh |
| `backend_manager.rs` | Backend lifecycle: lazy spawn, health check, request/response matching via oneshot channels |
| `runtime_manager.rs` | Normal stdio loop, request dispatch, idle reaper, peer-response forwarding |
| `daemon_manager.rs` / `broker_manager.rs` | Daemon client relay and shared broker runtime |
| `main.rs` | CLI parsing and top-level dispatch |

## Request Flow

### Cached Requests

```
Client                  Wrapper
  │                        │
  │─── initialize ────────▶│ → returns merged capabilities (instant)
  │◀── response ───────────│
  │                        │
  │─── tools/list ────────▶│ → returns cached tools (instant)
  │◀── response ───────────│
```

**Latency**: < 1ms (served from in-memory cache)

### Pass-Through Requests

```
Client                  Wrapper                  Backend (persistent)
  │                        │                        │
  │─── tools/call ────────▶│                        │
  │                        │── ensure_backend() ──▶│ (spawns if needed)
  │                        │── forward request ───▶│ (ID remapped)
  │                        │◀── response ──────────│ (ID restored)
  │◀── response ───────────│                        │ (stays alive)
  │                        │                        │
  │─── tools/call ────────▶│── forward request ───▶│ (reuses backend)
  │                        │◀── response ──────────│
  │◀── response ───────────│                        │
```

### Notification Relay

Bidirectional: backend notifications forwarded to client, client notifications forwarded to backend.

Cache invalidation notifications (`tools/list_changed`, `prompts/list_changed`, `resources/list_changed`) trigger automatic cache refresh.

## Initialization Sequence

```
Wrapper                              Subprocess (init, temporary)
   │                                     │
   │─── spawn process ─────────────────▶│
   │─── initialize request ────────────▶│ (raw JSON-RPC handshake)
   │◀── initialize response ───────────│
   │─── initialized notification ─────▶│
   │                                     │
   │─── tools/list ───────────────────▶│ (init_timeout applies)
   │◀── tools response ───────────────│
   │─── prompts/list ─────────────────▶│
   │◀── prompts response ─────────────│
   │─── resources/list ───────────────▶│
   │◀── resources response ────────────│
   │─── resources/templates/list ─────▶│
   │◀── templates response ────────────│
   │                                     │
   │─── kill process ──────────────────▶✗
   │
   ▼ (cache populated, ready to serve on stdio)
```

## Backend Lifecycle

- **Lazy spawn**: Backend created on first pass-through request
- **Retry**: 3 attempts with exponential backoff (1s, 2s, 4s)
- **Full handshake**: Each spawn performs MCP initialize + initialized sequence
- **Health check**: Verify child process alive before forwarding
- **Graceful shutdown**: SIGTERM → 5s wait → SIGKILL
- **Process groups**: Each child in own process group via `process-wrap`; `child_pgids` registry for cleanup

## Error Handling

| Scenario | Behavior |
|----------|----------|
| Init subprocess fails | Exit with error message |
| Backend spawn fails (after retries) | JSON-RPC error response to client |
| Backend dies mid-session | Auto re-spawn on next request |
| Pass-through call fails | JSON-RPC error propagated to client |
| Malformed JSON from backend | Logged and skipped |

## Limitations

1. **No streaming**: Responses collected before returning
2. **Single backend**: One persistent subprocess for all calls

## Future Improvements

- [ ] Connection pooling for high-throughput scenarios
- [x] Configurable init timeout (`--init-timeout`, default 30s)
- [x] Cache invalidation via `listChanged` notifications
- [x] Bidirectional notification relay
