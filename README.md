# mcp-wrapper-rs

A lightweight, universal MCP (Model Context Protocol) wrapper written in Rust. Reduces memory footprint from ~100MB per MCP server to ~2MB by caching protocol responses and spawning subprocesses only when needed.

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

`mcp-wrapper-rs` acts as a transparent proxy:

```
┌─────────────┐      ┌─────────────────┐      ┌─────────────┐
│ Claude Code │ ──── │ mcp-wrapper-rs  │ ──── │ MCP Server  │
│             │      │    (~2MB)       │      │ (on-demand) │
└─────────────┘      └─────────────────┘      └─────────────┘
```

- **Startup**: Spawns subprocess once to cache `tools/list`, `prompts/list`, `resources/list`
- **Runtime**: Responds instantly from cache for protocol queries
- **Tool calls**: Spawns subprocess, executes, returns result, kills subprocess

Result: **4 MCP servers using only ~8MB total** (vs ~370MB before)

## Installation

### From Source

```bash
git clone https://github.com/yourname/mcp-wrapper-rs.git
cd mcp-wrapper-rs
cargo build --release
```

Binary will be at `target/release/mcp-wrapper-rs` (~432KB)

### Pre-built Binaries

Check [Releases](https://github.com/yourname/mcp-wrapper-rs/releases) for pre-built binaries.

## Usage

```bash
mcp-wrapper-rs <command> [args...]
```

### Examples

```bash
# Wrap an npx-based MCP server
mcp-wrapper-rs npx -y mcp-searxng

# Wrap a Python MCP server
mcp-wrapper-rs python3 /path/to/server.py

# Wrap with environment variables (inherited from parent)
SEARXNG_URL=http://localhost:8080 mcp-wrapper-rs npx -y mcp-searxng
```

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
      "args": ["python3", "/path/to/server.py"]
    }
  }
}
```

## How It Works

1. **Initialization Phase**
   - Spawns subprocess with MCP `initialize` + `tools/list` + `prompts/list` + `resources/list`
   - Waits for 4 responses (with 60s timeout)
   - Caches all responses, kills subprocess

2. **Runtime Phase**
   - `initialize` → Instant response from cache
   - `tools/list` → Instant response from cache
   - `prompts/list` → Instant response from cache
   - `resources/list` → Instant response from cache
   - `tools/call` → Spawns subprocess, executes, returns, kills

3. **Resource Management**
   - Subprocess only lives during `tools/call` execution
   - 60-second timeout prevents hanging
   - Clean process termination with kill + wait

## Debug Logging

Logs are written to `/tmp/mcp-wrapper-rs.log`:

```bash
tail -f /tmp/mcp-wrapper-rs.log
```

## Performance

| Metric | Before | After |
|--------|--------|-------|
| Memory (4 servers) | ~370MB | ~8MB |
| Binary size | N/A | 432KB |
| `tools/list` latency | ~2s | <1ms |
| `tools/call` latency | Same | Same |

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

See [ARCHITECTURE.md](ARCHITECTURE.md) for detailed design documentation.

## License

MIT License - see [LICENSE](LICENSE)

## Contributing

Contributions welcome! Please read the architecture doc first to understand the design decisions.
