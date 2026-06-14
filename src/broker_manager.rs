//! Multi-session broker: owns a single Backend subprocess and serves
//! multiple wrapper clients over a Unix domain socket.
//!
//! Launched via `--broker-internal` flag. Runs as a detached process
//! (setsid in daemon.rs). Exits after all sessions disconnect + 60s idle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::backend_manager::{Backend, BackendEvent, ChildPgids};
use crate::mcp_interface::{self, Route};
use crate::{backend_manager, daemon_manager, mcp_manager, tools};

const BROKER_IDLE_SECS: u64 = 60;

/// Shared state across all sessions.
struct BrokerState {
    cache: Arc<mcp_manager::Cache>,
    backend_slot: backend_manager::BackendSlot,
    sessions: Mutex<HashMap<u64, mpsc::UnboundedSender<Value>>>,
    active_peer_session: Mutex<Option<u64>>,
    active_backend_calls: Arc<AtomicUsize>,
    pass_through_lock: Mutex<()>,
    next_session_id: AtomicU64,
}

struct BrokerActiveCallGuard {
    session_calls: Arc<AtomicUsize>,
    broker_calls: Arc<AtomicUsize>,
}

impl BrokerActiveCallGuard {
    fn new(session_calls: Arc<AtomicUsize>, broker_calls: Arc<AtomicUsize>) -> Self {
        session_calls.fetch_add(1, Ordering::AcqRel);
        broker_calls.fetch_add(1, Ordering::AcqRel);
        Self {
            session_calls,
            broker_calls,
        }
    }
}

