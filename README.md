# mcp-wrapper-rs

A stable MCP supervisor proxy written in Rust. It turns stdio MCP servers into controllable, observable backends by caching stable discovery data, exposing lifecycle control through the unified `mcp.wrapper` tool, and starting or reusing backend subprocesses only when dynamic calls need them.

[中文文檔](README-zh.md)

## The Problem

MCP servers (especially npx-based ones) consume significant memory when running persistently:

```
npx -y mcp-searxng          ~100MB
npx -y fetcher-mcp          ~120MB
npx -y @oevortex/ddg_search ~100MB
python3 server.py           ~50MB
────────────────────────────────────
Total                       ~370MB (idle!)
```

## The Solution

`mcp-wrapper-rs` acts as a stable supervisor proxy:

```
┌─────────────┐      ┌──────────────────────┐      ┌─────────────┐
│ Claude Code │ ──── │ mcp-wrapper-rs       │ ──── │ MCP Server  │
│             │      │ cache + mcp.wrapper  │      │ (on-demand) │
└─────────────┘      └──────────────────────┘      └─────────────┘
```

- **Startup**: Spawns the backend once to cache stable discovery data and exposes wrapper-owned tools capability
- **Runtime**: Serves `initialize` and list requests from cache, including the wrapper-owned `mcp.wrapper` tool
- **Wrapper control**: Handles `mcp.wrapper` actions locally for backend status, ping, refresh, restart, and stop
- **Backend calls**: Keeps dynamic MCP work owned by the backend subprocess, reusing or respawning it as needed

Result: **4 MCP servers using only ~8MB total** (vs ~370MB before)

## Core Concepts

`mcp-wrapper-rs` is a **supervisor proxy**: a stable MCP server the client
connects to, behind which real MCP servers are managed children. The client
talks to one transport that never dies; backend processes start, restart, and
stop without ever breaking that connection.

Four ideas define the product:

- **Cache-first idle economy.** Stable discovery data (`initialize`,
  `tools/list`, `prompts/list`, `resources/list`) is captured once and served
  from memory. Backends are spawned lazily — only when a dynamic call
  (`tools/call`, `resources/read`, …) actually needs them — so an idle wrapper
  costs almost nothing. The externally served `tools/list` / `initialize` views
  are **hold-state**: rebuilt only when backend data loads or refreshes, never
  recomputed per request.

- **One unified control surface: `mcp.wrapper`.** The wrapper injects exactly
  one reserved tool, `mcp.wrapper`, into `tools/list`. Backend status, ping,
  discovery refresh, restart, and stop are all `action`s on this single tool.
  The admin CLI and MCP clients project into the *same* action schema and
  result envelope — there is no second control path. A backend that tries to
  register the reserved name cannot overwrite it.

- **Owner-local lifecycle.** Each piece of state has exactly one owner.
  `BackendSlot` owns the backend process — its alive decision, restart/stop
  transitions, and the active-call safety invariant (stop and non-force restart
  are rejected while calls are in flight). `Cache` owns discovery data and the
  external view lifetime. `tools` is the external entry and dispatch layer; it
  owns the action schema and envelope but holds **no** lifecycle state — it
  routes actions to owner methods. Validation lives on the invocation objects
  themselves, not in a separate validator.

- **Observe → hook → readback control plane.** Backend facts (process exit,
  `listChanged`, degraded cooldown) are signals, not truth. A control action
  requests a legal transition through the owner; the new state is proven by a
  readback snapshot before any client, log, CLI, or tool projection reports it.
  Status is evidence, not a hope attached to a command.

In **daemon mode**, one broker owns a single shared backend/cache pair and
fans out to many client sessions; the active-call safety counter is
broker-global, so no session can stop or restart the shared backend while
another session has work in flight.

### What it fills in over plain stdio MCP

Plain stdio MCP leaves real gaps. This wrapper closes them behind one stable
transport:

