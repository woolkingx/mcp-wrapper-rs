//! Multi-session broker: owns a single Backend subprocess and serves
//! multiple wrapper clients over a Unix domain socket.
//!
//! Launched via `--broker-internal` flag. Runs as a detached process
//! (setsid in daemon.rs). Exits after all sessions disconnect + 60s idle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::cache;
use crate::daemon;
use crate::proxy::{Backend, ChildPgids};
use crate::router::{self, Route};
use crate::transport;

const BROKER_IDLE_SECS: u64 = 60;

/// Shared state across all sessions.
struct BrokerState {
    cmd: String,
    cmd_args: Vec<String>,
    init_timeout: Duration,
    cache: Arc<cache::Cache>,
    backend: Arc<Mutex<Option<Backend>>>,
    child_pgids: ChildPgids,
    sessions: Mutex<HashMap<u64, mpsc::UnboundedSender<Value>>>,
    next_session_id: AtomicU64,
    notif_tx: mpsc::UnboundedSender<Value>,
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

    let cmd = args[cmd_start].clone();
    let cmd_args: Vec<String> = args[cmd_start + 1..].to_vec();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("broker: failed to create tokio runtime");

    rt.block_on(broker_main(cmd, cmd_args, init_timeout));
}

async fn broker_main(cmd: String, cmd_args: Vec<String>, init_timeout: Duration) {
    let _tracing_guard = crate::init_tracing(&cmd, &cmd_args);
    info!(cmd = %cmd, version = env!("CARGO_PKG_VERSION"), "broker: starting");

    let paths = daemon::daemon_paths(&cmd, &cmd_args);
    let child_pgids: ChildPgids = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Build cache
    let cache = match cache::init_cache(&cmd, &cmd_args, init_timeout, &child_pgids).await {
        Ok(c) => Arc::new(c),
        Err(e) => {
            warn!(err = %e, "broker: init_cache failed");
            std::process::exit(1);
        }
    };

    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<Value>();
    let backend: Arc<Mutex<Option<Backend>>> = Arc::new(Mutex::new(None));

    let state = Arc::new(BrokerState {
        cmd: cmd.clone(),
        cmd_args: cmd_args.clone(),
        init_timeout,
        cache,
        backend: backend.clone(),
        child_pgids: child_pgids.clone(),
        sessions: Mutex::new(HashMap::new()),
        next_session_id: AtomicU64::new(1),
        notif_tx: notif_tx.clone(),
    });

    // Write PID file so wrappers can detect stale brokers
    let my_pid = std::process::id();
    if let Err(e) = std::fs::write(&paths.pid, format!("{}\n", my_pid)) {
        warn!(err = %e, "broker: failed to write PID file");
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

    // Notification fanout task
    let fanout_state = state.clone();
    tokio::spawn(async move {
        while let Some(notif) = notif_rx.recv().await {
            let method = notif.get("method").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(key) = router::is_list_changed_notification(method) {
                info!(method = %method, "broker: cache invalidation");
                let guard = fanout_state.backend.lock().await;
                if let Some(be) = guard.as_ref() {
                    if be.is_alive() {
                        let list_method = router::list_method_for_key(&key);
                        let req = transport::build_request(be.next_request_id(), list_method, None);
                        match tokio::time::timeout(Duration::from_secs(5), be.send_request(req)).await {
                            Ok(Ok(resp)) => {
                                if let Some(result) = resp.get("result").cloned() {
                                    fanout_state.cache.update(&key, result);
                                    info!(method = list_method, "broker: cache refreshed");
                                }
                            }
                            Ok(Err(e)) => warn!(err = %e, "broker: cache refresh failed"),
                            Err(_) => warn!("broker: cache refresh timeout"),
                        }
                    }
                }
            }

            // Fanout to all sessions
            let sessions = fanout_state.sessions.lock().await;
            for (sid, tx) in sessions.iter() {
                if tx.send(notif.clone()).is_err() {
                    debug!(session = sid, "broker: fanout channel closed");
                }
            }
        }
    });

    // Signal handlers for graceful shutdown
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    ).ok();

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
            _ = idle_check(&state) => {
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
    let mut guard = backend.lock().await;
    if let Some(be) = guard.take() {
        be.kill().await;
    }
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&paths.pid);
    crate::kill_all_pgids(&child_pgids);
    info!("broker: shutdown complete");
}

/// Orphan-aware idle check. Two exit conditions:
/// 1. Orphan: no session ever connected within ORPHAN_TIMEOUT → exit
/// 2. Idle: all sessions disconnected, no new ones for BROKER_IDLE_SECS → exit
const ORPHAN_TIMEOUT_SECS: u64 = 120;

