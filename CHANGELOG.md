# Changelog

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

### Added

- Conformance tests using schema2object against mcp-schema.json
- Integration tests with standalone echo_server.py fixture
- ID remapping for pass-through requests (client ID mapped to backend ID)

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