| Gap in plain stdio MCP | What mcp-wrapper-rs adds |
|---|---|
| Client cannot control the server it is connected to | `mcp.wrapper` lifecycle actions: `restart` / `stop` / `refresh` / `status` / `ping`, with the transport staying alive across restarts |
| No standard backend-state readback | `observe → hook → readback` control plane: snapshot, generation, cache epoch, discovery hash as proven evidence |
| Idle servers keep consuming memory | Cache-first stable discovery layer + lazy backend spawn |
| Every client spawns its own server | Daemon broker shares one backend/cache pair across sessions |

**Next direction:** a deeper **bidirectional communication** bridge, so wrapped
servers can drive richer two-way (server-to-client) MCP interactions through the
stable wrapper transport.

Full architecture, owner map, flow projections, and proof gates live in the
[handbook](docs/handbook/index.html).

## Installation

### From Source

```bash
git clone https://github.com/woolkingx/mcp-wrapper-rs.git
cd mcp-wrapper-rs
cargo install --path .
```

This will compile in release mode and install the binary to `~/.cargo/bin/mcp-wrapper-rs` (~440KB).

**For development**: Use `cargo build --release` to build without installing. Binary will be at `target/release/mcp-wrapper-rs`.

### Pre-built Binaries

Check [Releases](https://github.com/woolkingx/mcp-wrapper-rs/releases) for pre-built binaries.

## Usage

```bash
mcp-wrapper-rs [options] <command> [args...]
```

### Options

| Flag | Description |
|------|-------------|
| `--init-timeout <secs>` | Seconds to wait for subprocess init handshake (default: 30) |
| `--daemon` | Share a single backend via a broker process (N:1 architecture) |
| `--version`, `-V` | Show version and exit |
| `--help`, `-h` | Show help and exit |

### Examples

```bash
# Wrap an npx-based MCP server
mcp-wrapper-rs npx -y mcp-searxng

# Wrap a Python MCP server
mcp-wrapper-rs python3 /path/to/server.py

# Wrap with environment variables (inherited from parent)
SEARXNG_URL=http://localhost:8080 mcp-wrapper-rs npx -y mcp-searxng

# Use custom init timeout (default: 30s; increase for slow-starting servers)
mcp-wrapper-rs --init-timeout 15 npx -y mcp-searxng

# Daemon mode: multiple clients share one backend
mcp-wrapper-rs --daemon python3 /path/to/server.py
```

### Daemon Mode

With `--daemon`, multiple wrapper instances share a single backend subprocess through an auto-managed broker process:

```
wrapper 1 (stdio) ──→ UDS ──→ broker ──→ backend (single subprocess)
wrapper 2 (stdio) ──→ UDS ──→ broker ──↗
wrapper 3 (stdio) ──→ UDS ──→ broker ──↗
```

The first wrapper auto-spawns the broker; subsequent wrappers connect to it. The broker manages:
- Shared cache (initialize, tools/list, etc.)
- Backend subprocess lifecycle (lazy spawn, respawn on failure)
- Request multiplexing with ID remapping
- Notification fanout to all connected clients
- Graceful shutdown: broker spawned via `--daemon` runs until explicit SIGTERM

Coordination uses flock + PID file + Unix domain socket under `$XDG_RUNTIME_DIR/mcp-wrapper/`.

**When to use**: Stateless MCP servers shared across multiple Claude Code sessions.
**When NOT to use**: Stateful servers (e.g., browser automation) where each session needs its own backend.

### Admin CLI

The admin CLI is a projection over the same `mcp.wrapper` action schema, daemon, broker, backend, and MCP cache owners used at runtime. It does not manage backend processes through a second code path.

```bash
# Read broker/backend status for a command identity
mcp-wrapper-rs status --json -- python3 /path/to/server.py

# Start the broker if needed, then restart the backend and refresh MCP cache data
mcp-wrapper-rs backend restart --start --json -- python3 /path/to/server.py

# Inspect or stop the broker for the command identity
mcp-wrapper-rs broker status --json -- python3 /path/to/server.py
mcp-wrapper-rs broker stop --json -- python3 /path/to/server.py
```

Read-only admin commands do not start a broker unless `--start` is present.

### Claude Code Configuration

Edit `~/.claude.json`:

```json
{
  "mcpServers": {
    "searxng": {
      "type": "stdio",
      "command": "/path/to/mcp-wrapper-rs",
      "args": ["npx", "-y", "mcp-searxng"],
      "env": {
        "SEARXNG_URL": "http://localhost:8080"
      }
    },
    "fetcher": {
      "type": "stdio",
      "command": "/path/to/mcp-wrapper-rs",
      "args": ["npx", "-y", "fetcher-mcp"]
    },
    "my-python-server": {
      "type": "stdio",
      "command": "/path/to/mcp-wrapper-rs",
      "args": ["--daemon", "python3", "/path/to/server.py"]
    }
  }
}
```

## How It Works

1. **Initialization Phase**
   - Spawns subprocess, performs raw JSON-RPC MCP handshake
   - Queries `tools/list`, `prompts/list`, `resources/list`, `resources/templates/list`
   - Each query has a configurable timeout (`--init-timeout`, default 30s); unresponsive servers are skipped
   - Caches all results, kills init subprocess

2. **Runtime Phase**
   - `initialize` → Instant response from cache
   - `tools/list`, `prompts/list`, `resources/list` → Instant response from cache
   - `tools/call` with `mcp.wrapper` → Local wrapper control/readback through `src/tools`
   - Other `tools/call`, `resources/read`, `prompts/get` → Forwarded to persistent backend subprocess
   - Notifications relayed bidirectionally between client and backend

3. **Resource Management**
   - Persistent backend subprocess reused across calls
   - Dead backend auto-respawns with exponential backoff retry
   - Cache invalidation on `listChanged` notifications

## Debug Logging

Debug logging is **disabled by default**. Enable it with the `MCP_WRAPPER_DEBUG` environment variable:

```bash
MCP_WRAPPER_DEBUG=1 mcp-wrapper-rs npx -y mcp-searxng
```

Each MCP server gets its own log file based on the inferred name. Log location follows XDG Base Directory spec:
- `$XDG_RUNTIME_DIR/mcp-wrapper/mcp-searxng.log` (Linux with XDG runtime dir)
- `$TMPDIR/mcp-wrapper/mcp-searxng.log` (macOS or custom TMPDIR)
- `/tmp/mcp-wrapper/mcp-searxng.log` (fallback)

You can override the name with `MCP_SERVER_NAME`:
```bash
MCP_SERVER_NAME=my-custom-name mcp-wrapper-rs python3 server.py
# Logs to: $XDG_RUNTIME_DIR/mcp-wrapper/my-custom-name.log
```

## Performance

| Metric | Before | After |
|--------|--------|-------|
| Memory (4 servers) | ~370MB | ~8MB |
| Binary size | N/A | ~1.6MB |
| `tools/list` latency | ~2s | <1ms |
| `tools/call` latency | Same | Same |

### Real-World Impact

After deploying mcp-wrapper-rs with 8 MCP servers in production:

**Startup Performance**
- Claude Code launch time: **~40% faster**
- MCP initialization: From ~10s to <1s (instant cache responses)

**Runtime Performance**
- CPU load: **Reduced by ~40%** (no idle MCP processes)
- Response latency: Protocol queries return instantly from cache
- Static protocol requests stay in the wrapper cache; backend lifecycle starts only after dynamic MCP work is requested

**Why It's Faster**
- **Lazy backend lifecycle**: Backend processes only run when needed, eliminating idle overhead
- **Cache-first design**: `initialize`, `tools/list`, `prompts/list`, `resources/list` served from memory
- **Unified control surface**: CLI and MCP clients use the same wrapper action boundary for lifecycle readback and control
- **Clean resource lifecycle**: Backend process groups are cleaned up when the wrapper stops or explicitly stops the backend

## Compatibility

Works with any MCP server that:
- Uses stdio transport
- Follows MCP protocol (JSON-RPC 2.0)
- Supports standard initialization handshake

Tested with:
- `npx -y mcp-searxng`
- `npx -y fetcher-mcp`
- `npx -y @oevortex/ddg_search`
- Python-based MCP servers

## Architecture

Start with the [handbook](docs/handbook/index.html) for the current architecture
map, owner boundaries, flow projections, and proof gates. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the older compact architecture note.

## License

MIT License - see [LICENSE](LICENSE)

## Contributing

Contributions welcome. Please read the handbook first to understand the current owner boundaries and proof gates.