async fn idle_check(state: &Arc<BrokerState>) {
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

async fn handle_session(session_id: u64, stream: UnixStream, state: Arc<BrokerState>) {
    let (reader, writer) = stream.into_split();
    let mut uds_reader = BufReader::new(reader);
    let mut uds_writer = writer;

    let (session_tx, mut session_rx) = mpsc::unbounded_channel::<Value>();
    state.sessions.lock().await.insert(session_id, session_tx);

    loop {
        let mut line = String::new();
        tokio::select! {
            result = uds_reader.read_line(&mut line) => {
                match result {
                    Ok(0) | Err(_) => break,
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

                        match transport::classify(&raw) {
                            transport::JsonRpcMessage::Request { id, method, .. } => {
                                let resp = handle_request(session_id, id, &method, raw, &state).await;
                                let resp_line = serde_json::to_string(&resp).unwrap_or_default();
                                if write_line(&mut uds_writer, &resp_line).await.is_err() {
                                    break;
                                }
                            }
                            transport::JsonRpcMessage::Notification { method, .. } => {
                                if method.is_empty() { continue; }
                                let guard = state.backend.lock().await;
                                if let Some(be) = guard.as_ref() {
                                    if be.is_alive() {
                                        let _ = be.send_notification(&raw).await;
                                    }
                                }
                            }
                            transport::JsonRpcMessage::Response { .. } => {
                                debug!(session = session_id, "broker: unexpected response");
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
    info!(session = session_id, remaining = sessions.len(), "broker: session disconnected");
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
    method: &str,
    raw: Value,
    state: &Arc<BrokerState>,
) -> Value {
    match router::route(method) {
        Route::Cache(key) => {
            debug!(session = session_id, method = %method, "broker: cache");
            match state.cache.lookup(&key) {
                Some(result) => transport::build_response(client_id, result),
                None => transport::build_error_response(
                    client_id, transport::error_codes::INTERNAL_ERROR, "cache miss", None,
                ),
            }
        }
        Route::Local => {
            transport::build_response(client_id, Value::Object(serde_json::Map::new()))
        }
        Route::PassThrough => {
            info!(session = session_id, method = %method, "broker: pass-through");

            if let Err(msg) = ensure_backend(state).await {
                warn!(session = session_id, err = %msg, "broker: backend spawn failed");
                return transport::build_error_response(
                    client_id, transport::error_codes::INTERNAL_ERROR, &msg, None,
                );
            }

            let be_lock = state.backend.lock().await;
            let be = match be_lock.as_ref() {
                Some(be) => be,
                None => {
                    return transport::build_error_response(
                        client_id, transport::error_codes::INTERNAL_ERROR, "backend unavailable", None,
                    );
                }
            };

            let backend_id = be.next_request_id();
            let mut forwarded = raw;
            forwarded["id"] = backend_id.clone();

            match be.send_request(forwarded).await {
                Ok(mut resp) => {
                    resp["id"] = client_id;
                    resp
                }
                Err(e) => {
                    let detail = format!("{}", e);
                    warn!(session = session_id, method = %method, err = %detail, "broker: pass-through failed");
                    drop(be_lock);
                    let mut guard = state.backend.lock().await;
                    if let Some(dead) = guard.take() {
                        dead.kill().await;
                    }
                    transport::build_error_response(
                        client_id, transport::error_codes::INTERNAL_ERROR, &detail, None,
                    )
                }
            }
        }
    }
}

/// Ensure a live backend exists. Same retry logic as main.rs ensure_backend.
async fn ensure_backend(state: &Arc<BrokerState>) -> Result<(), String> {
    {
        let guard = state.backend.lock().await;
        if let Some(be) = guard.as_ref() {
            if be.is_alive() {
                return Ok(());
            }
            warn!("broker: backend died, will respawn");
        }
    }

    let backoff = [1, 2, 4];
    for (attempt, delay) in backoff.iter().enumerate() {
        info!(attempt = attempt + 1, "broker: spawning backend");
        match spawn_and_init_backend(state).await {
            Ok(new_be) => {
                info!("broker: backend ready");
                let mut guard = state.backend.lock().await;
                if let Some(old) = guard.take() {
                    old.kill().await;
                }
                *guard = Some(new_be);
                drop(guard);

                // Trigger cache refresh via existing fanout mechanism
                for method in &[
                    "notifications/tools/list_changed",
                    "notifications/prompts/list_changed",
                    "notifications/resources/list_changed",
                ] {
                    let _ = state.notif_tx.send(
                        crate::transport::build_notification(method, None),
                    );
                }

                return Ok(());
            }
            Err(e) => {
                warn!(attempt = attempt + 1, err = %e, "broker: spawn failed");
                if attempt < backoff.len() - 1 {
                    tokio::time::sleep(Duration::from_secs(*delay)).await;
                }
            }
        }
    }

    let mut guard = state.backend.lock().await;
    if let Some(dead) = guard.take() {
        dead.kill().await;
    }
    Err("backend spawn failed after 3 attempts".to_string())
}

async fn spawn_and_init_backend(state: &Arc<BrokerState>) -> Result<Backend, String> {
    let backend = Backend::spawn(
        &state.cmd, &state.cmd_args, state.notif_tx.clone(), &state.child_pgids,
    ).map_err(|e| format!("spawn: {}", e))?;

    let init_req = cache::build_initialize_request(backend.next_request_id());
    let init_resp = tokio::time::timeout(state.init_timeout, backend.send_request(init_req))
        .await
        .map_err(|_| "initialize handshake timeout".to_string())?
        .map_err(|e| format!("initialize handshake: {}", e))?;

    if init_resp.get("error").is_some() {
        return Err(format!("initialize rejected: {}", init_resp.get("error").unwrap()));
    }

    let initialized_notif = transport::build_notification("notifications/initialized", None);
    backend.send_notification(&initialized_notif).await
        .map_err(|e| format!("send initialized notification: {}", e))?;

    Ok(backend)
}
