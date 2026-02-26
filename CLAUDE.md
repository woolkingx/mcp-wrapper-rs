# mcp-wrapper-rs Project Memory

## Critical Rules

| rule | trigger | action | why |
|------|---------|--------|-----|
| cli-validate | add CLI flag | match in `main()` before runtime, error on unknown | silent hang on invalid flags |
| release-build | production binary | `cargo build --release` + `cargo install --path .` | debug builds 10x larger |
| real-server-test | before release | test with npx, Python, shell MCP servers | subprocess timing is env-specific |
| agent-review | agent writes code | `git diff` → verify only planned changes | agents add unplanned breakage |
| english-only | write code/docs | all English, exception: `*-zh.md` suffix | public GitHub, global community |
| process-group | spawn child | `ProcessGroup::leader()` + track pgid | orphan leak without it |
| shutdown-kill | wrapper exits | `libc::kill(-pgid, SIGKILL)` before `async_main` returns | rmcp drop uses `tokio::spawn` which fails during runtime shutdown |
| mutex-type | lock spans `.await` | `tokio::sync::Mutex`, never `std::sync::Mutex` | std Mutex blocks runtime |
| backend-error | spawn fails in `Spawning` state | reset to `Empty` + `notify_waiters()` | concurrent callers hang otherwise |
| reaper-loop | idle reaper | never `break` — loop runs for process lifetime | must keep checking after kill |
| hot-path-alloc | `format!` in log | guard with `tracing::enabled!(Level::DEBUG)` | allocates even when debug off |
| version-source | read version | `env!("CARGO_PKG_VERSION")` only — Cargo.toml is source of truth | prevents version drift |

## Architecture

```
Client → [stdin/stdout] → rmcp serve_server(McpProxy) → ServerHandler trait
                                   ↓ (on tools/call)
                          McpProxy.ensure_backend()
                                   ↓
                          rmcp serve_client(TokioChildProcess) → real MCP server
                          (wrapped in ProcessGroup::leader())
```

### McpProxy Design

- Implements `rmcp::ServerHandler` — rmcp handles full JSON-RPC protocol
- **Startup**: spawn subprocess → cache tools/prompts/resources/resource_templates → keep as first backend
- **Serve**: list requests served from cache (instant)
- **Tool call**: `ensure_backend()` → clone `Peer` → release lock → `peer.call_tool()` outside lock
- **Recovery**: `is_closed()` check → auto-respawn
- **Cleanup**: each child in own process group, `child_pgids` registry, killpg on exit

### BackendState Machine

```
Empty → Spawning(Notify) → Ready(RunningService, stderr_buf)
          ↑ concurrent callers wait on Notify
          ↑ on failure: reset to Empty + notify_waiters
```

### Concurrency (rmcp internals)

- `Peer` = `mpsc::Sender` (Arc) — clone is cheap, full concurrent multiplexing
- No connection pool needed — multiple `call_tool` in flight on one connection
- Mutex held only during `ensure_backend()` to clone `Peer`, NOT during call

## Conventions

| area | pattern |
|------|---------|
| error handling | `Result<T, ErrorData>` for ServerHandler methods |
| logging | `tracing` macros only — never `println!` or hand-rolled `log()` |
| comments | explain WHY, not WHAT |
| JSON-RPC | delegate to rmcp — never hand-parse |
| CLI flags | match in `main()` before runtime: `--init-timeout <secs>` (default 30), `--version`, `--help` |
| new ServerHandler method | cached → return struct field; forwarded → `ensure_backend()` + `peer.method()` |
| idle reaper | `AtomicU64` timestamp, not `Notify` (edge-triggered, loses wakeups under burst) |
| active call guard | `ActiveCallGuard` RAII (AtomicUsize) prevents reaper from killing mid-call |
| child pgids | `Arc<std::sync::Mutex<Vec<u32>>>` — std Mutex is correct here (sync-only access) |

## Testing Strategy (Schema-Driven)

Tests derive from schema — not hand-written from memory.

| file | purpose | driver |
|------|---------|--------|
| `behavioral.rs` | black-box process tests (CLI, basic MCP flow) | manual |
| `conformance.rs` | JSON-RPC response conformance | `mcp-schema.json` |
| `schema_driven.rs` | proxy-specific behaviors (concurrency, error codes, reaper) | `mcp-proxy-tests.json` |

**Adding tests**: add entry to `mcp-proxy-tests.json` under `x-tests` → implement in `schema_driven.rs`

**Scenario tags**: `null`, `missing`, `empty`, `min_boundary`, `max_boundary`, `valid_normal`, `valid_edge`, `custom:<desc>`

## Release Checklist

- [ ] `cargo build --release` succeeds
- [ ] `--version` / `--help` / `--unknown` / no-args all behave correctly
- [ ] Wraps npx, Python, shell script MCP servers
- [ ] Debug logging works (`MCP_WRAPPER_DEBUG=1`)
- [ ] Consecutive tool calls reuse backend (single "spawning" in log)
- [ ] No orphan processes after wrapper exit
- [ ] Version bumped in `Cargo.toml`, CHANGELOG.md updated

## Reminders

- **Release binaries MUST be stripped** (configured in `Cargo.toml` `[profile.release]`)
- **Error messages → stderr**, JSON-RPC output → stdout
- **CLI follows POSIX**: `-h`/`-V` short flags, `--help`/`--version` long flags
- **Cache never invalidated** — assumes static tool definitions (future: `tools/listChanged`)
- **rmcp drop is unreliable** — always kill process groups explicitly before runtime shutdown
