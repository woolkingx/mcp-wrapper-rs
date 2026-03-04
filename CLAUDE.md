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
| shutdown-kill | wrapper exits | `libc::kill(-pgid, SIGKILL)` before `async_main` returns | async drop is unreliable during runtime shutdown |
| mutex-type | lock spans `.await` | `tokio::sync::Mutex`, never `std::sync::Mutex` | std Mutex blocks runtime |
| backend-error | spawn fails | retry up to 3 attempts with exponential backoff | transient failures should not be permanent |
| reaper-loop | idle reaper | never `break` — loop runs for process lifetime | must keep checking after kill |
| hot-path-alloc | `format!` in log | guard with `tracing::enabled!(Level::DEBUG)` | allocates even when debug off |
| version-source | read version | `env!("CARGO_PKG_VERSION")` only — Cargo.toml is source of truth | prevents version drift |

## Architecture

```
Client → [stdin/stdout] → main.rs event loop → Router
                                    ↓
                    ┌───────────────┼───────────────┐
                    ▼               ▼               ▼
                  Cache          PassThru      Notifications
                (list/*, init)  (tools/call,  (bidirectional
                                 resources/read, relay)
                                 prompts/get...)
                                    ↓
                              proxy::Backend
                                    ↓
                              child subprocess
```

### Module Design

- **transport.rs**: Raw JSON-RPC line protocol — read/write/build messages over stdin/stdout
- **router.rs**: Method dispatch — classifies requests as Cached, PassThrough, or Notification
- **cache.rs**: Init sequence — spawns subprocess, queries all list endpoints, caches results; supports invalidation via `listChanged` notifications
- **proxy.rs**: Backend lifecycle — `Option<Backend>` with lazy spawn, oneshot request/response matching, process group isolation
- **main.rs**: CLI parsing, signal handling, event loop orchestrating router + cache + proxy

### Backend Lifecycle

- **Lazy spawn**: Backend created on first pass-through request, not at startup
- **Retry**: 3 attempts with exponential backoff on spawn failure
- **Health check**: verify child process alive before forwarding
- **Graceful shutdown**: SIGTERM → 5s wait → SIGKILL
- **Cleanup**: each child in own process group, `child_pgids` registry, killpg on exit

### Concurrency

- Oneshot channels for request/response matching (client ID remapped to backend ID)
- `tokio::sync::Mutex` for backend access (lock spans `.await`)
- Bidirectional notification relay between client and backend

## Conventions

| area | pattern |
|------|---------|
| error handling | JSON-RPC error responses with standard error codes |
| logging | `tracing` macros only — never `println!` or hand-rolled `log()` |
| comments | explain WHY, not WHAT |
| JSON-RPC | raw protocol handling via `transport.rs` — build/parse messages directly |
| CLI flags | match in `main()` before runtime: `--init-timeout <secs>` (default 30), `--version`, `--help` |
| new method support | cached → add to `router.rs` Cached variant + `cache.rs`; forwarded → add PassThrough variant |
| ID remapping | client ID saved, replaced with monotonic backend ID, restored on response |
| child pgids | `Arc<std::sync::Mutex<Vec<u32>>>` — std Mutex is correct here (sync-only access) |

## Testing Strategy

| file | purpose | driver |
|------|---------|--------|
| `conformance.rs` | JSON-RPC response conformance against MCP schema | `mcp-schema.json` |
| `integration.rs` | End-to-end proxy tests with echo_server.py fixture | manual |

**Conformance tests**: validate response structure using schema2object against `mcp-schema.json`

**Integration tests**: standalone `echo_server.py` fixture for full request/response cycle

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
- **Cache invalidation supported** — responds to `tools/list_changed`, `prompts/list_changed`, `resources/list_changed` notifications
- **Always kill process groups explicitly** before runtime shutdown — async drop is unreliable
