# mcp-wrapper-rs Project Memory

## Critical Rules (Read First)

1. **MUST validate CLI arguments before entering main loop**
   - *WHY: Prevents silent hangs when users provide invalid flags*
   - Pattern: Check for `-` prefix → match known flags → error on unknown → fallback to command mode

2. **ALWAYS use `cargo build --release` for production binaries**
   - *WHY: Debug builds are 10x larger and 50x slower*
   - Size: Release ~440KB, Debug ~4MB

3. **REQUIRED: Test with real MCP servers before releasing**
   - *WHY: Subprocess timing is environment-specific*
   - Test matrix: npx (searxng, fetcher), Python servers, shell scripts

4. **Language Policy: All code and docs MUST be in English**
   - *WHY: Public GitHub project for global open-source community*
   - Exception: Localized docs with language suffix (`README-zh.md`, `ARCHITECTURE-ja.md`)

## Architecture Overview

```
┌────────────┐  stdin   ┌──────────────────┐  subprocess  ┌─────────────┐
│ Claude     │ ◄──────► │ mcp-wrapper-rs   │ ────────────► │ MCP Server  │
│ Code       │  stdout  │ (Rust proxy)     │  on-demand   │ (Node/Py)   │
└────────────┘          └──────────────────┘              └─────────────┘
                              ~2MB                          spawned only
                                                           during tool call
```

### Key Files
```
src/
├── main.rs              # Entry point, CLI arg parsing, main event loop (435 lines)
├── subprocess.rs        # Process spawning, timeout handling (implicit in main.rs)
└── cache.rs            # Protocol response cache (implicit in main.rs)

Cargo.toml              # Dependencies: serde_json, libc
target/release/         # Compiled binary (~440KB with strip=true)
```

### Memory Model
- **Startup**: Spawn once → cache 4 responses → kill subprocess
- **Runtime**: Serve `initialize`/`tools/list`/`prompts/list`/`resources/list` from cache (instant)
- **Tool execution**: Spawn → execute → kill (clean lifecycle)

## Key Commands

### Development
```bash
# Build for development (unoptimized, includes debug symbols)
cargo build

# Build for release (optimized, ~440KB)
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

# Test with real MCP server
./target/release/mcp-wrapper-rs npx -y mcp-searxng
# In another terminal: echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | nc localhost <port>
```

### Debug Logging
```bash
# Enable debug logs to /tmp/mcp-wrapper-*.log
MCP_WRAPPER_DEBUG=1 mcp-wrapper-rs npx -y mcp-searxng

# View logs
tail -f /tmp/mcp-wrapper-mcp-searxng.log
```

## Development Guidelines

### Adding New Features

1. **CLI Arguments**
   - Add flag matching in `main()` before main loop
   - Pattern: `args[1].starts_with("-")` → `match args[1]` → error on unknown

2. **Subprocess Timeout**
   - Current: 60 seconds (`SUBPROCESS_TIMEOUT_SECS`)
   - Configurable via: Environment variable or Cargo feature flag
   - *WHY: Different MCP servers have different initialization times*

3. **Cache Invalidation**
   - Currently: Never invalidated (assumes static tool definitions)
   - Future: Add `tools/listChanged` notification support

### Code Style

- Use Rust 2021 idioms
- Prefer `std::process::Command` over `libc::system`
- Error handling: Return `Result<T, E>` instead of `panic!`
- Comments: Explain WHY, not WHAT

### Performance Constraints

| Metric | Target | Current |
|--------|--------|---------|
| Binary size | <500KB | ~440KB ✓ |
| Startup time | <100ms | ~50ms ✓ |
| Memory (idle) | <5MB | ~2MB ✓ |
| Tool call latency | +0ms overhead | Meets target ✓ |

### Common Pitfalls

1. **Subprocess Zombies**
   - *Problem*: Not calling `.wait()` after `.kill()`
   - *Solution*: Always `child.kill()` then `child.wait()`

2. **Stdin EOF**
   - *Problem*: Subprocess hangs waiting for more input
   - *Solution*: `drop(stdin)` to send EOF after writing requests

3. **JSON-RPC Protocol**
   - *Problem*: Forgetting `\n` delimiter between messages
   - *Solution*: Use `writeln!()` not `write!()`

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
- [ ] Binary size <500KB (`ls -lh target/release/mcp-wrapper-rs`)
- [ ] Update version in `Cargo.toml` and `main.rs`

## Version History

- **0.1.1** (2026-02-12): Added `--version`, `--help`, unknown flag handling. Fixed silent hang on invalid flags.
- **0.1.0** (2026-01-25): Initial release. Core proxy functionality, subprocess caching, debug logging.

## Reminders

- **Release binaries MUST be stripped** (already configured in `Cargo.toml`)
- **Version numbers MUST match** in `Cargo.toml` and `main.rs`
- **CLI behavior MUST follow POSIX conventions**: `-h` short flag, `--help` long flag
- **Error messages go to stderr**, success output to stdout
