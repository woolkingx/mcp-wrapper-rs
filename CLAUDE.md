# mcp-wrapper-rs Project Memory

## Critical Rules (Read First)

1. **MUST validate CLI arguments before entering main loop**
   - *WHY: Prevents silent hangs when users provide invalid flags*
   - Pattern: Check for `-` prefix → match known flags → error on unknown → fallback to command mode

2. **ALWAYS use `cargo build --release` for production binaries**
   - *WHY: Debug builds are 10x larger and much slower*
   - Size: Release ~1.6MB (rmcp SDK + dependencies), Debug ~10MB+

3. **REQUIRED: Test with real MCP servers before releasing**
   - *WHY: Subprocess timing is environment-specific*
   - Test matrix: npx (searxng, fetcher), Python servers, shell scripts

4. **MUST diff-review all agent-generated code before committing**
   - *WHY: Agents (haiku/sub-agents) add unplanned changes that break functionality*
   - Pattern: `git diff` → verify only planned changes exist → revert unplanned additions

5. **Language Policy: All code and docs MUST be in English**
   - *WHY: Public GitHub project for global open-source community*
   - Exception: Localized docs with language suffix (`README-zh.md`, `ARCHITECTURE-ja.md`)

## Architecture Overview

```
Client → [stdin/stdout] → rmcp serve_server(McpProxy) → ServerHandler trait
                                   ↓ (on tools/call)
                          McpProxy.ensure_backend()
                                   ↓
                          rmcp serve_client(TokioChildProcess) → real MCP server
```

### Key Design: McpProxy (ServerHandler)

- **McpProxy** struct implements `rmcp::ServerHandler` trait
- On startup: spawns subprocess via rmcp client, caches tools/prompts/resources/resource_templates, kills init subprocess
- On serve: rmcp handles full JSON-RPC protocol on stdin/stdout
- On tool call: lazy-spawns persistent backend subprocess, reuses for subsequent calls
- Backend auto-recovers if subprocess dies (`is_closed()` check → re-spawn)

### Key Files
```
src/
└── main.rs                   # Everything: CLI, McpProxy, ServerHandler impl

tests/
├── behavioral.rs             # Integration tests: spawn wrapper + Python echo/slow servers
├── conformance.rs            # MCP schema conformance tests (validates JSON-RPC responses)
├── schema_driven.rs          # SDD tests: derived from mcp-proxy-tests.json x-tests tags
├── support/                  # schema2object (Rust) copied from projects/schema2object/rust/src/
│   ├── mod.rs                # Module root (uses super:: not crate::)
│   └── *.rs                  # ObjectTree, validate, compose, defaults, error
└── fixtures/
    ├── mcp-schema.json       # MCP official schema 2025-03-26 (83 definitions, 89KB)
    └── mcp-proxy-tests.json  # SDD companion schema: x-methods + x-tests for proxy behaviors

Cargo.toml                    # Dependencies: rmcp, tokio, tracing, tracing-subscriber, tracing-appender
target/release/               # Compiled binary (~2.2MB with strip=true)
```

### Memory Model
- **Startup**: Spawn once → rmcp client handshake → `list_all_tools/prompts/resources` (parallel, 5s list_timeout each) → cache → **keep client alive as first backend**
- **Runtime**: Serve `initialize`/`tools/list`/`prompts/list`/`resources/list` from cache (instant, rmcp handles id/protocol)
- **Tool execution**: `ensure_backend()` → clone `Peer` → release lock → `peer.call_tool()` outside lock → reuse for subsequent calls
- **Idle reaper**: AtomicU64 timestamp, sleep-loop every 60s, kills backend only when `active_calls == 0`; loop runs for process lifetime

