# Changelog

## [0.5.1] - 2026-07-11

### Fixed

- Backend stderr retention now truncates at a valid UTF-8 boundary. Previously,
  the bounded 4096-byte diagnostic tail sliced a Rust `String` at an arbitrary
  byte offset. A multibyte character crossing that offset caused a worker panic;
  the release profile converted it to `SIGABRT`, closing the wrapper transport
  and making local lifecycle tools such as `backend.restart` unreachable.
- Added regression coverage for Chinese text, an em dash, emoji, and unchanged
  short text. Diagnostic content can no longer terminate the control plane.
- Backend exit now drains pending request senders, so a request cannot wait
  forever after its backend disappears when no explicit timeout is configured.
- Daemon cancellation and progress ownership are scoped by session and raw
  JSON-RPC ID. Reusing numeric or string IDs in different client sessions no
  longer allows one session to affect another.
- Cancelled and timed-out backend request IDs remain suppressed even after the
  bounded tombstone detail cache rotates, preventing late backend responses
  from leaking as unmatched client-visible messages.
- Backend progress notifications no longer wait for the consumer from inside
  the backend reader, removing a response-order deadlock during refresh.
- Discovery refresh pagination now shares one absolute deadline instead of
  renewing the timeout for every page or cache key.
- Standard `notifications/resources/list_changed` refreshes both resources and
  resource templates, and wrapper-triggered refresh/restart actions notify
  connected daemon clients after cache mutation.
- Concurrent daemon integration tests now use isolated broker identities, so
  one test's cleanup cannot terminate another test's shared broker.
- Discovery refresh now rejects JSON-RPC error envelopes and malformed list
  results instead of projecting them as valid empty data. All selected keys are
  built as one candidate and committed atomically, so a later-key failure
  cannot mix new and old discovery rows.
- Cache refresh construction is serialized and commit checks both the base
  cache epoch and live backend generation. Stale invalidations and candidates
  crossing a backend restart cannot move committed discovery truth backward.
- Failed refreshes return an error, preserve the complete prior snapshot, and
  emit no list-changed notification.

### Changed

- Broker request lifetime is represented by session-aware request rows and
  backend-generation bindings. Active-call state, cancellation, completion,
  and cleanup are derived from those owner tables instead of compatibility
  counters or process-global request IDs.
- Request and backend generation readback is retained through terminal cleanup,
  making lifecycle status a projection of owned rows rather than a parallel
  mutable truth.

### Proof

- `cargo test stderr_tail_truncation`
- `cargo test backend_manager_tests`
- `cargo test lifetime_tests`
- `cargo test`
- `cargo build --release`

## [0.5.0] - 2026-06-14

Supervisor-proxy milestone. This release fills gaps that plain stdio MCP leaves
open: a client cannot control the lifecycle of the server it is connected to,
there is no standard backend-state readback, idle servers still cost memory, and
every client spawns its own backend. mcp-wrapper-rs closes all four behind one
stable transport.

### What it fills in over plain stdio MCP

- **Backend lifecycle control** — plain MCP gives the client no way to restart,
  stop, refresh, or inspect the server it talks to. The unified `mcp.wrapper`
  tool exposes `restart` / `stop` / `refresh` / `status` / `ping`, and a backend
  restart never drops the client-facing transport.
- **Unified readback / observability** — plain MCP has no standard backend
  status surface. An `observe -> hook -> readback` control plane turns backend
  facts into proven state (snapshot, generation, cache epoch, discovery hash)
  before any client, log, or CLI reports it.
- **Cache-first stable discovery layer** — plain MCP servers keep consuming
  memory while idle. The wrapper caches stable discovery data and spawns the
  backend lazily, so an idle wrapper is near-free.
- **Backend sharing across clients** — plain MCP makes every client spawn its
  own server. Daemon mode shares one backend/cache pair across sessions through
  a broker, with broker-global active-call safety.

### Added

- **Unified `mcp.wrapper` tool** — one reserved backend-visible tool injected
  into `tools/list`. Backend `status`, `ping`, `refresh`, `restart`, and `stop`
  are `action`s on this single tool, sharing one action schema and result
  envelope across MCP clients and the admin CLI. Backend tools named
  `mcp.wrapper` are rejected by collision policy and cannot overwrite the
  wrapper descriptor.
- **Daemon broker active-call safety** — the broker tracks a broker-global
  active backend-call counter; daemon `stop` and non-force `restart` (via
  `mcp.wrapper`, legacy wrapper-control methods, or CLI) are rejected while any
  shared-backend call is in flight.
- **`BackendSlot::live()`** owner query and active-call rejection predicates
  (`restart_blocked_by_active_calls`, `stop_blocked_by_active_calls`).
- Collision-preservation regression proof: the wrapper descriptor always wins a
  reserved-name collision.

### Changed

- **Owner reshape** — wrapper tool logic reshaped to owner-local methods.
  `BackendSlot` owns the lifecycle decision; callers reach live state through
  owner APIs instead of locking the raw backend handle. Tool validation is
  internalized into `ToolAction::validate_params` and
  `ToolInvocation::from_arguments`; the dispatch layer holds no validation
  rules. `tools` is the external entry / microkernel projection, not a
  lifecycle owner.
- **Cache as hold-state** — `mcp_manager::Cache` owns the external `tools/list`
  / `initialize` views as derived hold-state, rebuilt on load/refresh, so
  `lookup` is a pure read. The backend discovery fingerprint (`discovery_hash`)
  is computed from raw backend responses only and is never polluted by the
  wrapper-merged view.
- **Docs role split** — `CLAUDE.md` trimmed to act rules + navigation;
  architecture truth (owner map, backend lifecycle, concurrency, planes, tools
  projection) lives in `docs/handbook/`.

### Behaviour

- No external contract change from the reshape: envelope shapes, error codes,
  JSON-RPC error codes, the `tools/list` union, and admin CLI output are
  unchanged. Verified by the full test suite plus daemon and real-server smoke.

### Next

- **Bidirectional communication** is the next development direction: deepening
  the server-to-client request/notification bridge so wrapped servers can drive
  richer two-way MCP interactions through the stable wrapper transport.

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