impl Drop for BrokerActiveCallGuard {
    fn drop(&mut self) {
        self.session_calls.fetch_sub(1, Ordering::AcqRel);
        self.broker_calls.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Entry point for `--broker-internal`. Called from `main()` BEFORE tokio runtime.
pub fn start_broker_process(args: Vec<String>) {
    // Parse: [--init-timeout N] <cmd> [cmd_args...]
    let (init_timeout, cmd_start) = if args.len() >= 2 && args[0] == "--init-timeout" {
        let secs: u64 = args[1].parse().unwrap_or(30);
        (Duration::from_secs(secs.max(1)), 2)
    } else {
        (Duration::from_secs(30), 0)
    };

    if args.len() <= cmd_start {
        eprintln!("broker: missing command");
        std::process::exit(1);
    }

    let no_idle_timeout = args[cmd_start..].contains(&"--no-idle-timeout".to_string());
    let cmd_start = if no_idle_timeout {
        cmd_start + 1
    } else {
        cmd_start
    };

    let cmd = args[cmd_start].clone();
    let cmd_args: Vec<String> = args[cmd_start + 1..].to_vec();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("broker: failed to create tokio runtime");

    rt.block_on(broker_main(cmd, cmd_args, init_timeout, no_idle_timeout));
}

async fn broker_main(
    cmd: String,
    cmd_args: Vec<String>,
    init_timeout: Duration,
    no_idle_timeout: bool,
) {
    let _tracing_guard = crate::logging::init_tracing(&cmd, &cmd_args);
    info!(cmd = %cmd, version = env!("CARGO_PKG_VERSION"), "broker: starting");

    let paths = daemon_manager::daemon_paths(&cmd, &cmd_args);
    let child_pgids: ChildPgids = backend_manager::new_child_pgids();

    // Build cache and keep the init backend alive for reuse
    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<BackendEvent>();
    let (cache, init_backend) = match mcp_manager::init_cache_with_backend(
        &cmd,
        &cmd_args,
        init_timeout,
        &child_pgids,
        notif_tx.clone(),
    )
    .await
    {
        Ok(pair) => (Arc::new(pair.0), pair.1),
        Err(e) => {
            warn!(err = %e, "broker: init_cache failed");
            std::process::exit(1);
        }
    };
    let backend_slot = backend_manager::BackendSlot::from_backend(
        backend_manager::BackendSpec {
            cmd: cmd.clone(),
            args: cmd_args.clone(),
            init_timeout,
            child_pgids: child_pgids.clone(),
            notif_tx: notif_tx.clone(),
        },
        init_backend,
    );
    info!("broker: init backend kept alive for reuse");

    let state = Arc::new(BrokerState {
        cache,
        backend_slot,
        sessions: Mutex::new(HashMap::new()),
        active_peer_session: Mutex::new(None),
        active_backend_calls: Arc::new(AtomicUsize::new(0)),
        pass_through_lock: Mutex::new(()),
        next_session_id: AtomicU64::new(1),
    });

    // Write PID file so wrappers can detect stale brokers
    let my_pid = std::process::id();
    if let Err(e) = std::fs::write(&paths.pid, format!("{}\n", my_pid)) {
        warn!(err = %e, "broker: failed to write PID file");
    }
    if let Err(e) = daemon_manager::write_broker_meta(&paths) {
        warn!(err = %e, "broker: failed to write metadata");
    }
    info!(pid = my_pid, "broker: wrote PID file");

    // Remove stale socket, then bind
    let _ = std::fs::remove_file(&paths.socket);
    std::fs::create_dir_all(paths.socket.parent().unwrap()).ok();
    let listener = match UnixListener::bind(&paths.socket) {
        Ok(l) => l,
        Err(e) => {
            warn!(err = %e, "broker: bind failed");
            let _ = std::fs::remove_file(&paths.pid);
            std::process::exit(1);
        }
    };
    info!(socket = %paths.socket.display(), "broker: listening");

    // Backend observe fanout task
    let fanout_state = state.clone();
    tokio::spawn(async move {
        while let Some(event) = notif_rx.recv().await {
            match event {
                BackendEvent::Notification(notif) => {
                    let method = notif.get("method").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(key) = mcp_interface::is_list_changed_notification(method) {
                        info!(method = %method, "broker: cache invalidation");
                        refresh_mcp_data_key(&fanout_state, key).await;
                    }
                    fanout_to_all_sessions(&fanout_state, notif).await;
                }
                BackendEvent::PeerRequest(req) => {
                    forward_peer_request(&fanout_state, req).await;
                }
                BackendEvent::UnmatchedResponse(resp) => {
                    warn!("broker: unmatched backend response");
                    fanout_to_all_sessions(&fanout_state, resp).await;
                }
                BackendEvent::ProcessExit => {
                    warn!("broker: backend process exit observed");
                }
            }
        }
    });

    // Signal handlers for graceful shutdown
    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    // Accept loop with idle timeout and signal handling
    let socket_path = paths.socket.clone();
    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let sid = state.next_session_id.fetch_add(1, Ordering::Relaxed);
                        info!(session = sid, "broker: new session");
                        let s = state.clone();
                        tokio::spawn(async move { handle_session(sid, stream, s).await });
                    }
                    Err(e) => warn!(err = %e, "broker: accept error"),
                }
            }
            _ = idle_check(&state, no_idle_timeout) => {
                info!("broker: idle timeout, shutting down");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("broker: SIGINT, shutting down");
                break;
            }
            _ = async {
                #[cfg(unix)]
                {
                    match sigterm.as_mut() {
                        Some(s) => s.recv().await,
                        None => std::future::pending::<Option<()>>().await,
                    }
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<Option<()>>().await
                }
            } => {
                info!("broker: SIGTERM, shutting down");
                break;
            }
        }
    }

    // Cleanup
    let backend = state.backend_slot.handle();
    let mut guard = backend.lock().await;
    if let Some(be) = guard.take() {
        be.kill().await;
    }
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&paths.pid);
    let _ = std::fs::remove_file(&paths.meta);
    backend_manager::kill_all_pgids(&child_pgids);
    info!("broker: shutdown complete");
}

/// Orphan-aware idle check. Two exit conditions:
/// 1. Orphan: no session ever connected within ORPHAN_TIMEOUT → exit
/// 2. Idle: all sessions disconnected, no new ones for BROKER_IDLE_SECS → exit
const ORPHAN_TIMEOUT_SECS: u64 = 120;