### Concurrency Design (rmcp internals)
- `Peer` contains only `mpsc::Sender` — `clone()` is cheap (Arc-based)
- `peer.call_tool()` sends request into `mpsc::channel(1024)`, awaits `oneshot` response
- rmcp event loop matches responses by JSON-RPC `id` — **full concurrent multiplexing on one connection**
- **No connection pool needed**: multiple concurrent `call_tool` callers each clone `Peer`, all in-flight simultaneously
- Mutex held only during `ensure_backend()` to clone `Peer` — NOT during the actual call

### BackendState Machine
```
Empty → Spawning(Notify) → Ready(RunningService, stderr_buf)
          ↑ concurrent callers wait on Notify
          ↑ on spawn failure: reset to Empty + notify_waiters (prevent hang)
```

## Key Commands

### Development
```bash
# Build for development (unoptimized, includes debug symbols)
cargo build

# Build for release (optimized, ~1.6MB)
cargo build --release

# Install to ~/.cargo/bin/
cargo install --path .

# Run tests
cargo test

# Check without building
cargo check
```

### Testing
```bash
# Test version flag (should not hang)
./target/release/mcp-wrapper-rs --version

# Test help output
./target/release/mcp-wrapper-rs --help

# Test unknown flag handling
./target/release/mcp-wrapper-rs --unknown 2>&1 | grep "Error"

# Test with real MCP server (stdio proxy — pipe JSON directly to wrapper's stdin)
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}' \
  | ./target/release/mcp-wrapper-rs npx -y mcp-searxng
```

### Debug Logging
```bash
# Enable INFO-level logs
MCP_WRAPPER_DEBUG=1 mcp-wrapper-rs npx -y mcp-searxng

# Enable DEBUG-level logs (most verbose)
MCP_WRAPPER_DEBUG=debug mcp-wrapper-rs npx -y mcp-searxng

# Log file naming: mcp-wrapper-{name}-{hash8}.log
# hash8 = first 8 hex chars of hash(cmd + args), guarantees uniqueness
# Override with: MCP_SERVER_NAME=my-server → mcp-wrapper-my-server.log

# View logs (Linux with XDG runtime dir)
tail -f /run/user/1000/mcp-wrapper/mcp-wrapper-mcp-searxng-*.log
```

## Development Guidelines

### Adding New Features

1. **CLI Arguments**
   - Add flag matching in `main()` before runtime creation
   - Pattern: `args[i].starts_with("-")` → `match args[i]` → consume value if needed → error on unknown
   - Current flags: `--init-timeout <secs>` (default 5), `--version`, `--help`

2. **New ServerHandler methods**
   - Override in `impl ServerHandler for McpProxy`
   - For cached responses: return from struct field
   - For forwarded requests: use `ensure_backend()` → `peer.method()`

3. **Cache Invalidation**
   - Currently: Never invalidated (assumes static tool definitions)
   - Future: Add `tools/listChanged` notification support

### Code Style

- Use Rust 2021 idioms
- rmcp handles all JSON-RPC protocol details — never hand-parse JSON-RPC
- Error handling: Return `Result<T, ErrorData>` for ServerHandler methods
- Logging: Use `tracing` macros (`info!`, `debug!`, `warn!`) — never hand-rolled `log()` or `println!`
- Comments: Explain WHY, not WHAT
- Version comes from `env!("CARGO_PKG_VERSION")` — single source of truth in Cargo.toml

### Performance Constraints

| Metric | Target | Current |
|--------|--------|---------|
| Binary size | — | ~2.2MB |
| Startup time | <100ms | ~50ms ✓ |
| Memory (idle) | <5MB | ~3MB ✓ |
| Tool call latency | +0ms overhead | Meets target ✓ |

### Common Pitfalls

1. **Subprocess Cleanup**
   - rmcp's `TokioChildProcess` handles kill-on-drop via `process-wrap` crate
   - No manual `child.kill()` + `child.wait()` needed

