# Changelog

## [0.4.2] - 2026-03-28

### Fixed

- **Daemon: no idle timeout** — broker spawned via `--daemon` no longer exits after 60s of no sessions. The broker now runs until explicit SIGTERM, matching the expected "always-on" daemon semantics. Non-daemon mode (relay) retains the 60s idle exit behavior unchanged.

### Changed

- `spawn_broker_process()` passes `--no-idle-timeout` to broker when spawned via `--daemon`
- `idle_check()` accepts `no_idle_timeout: bool`; returns `pending()` immediately when set, preventing the idle shutdown path from ever triggering

## [0.4.1] - 2026-03-24

### Fixed

- **Daemon: keep init backend alive** — daemon/broker mode no longer kills the backend after cache init. The init backend is reused for serving requests, eliminating a redundant respawn on the first `tools/call`.
- **Daemon: synchronous cache refresh on respawn** — when backend dies and is respawned on demand, cache is refreshed synchronously (query all `list/*` endpoints) **before** sending `list_changed` notifications to clients. This guarantees clients never read stale cache after receiving a notification.

### Changed

- Extracted `init_cache_inner()` from `init_cache()` to share logic between normal mode (kill backend after init) and daemon mode (keep backend alive)
- Added `init_cache_with_backend()` public API for daemon/broker use
- `ensure_backend()` in broker now uses dedicated `refresh_cache_from_backend()` + `notify_clients_list_changed()` instead of routing through the async fanout task
- Notification to clients after respawn is sent directly to session channels, bypassing fanout task to guarantee ordering

## [0.4.0] - 2026-03-17

### Added

- **Daemon mode** (`--daemon`): N:1 broker architecture where multiple wrapper instances share a single backend subprocess via Unix domain socket
- `src/daemon.rs`: coordination protocol (flock + PID file + UDS connect/retry), bidirectional stdin↔UDS relay
- `src/broker.rs`: multi-session broker with shared cache, backend multiplexing, notification fanout
- PID file management (`{hash}.pid`) for stale broker detection and recovery
- Orphan guard: broker auto-exits after 120s if no session ever connects
- Idle exit: broker shuts down 60s after all sessions disconnect
- Signal handling (SIGINT/SIGTERM) for graceful broker and relay shutdown
- Integration tests for daemon mode: single client, two-client sharing, ping

### Changed

- Version bump to 0.4.0
- Made helper functions `pub` for broker reuse: `init_tracing`, `kill_all_pgids`, `cmd_hash`, `log_dir`, `sanitize_name`, `infer_mcp_name`
- Updated CLI help to document `--daemon` flag

### Dependencies

- Added `fd-lock = "4"` for flock-based coordination
- Added `"net"` to tokio features for Unix domain socket support

## [0.3.0] - 2026-03-05

### Changed

- Replaced rmcp SDK with raw JSON-RPC protocol handling
- Modularized into 5 focused modules: transport, router, cache, proxy, main
- Advertise ALL backend capabilities (not just tools)
- Bidirectional notification relay (backend to client and client to backend)
- Cache invalidation on tools/list_changed, prompts/list_changed, resources/list_changed
- Lazy backend spawn with 3-attempt exponential backoff retry
- Graceful backend shutdown: SIGTERM then 5s wait then SIGKILL
- Full MCP handshake on lazy backend spawn

### Removed

- rmcp SDK dependency
- x-tests JSON spec (mcp-proxy-tests.json)
- Old test files (behavioral.rs, schema_driven.rs)

### Fixed

- Capability-aware cache init: skip list methods unsupported by backend (prevents timeout)
- Eliminated all dead code warnings (0 compiler warnings)

### Added

- Conformance tests using schema2object against mcp-schema.json
- Integration tests with standalone echo_server.py fixture
- ID remapping for pass-through requests (client ID mapped to backend ID)
- 3-layer automated test suite (`scripts/test-all.sh`): compile check, unit/integration tests, real-server smoke tests

## [0.2.1] - 2026-02-26

### Fixed

- **Orphan subprocess leak**: Child process trees (e.g. `npm` → `sh` → `node`) survived wrapper exit. Root cause: rmcp's `ChildWithCleanup::drop` uses `tokio::spawn` for async kill, which silently fails during runtime shutdown.

### Added

- Process group isolation via `process-wrap` `ProcessGroup::leader()` — each spawned backend runs in its own process group (pgid == child pid).
- Deterministic shutdown: all child process groups are killed with `SIGKILL` via `libc::kill(-pgid)` before the tokio runtime exits.
- `child_pgids` registry tracks every spawned subprocess for cleanup.

### Dependencies

- Added `process-wrap` (already a transitive dep of rmcp, now explicit).
- Added `libc` (already a transitive dep, now explicit for `kill` syscall).

## [0.2.0] - 2026-02-25

### Changed

- Async concurrency overhaul: `BackendState` machine, spawn-outside-lock pattern, `AtomicU64` reaper, `ActiveCallGuard` RAII, init backend reuse.
- `tokio::Mutex` for stderr buffer (lock guard spans `.await`).
- Hot-path allocation guards (`tracing::enabled!` checks).
- Capabilities filtering: advertise only implemented capabilities.
- Error code preservation from backend MCP errors.
- Forwarding for `read_resource`, `get_prompt`, `complete`.
- Schema-Driven Development test suite added.

## [0.2.0] - 2026-02-23

### Changed

- Full rewrite using rmcp 0.16 SDK.
- Persistent backend for tool calls.
- Pagination support.
- Protocol handling delegated to rmcp.

## [0.1.3] - 2026-02-18

### Fixed

- Cache and response ordering bugs.

## [0.1.1] - 2026-02-12

### Added

- `--version`, `--help` flags.
- Unknown flag handling with error message.

### Fixed

- Silent hang on invalid CLI flags.

## [0.1.0] - 2026-01-25

### Added

- Initial release.
- Core proxy functionality.
- Subprocess caching.
- Debug logging.