async fn idle_check(state: &Arc<BrokerState>, no_idle_timeout: bool) {
    if no_idle_timeout {
        std::future::pending::<()>().await;
        return;
    }
    let spawn_time = tokio::time::Instant::now();
    let mut last_active = spawn_time;

    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let sessions = state.sessions.lock().await;
        let ever_had_session = state.next_session_id.load(Ordering::Relaxed) > 1;
        let now = tokio::time::Instant::now();

        if !sessions.is_empty() {
            last_active = now;
            continue;
        }

        // Orphan guard: never received any session
        if !ever_had_session
            && now.duration_since(spawn_time) > Duration::from_secs(ORPHAN_TIMEOUT_SECS)
        {
            info!("broker: orphan timeout (no sessions ever connected)");
            return;
        }

        // Normal idle: had sessions, all gone, idle too long
        if ever_had_session
            && now.duration_since(last_active) > Duration::from_secs(BROKER_IDLE_SECS)
        {
            info!("broker: idle timeout ({BROKER_IDLE_SECS}s with no sessions)");
            return;
        }
    }
}

async fn refresh_mcp_data_key(state: &Arc<BrokerState>, key: mcp_interface::McpDataKey) {
    let backend = state.backend_slot.handle();
    let be = {
        let guard = backend.lock().await;
        guard.as_ref().cloned()
    };
    if let Some(be) = be {
        if be.is_alive() {
            let list_method = mcp_interface::method_for_mcp_data_key(&key);
            let req = mcp_interface::build_request(be.next_request_id(), list_method, None);
            match tokio::time::timeout(Duration::from_secs(5), be.send_request(req)).await {
                Ok(Ok(resp)) => {
                    if let Some(result) = resp.get("result").cloned() {
                        state.cache.update(&key, result);
                        info!(method = list_method, "broker: cache refreshed");
                    }
                }
                Ok(Err(e)) => warn!(err = %e, "broker: cache refresh failed"),
                Err(_) => warn!("broker: cache refresh timeout"),
            }
        }
    }
}

async fn fanout_to_all_sessions(state: &Arc<BrokerState>, msg: Value) {
    let sessions = state.sessions.lock().await;
    for (sid, tx) in sessions.iter() {
        if tx.send(msg.clone()).is_err() {
            debug!(session = sid, "broker: fanout channel closed");
        }
    }
}

async fn forward_peer_request(state: &Arc<BrokerState>, req: Value) {
    let active_session = *state.active_peer_session.lock().await;
    if let Some(session_id) = active_session {
        let sessions = state.sessions.lock().await;
        if let Some(tx) = sessions.get(&session_id) {
            if tx.send(req).is_err() {
                debug!(session = session_id, "broker: peer request channel closed");
            }
        }
    } else if let Some(id) = req.get("id").cloned() {
        let backend = state.backend_slot.handle();
        let be = {
            let guard = backend.lock().await;
            guard.as_ref().cloned()
        };
        if let Some(be) = be {
            let resp = mcp_interface::build_error_response(
                id,
                mcp_interface::error_codes::INTERNAL_ERROR,
                "no active client session for server request",
                None,
            );
            if let Err(e) = be.send_message(&resp).await {
                warn!(err = %e, "broker: peer request rejection failed");
            }
        }
    }
}