2. **Multi-threaded Runtime**
   - rmcp's `serve_server` requires `Send + Sync + 'static` on handlers
   - MUST use `tokio::sync::Mutex` (not `std::sync::Mutex`) — lock guard spans `.await` points
   - Runtime: `new_multi_thread()` — required by rmcp's task spawning

3. **Backend Connection**
   - `ensure_backend()` uses `is_closed()` to detect dead connections
   - Always clone `Peer` before releasing the mutex guard
   - `BackendState::Spawning` error path MUST reset to `Empty` + `notify_waiters()` — otherwise concurrent callers hang

4. **Idle Reaper**
   - MUST NOT `break` after killing backend — reaper loop runs for process lifetime
   - Use `AtomicU64` timestamp (not `Notify`) — `Notify` is edge-triggered and loses wakeups under burst
   - `ActiveCallGuard` (RAII, AtomicUsize) prevents reaper from killing mid-call backend

5. **Hot Path Allocations**
   - Wrap `format!("{:?}", ...)` in `if tracing::enabled!(Level::DEBUG)` — unconditional format allocates even when debug is off
   - Use `v.len()` (key count) for `arg_keys` in info! log — zero allocation

## Testing Strategy (Schema-Driven Development)

Tests derive from schema — not hand-written from memory.

**Three test layers:**
| File | Purpose | Driver |
|------|---------|--------|
| `behavioral.rs` | Black-box process tests (CLI flags, basic MCP flow) | Manual |
| `conformance.rs` | JSON-RPC response conformance against mcp-schema.json | MCP schema |
| `schema_driven.rs` | Proxy-specific behaviors (concurrency, error codes, reaper) | mcp-proxy-tests.json |

**Adding new behavior tests:**
1. Add entry to `tests/fixtures/mcp-proxy-tests.json` under `x-tests` with scenario tags
2. Implement test in `schema_driven.rs` matching that tag
3. Schema = spec; test = derived proof

**Standard scenario tags:** `null`, `missing`, `empty`, `min_boundary`, `max_boundary`, `valid_normal`, `valid_edge`, `custom:<description>`

## Testing Checklist

Before releasing a new version:

- [ ] `cargo build --release` succeeds
- [ ] `mcp-wrapper-rs --version` shows correct version
- [ ] `mcp-wrapper-rs --help` displays usage
- [ ] `mcp-wrapper-rs --unknown` shows error + help
- [ ] `mcp-wrapper-rs` (no args) shows error + help
- [ ] Wraps npx-based MCP (test with mcp-searxng)
- [ ] Wraps Python MCP (test with custom server.py)
- [ ] Wraps shell script MCP (test with run_server.sh)
- [ ] Debug logging works (`MCP_WRAPPER_DEBUG=1`)
- [ ] Consecutive tool calls reuse persistent backend (check log for single "spawning" message)
- [ ] Update version in `Cargo.toml` (main.rs reads from there via `env!()`)

## Version History

- **0.2.0** (2026-02-25): Async concurrency overhaul + MCP compliance fixes. BackendState machine, spawn-outside-lock, AtomicU64 reaper, ActiveCallGuard RAII, init backend reuse, tokio::Mutex for stderr, hot-path alloc guards. Capabilities filtering, error code preservation, read_resource/get_prompt/complete forwarding. SDD test suite added.
- **0.2.0** (2026-02-23): Full rewrite using rmcp 0.16 SDK. Persistent backend for tool calls. Pagination support. Protocol handling delegated to rmcp.
- **0.1.3** (2026-02-18): Bug fixes for cache and response ordering.
- **0.1.1** (2026-02-12): Added `--version`, `--help`, unknown flag handling. Fixed silent hang on invalid flags.
- **0.1.0** (2026-01-25): Initial release. Core proxy functionality, subprocess caching, debug logging.

## Reminders

- **Release binaries MUST be stripped** (already configured in `Cargo.toml`)
- **Version is defined ONLY in `Cargo.toml`** — code reads via `env!("CARGO_PKG_VERSION")`
- **CLI behavior MUST follow POSIX conventions**: `-h` short flag, `--help` long flag
- **Error messages go to stderr**, success output to stdout