async fn handle_session(session_id: u64, stream: UnixStream, state: Arc<BrokerState>) {
    let (reader, writer) = stream.into_split();
    let mut uds_reader = BufReader::new(reader);
    let mut uds_writer = writer;

    let (session_tx, mut session_rx) = mpsc::unbounded_channel::<Value>();
    let active_session_calls = Arc::new(AtomicUsize::new(0));
    let mut uds_closed = false;
    state
        .sessions
        .lock()
        .await
        .insert(session_id, session_tx.clone());

    loop {
        if uds_closed && active_session_calls.load(Ordering::Acquire) == 0 && session_rx.is_empty()
        {
            break;
        }

        let mut line = String::new();
        tokio::select! {
            result = uds_reader.read_line(&mut line), if !uds_closed => {
                match result {
                    Ok(0) | Err(_) => {
                        uds_closed = true;
                        continue;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() { continue; }
                        let raw: Value = match serde_json::from_str(trimmed) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!(session = session_id, err = %e, "broker: bad JSON");
                                continue;
                            }
                        };

                        match mcp_interface::classify(&raw) {
                            mcp_interface::JsonRpcMessage::Request { id, method, .. } => {
                                handle_request(
                                    session_id,
                                    id,
                                    method,
                                    raw,
                                    state.clone(),
                                    session_tx.clone(),
                                    active_session_calls.clone(),
                                ).await;
                            }
                            mcp_interface::JsonRpcMessage::Notification { method, .. } => {
                                if method.is_empty() { continue; }
                                let backend = state.backend_slot.handle();
                                let be = {
                                    let guard = backend.lock().await;
                                    guard.as_ref().cloned()
                                };
                                if let Some(be) = be {
                                    if be.is_alive() {
                                        let _ = be.send_notification(&raw).await;
                                    }
                                }
                            }
                            mcp_interface::JsonRpcMessage::Response { .. } => {
                                let active_session = *state.active_peer_session.lock().await;
                                if active_session == Some(session_id) {
                                    let backend = state.backend_slot.handle();
                                    let be = {
                                        let guard = backend.lock().await;
                                        guard.as_ref().cloned()
                                    };
                                    if let Some(be) = be {
                                        if be.is_alive() {
                                            if let Err(e) = be.send_message(&raw).await {
                                                warn!(session = session_id, err = %e, "broker: forward client response failed");
                                            }
                                        }
                                    }
                                } else {
                                    debug!(session = session_id, "broker: unexpected response");
                                }
                            }
                        }
                    }
                }
            }
            Some(notif) = session_rx.recv() => {
                let notif_line = serde_json::to_string(&notif).unwrap_or_default();
                if write_line(&mut uds_writer, &notif_line).await.is_err() {
                    break;
                }
            }
        }
    }

    let mut sessions = state.sessions.lock().await;
    sessions.remove(&session_id);
    info!(
        session = session_id,
        remaining = sessions.len(),
        "broker: session disconnected"
    );
}

async fn write_line(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    line: &str,
) -> Result<(), std::io::Error> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn handle_request(
    session_id: u64,
    client_id: Value,
    method: String,
    raw: Value,
    state: Arc<BrokerState>,
    session_tx: mpsc::UnboundedSender<Value>,
    active_session_calls: Arc<AtomicUsize>,
) {
    if tools::is_wrapper_tool_call(&raw) {
        let result = tools::invoke(
            &raw,
            tools::InvocationContext {
                cache: &state.cache,
                backend_slot: &state.backend_slot,
                active_calls: &state.active_backend_calls,
            },
        )
        .await;
        let _ = session_tx.send(mcp_interface::build_response(client_id, result));
        return;
    }

    let resp = match mcp_interface::route(&method) {
        Route::McpData(key) => {
            debug!(session = session_id, method = %method, "broker: cache");
            match state.cache.lookup(&key) {
                Some(result) => mcp_interface::build_response(client_id, result),
                None => mcp_interface::build_error_response(
                    client_id,
                    mcp_interface::error_codes::INTERNAL_ERROR,
                    "cache miss",
                    None,
                ),
            }
        }
        Route::Local => {
            mcp_interface::build_response(client_id, Value::Object(serde_json::Map::new()))
        }
        Route::WrapperControl(control) => {
            handle_wrapper_control(
                client_id,
                control,
                raw,
                &state.cache,
                &state.backend_slot,
                &state.active_backend_calls,
            )
            .await
        }
        Route::PassThrough => {
            let active_call_guard = BrokerActiveCallGuard::new(
                active_session_calls.clone(),
                state.active_backend_calls.clone(),
            );
            tokio::spawn(async move {
                let _active_call_guard = active_call_guard;
                let _serialize_backend_call = state.pass_through_lock.lock().await;
                info!(session = session_id, method = %method, "broker: pass-through");

                if let Err(msg) = ensure_backend(&state).await {
                    warn!(session = session_id, err = %msg, "broker: backend spawn failed");
                    let resp = mcp_interface::build_error_response(
                        client_id,
                        mcp_interface::error_codes::INTERNAL_ERROR,
                        &msg,
                        None,
                    );
                    let _ = session_tx.send(resp);
                    return;
                }

                let backend = state.backend_slot.handle();
                let be = {
                    let guard = backend.lock().await;
                    guard.as_ref().cloned()
                };
                let be = match be {
                    Some(be) => be,
                    None => {
                        let resp = mcp_interface::build_error_response(
                            client_id,
                            mcp_interface::error_codes::INTERNAL_ERROR,
                            "backend unavailable",
                            None,
                        );
                        let _ = session_tx.send(resp);
                        return;
                    }
                };

                let backend_id = be.next_request_id();
                let mut forwarded = raw;
                forwarded["id"] = backend_id.clone();

                *state.active_peer_session.lock().await = Some(session_id);
                let resp = match be.send_request(forwarded).await {
                    Ok(mut resp) => {
                        *state.active_peer_session.lock().await = None;
                        resp["id"] = client_id;
                        resp
                    }
                    Err(e) => {
                        *state.active_peer_session.lock().await = None;
                        let detail = format!("{}", e);
                        warn!(session = session_id, method = %method, err = %detail, "broker: pass-through failed");
                        let backend = state.backend_slot.handle();
                        let mut guard = backend.lock().await;
                        if let Some(dead) = guard.take() {
                            dead.kill().await;
                        }
                        mcp_interface::build_error_response(
                            client_id,
                            mcp_interface::error_codes::INTERNAL_ERROR,
                            &detail,
                            None,
                        )
                    }
                };

                let _ = session_tx.send(resp);
            });
            return;
        }
    };

    let _ = session_tx.send(resp);
}

async fn handle_wrapper_control(
    client_id: Value,
    control: mcp_interface::WrapperControlMethod,
    raw: Value,
    cache: &Arc<mcp_manager::Cache>,
    backend_slot: &backend_manager::BackendSlot,
    active_calls: &Arc<AtomicUsize>,
) -> Value {
    match control {
        mcp_interface::WrapperControlMethod::BackendStatus => {
            let backend = backend_slot.snapshot().await;
            let mcp = cache.snapshot();
            mcp_interface::build_response(
                client_id,
                serde_json::json!({
                    "backend": backend.to_value(),
                    "mcp": mcp.to_value(),
                }),
            )
        }
        mcp_interface::WrapperControlMethod::BackendRefresh => {
            let backend = backend_slot.handle();
            let be = {
                let guard = backend.lock().await;
                guard.as_ref().cloned()
            };
            let be = match be {
                Some(be) if be.is_alive() => be,
                _ => {
                    return mcp_interface::build_error_response(
                        client_id,
                        mcp_interface::error_codes::INTERNAL_ERROR,
                        "backend unavailable for refresh",
                        None,
                    );
                }
            };
            let report = cache.refresh_all(&be, backend_slot.generation()).await;
            mcp_interface::build_response(client_id, report.to_value())
        }
        mcp_interface::WrapperControlMethod::BackendRestart => {
            let force = raw
                .get("params")
                .and_then(|v| v.get("force"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if backend_manager::BackendSlot::restart_blocked_by_active_calls(
                active_calls.load(Ordering::Acquire),
                force,
            ) {
                return mcp_interface::build_error_response(
                    client_id,
                    mcp_interface::error_codes::INTERNAL_ERROR,
                    "backend restart rejected while calls are active",
                    None,
                );
            }
            if let Err(e) = backend_slot.restart(force).await {
                return mcp_interface::build_error_response(
                    client_id,
                    mcp_interface::error_codes::INTERNAL_ERROR,
                    &e,
                    None,
                );
            }
            let be = match backend_slot.live().await {
                Some(be) => be,
                None => {
                    return mcp_interface::build_error_response(
                        client_id,
                        mcp_interface::error_codes::INTERNAL_ERROR,
                        "backend unavailable after restart",
                        None,
                    );
                }
            };
            let report = cache.refresh_all(&be, backend_slot.generation()).await;
            mcp_interface::build_response(client_id, report.to_value())
        }
        mcp_interface::WrapperControlMethod::BackendStop => {
            if backend_manager::BackendSlot::stop_blocked_by_active_calls(
                active_calls.load(Ordering::Acquire),
            ) {
                return mcp_interface::build_error_response(
                    client_id,
                    mcp_interface::error_codes::INTERNAL_ERROR,
                    "backend stop rejected while calls are active",
                    None,
                );
            }
            backend_slot.stop().await;
            let backend = backend_slot.snapshot().await;
            mcp_interface::build_response(client_id, backend.to_value())
        }
        mcp_interface::WrapperControlMethod::BackendPing => {
            let backend = backend_slot.snapshot().await;
            mcp_interface::build_response(
                client_id,
                serde_json::json!({
                    "ok": backend.alive,
                    "backend": backend.to_value(),
                }),
            )
        }
    }
}

/// Ensure a live backend exists. On respawn, refreshes cache synchronously
/// before notifying clients, so clients never read stale cache.
///
/// Sequence on respawn:
/// 1. Spawn new backend + MCP handshake
/// 2. Query all list/* endpoints → update cache
/// 3. Send list_changed notifications to connected clients
async fn ensure_backend(state: &Arc<BrokerState>) -> Result<(), String> {
    let was_respawn;
    {
        let backend = state.backend_slot.handle();
        let guard = backend.lock().await;
        if let Some(be) = guard.as_ref() {
            if be.is_alive() {
                return Ok(());
            }
            warn!("broker: backend died, will respawn");
            was_respawn = true;
        } else {
            was_respawn = false;
        }
    }

    let was_spawned = state.backend_slot.ensure().await?;
    if was_respawn && was_spawned {
        let backend = state.backend_slot.handle();
        let be = {
            let guard = backend.lock().await;
            guard.as_ref().cloned()
        };
        refresh_cache_from_backend(be, &state.cache).await;
        notify_clients_list_changed(state).await;
    }
    Ok(())
}

/// Query all list/* endpoints from the live backend and update cache.
async fn refresh_cache_from_backend(be: Option<Arc<Backend>>, cache: &Arc<mcp_manager::Cache>) {
    let be = match be {
        Some(be) => be,
        None => return,
    };

    let keys_methods = [
        (mcp_interface::McpDataKey::ToolsList, "tools/list"),
        (mcp_interface::McpDataKey::PromptsList, "prompts/list"),
        (mcp_interface::McpDataKey::ResourcesList, "resources/list"),
        (
            mcp_interface::McpDataKey::ResourceTemplatesList,
            "resources/templates/list",
        ),
    ];

    for (key, method) in &keys_methods {
        let req = mcp_interface::build_request(be.next_request_id(), method, None);
        match tokio::time::timeout(Duration::from_secs(5), be.send_request(req)).await {
            Ok(Ok(resp)) => {
                if let Some(result) = resp.get("result").cloned() {
                    cache.update(key, result);
                    info!(method = *method, "broker: cache refreshed after respawn");
                }
            }
            Ok(Err(e)) => warn!(method = *method, err = %e, "broker: cache refresh failed"),
            Err(_) => warn!(method = *method, "broker: cache refresh timeout"),
        }
    }
}

/// Send list_changed notifications to all connected clients.
/// Called after cache is already refreshed, so clients get fresh data.
async fn notify_clients_list_changed(state: &Arc<BrokerState>) {
    let notifications = [
        "notifications/tools/list_changed",
        "notifications/prompts/list_changed",
        "notifications/resources/list_changed",
    ];

    let sessions = state.sessions.lock().await;
    for method in &notifications {
        let notif = mcp_interface::build_notification(method, None);
        for (sid, tx) in sessions.iter() {
            if tx.send(notif.clone()).is_err() {
                debug!(
                    session = sid,
                    method = *method,
                    "broker: notify channel closed"
                );
            }
        }
    }
    info!("broker: sent list_changed to {} clients", sessions.len());
}
