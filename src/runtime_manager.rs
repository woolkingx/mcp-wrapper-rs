//! Normal runtime owner: stdio event loop, active-call tracking, idle reaper,
//! and request dispatch composition.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::io::BufReader;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::backend_manager::{BackendEvent, ChildPgids};
use crate::event_plane::{Dir, EventProgramId};
use crate::mcp_interface::Route;
use crate::{
    backend_manager, event_plane, lifetime, logging, mcp_interface, mcp_manager, timeouts, tools,
};

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct ActiveCallGuard {
    active_calls: Arc<AtomicUsize>,
}

impl ActiveCallGuard {
    fn new(active_calls: Arc<AtomicUsize>) -> Self {
        active_calls.fetch_add(1, Ordering::Relaxed);
        Self { active_calls }
    }
}

impl Drop for ActiveCallGuard {
    fn drop(&mut self) {
        self.active_calls.fetch_sub(1, Ordering::Relaxed);
    }
}

pub async fn run_normal_command(init_timeout: Duration, cmd: String, cmd_args: Vec<String>) {
    let _tracing_guard = logging::init_tracing(&cmd, &cmd_args);
    info!(
        cmd = %cmd,
        init_timeout_secs = init_timeout.as_secs(),
        version = env!("CARGO_PKG_VERSION"),
        "started"
    );
    debug!(args = ?cmd_args, "startup args");
    let request_timeout = match timeouts::PassThroughRequestTimeout::from_env() {
        Ok(timeout) => timeout.duration(),
        Err(msg) => {
            warn!(err = %msg, "request timeout disabled");
            None
        }
    };
    let request_lifetime = lifetime::RequestLifetime::new();

    #[cfg(unix)]
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(signal) => Some(signal),
        Err(e) => {
            warn!(err = %e, "failed to register SIGTERM handler");
            None
        }
    };

    macro_rules! sigterm_recv {
        () => {{
            #[cfg(unix)]
            {
                async {
                    match sigterm.as_mut() {
                        Some(signal) => signal.recv().await,
                        None => std::future::pending::<Option<()>>().await,
                    }
                }
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<Option<()>>()
            }
        }};
    }

    let child_pgids: ChildPgids = backend_manager::new_child_pgids();

    let cache = tokio::select! {
        result = mcp_manager::init_cache(&cmd, &cmd_args, init_timeout, &child_pgids) => {
            match result {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    warn!(err = %e, "init failed");
                    eprintln!("Error: Failed to initialize MCP server: {}", e);
                    std::process::exit(1);
                }
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("signal: SIGINT during init");
            kill_all_pgids(&child_pgids);
            info!("shutdown");
            return;
        }
        _ = sigterm_recv!() => {
            info!("signal: SIGTERM during init");
            kill_all_pgids(&child_pgids);
            info!("shutdown");
            return;
        }
    };

    info!("serving");

    let last_activity = Arc::new(AtomicU64::new(now_millis()));
    let active_calls = Arc::new(AtomicUsize::new(0));
    let pass_through_lock = Arc::new(Mutex::new(()));

    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<BackendEvent>();

    let backend_slot = Arc::new(backend_manager::BackendSlot::new_empty(
        backend_manager::BackendSpec {
            cmd: cmd.clone(),
            args: cmd_args.clone(),
            init_timeout,
            child_pgids: child_pgids.clone(),
            notif_tx: notif_tx.clone(),
        },
    ));
    let backend = backend_slot.handle();

    let stdin = tokio::io::stdin();
    let mut stdin_reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let mut stdin_closed = false;

    let reaper_backend = backend.clone();
    let reaper_last_activity = last_activity.clone();
    let reaper_active_calls = active_calls.clone();
    let reaper_child_pgids = child_pgids.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(timeouts::NORMAL_IDLE_REAPER_SECS)).await;
            let now = now_millis();
            let last = reaper_last_activity.load(Ordering::Relaxed);
            let idle_for = now.saturating_sub(last);
            if idle_for < timeouts::NORMAL_IDLE_REAPER_SECS * 1000 {
                continue;
            }
            if reaper_active_calls.load(Ordering::Relaxed) > 0 {
                continue;
            }
            let mut guard = reaper_backend.lock().await;
            if let Some(be) = guard.take() {
                info!("backend: idle timeout, shutting down");
                be.kill().await;
                let _ = &reaper_child_pgids;
            }
        }
    });

    loop {
        if stdin_closed && active_calls.load(Ordering::Acquire) == 0 && out_rx.is_empty() {
            break;
        }

        tokio::select! {
            msg_opt = mcp_interface::read_message(&mut stdin_reader), if !stdin_closed => {
                let raw = match msg_opt {
                    Some(v) => v,
                    None => {
                        info!("stdin: EOF");
                        stdin_closed = true;
                        continue;
                    }
                };
                match mcp_interface::classify(&raw) {
                    mcp_interface::JsonRpcMessage::Request { id, method, .. } => {
                        handle_request(
                            id,
                            method,
                            raw,
                            cache.clone(),
                            backend_slot.clone(),
                            active_calls.clone(),
                            last_activity.clone(),
                            pass_through_lock.clone(),
                            request_lifetime.clone(),
                            request_timeout,
                            out_tx.clone(),
                        ).await;
                    }
                    mcp_interface::JsonRpcMessage::Notification { method, .. } => {
                        if method.is_empty() {
                            debug!("malformed message (no method, no id), skipping");
                            continue;
                        }
                        if event_plane::should_drop_client_notification(&method) {
                            debug!(method = %method, "dropping local client notification");
                            continue;
                        }
                        let be = {
                            let guard = backend.lock().await;
                            guard.as_ref().cloned()
                        };
                        if let Some(be) = be {
                            if be.is_alive() {
                                let notification_program =
                                    event_plane::notification_program(Dir::ClientToServer, &method);
                                if notification_program == Some(EventProgramId::CancelRequest) {
                                    if request_lifetime.cancel_client_request(&raw, &be).await {
                                        debug!(method = %method, "forwarded translated client cancellation");
                                    } else if request_lifetime
                                        .client_cancel_peer_request(&raw)
                                        .await
                                    {
                                        if let Err(e) = be.send_notification(&raw).await {
                                            warn!(err = %e, method = %method, "forward peer cancellation failed");
                                        }
                                    } else {
                                        debug!(method = %method, "dropping unmatched client cancellation");
                                    }
                                    continue;
                                }
                                if notification_program == Some(EventProgramId::ProgressRequest) {
                                    if request_lifetime
                                        .client_progress_is_active(&raw)
                                        .await
                                    {
                                        if let Err(e) = be.send_notification(&raw).await {
                                            warn!(err = %e, method = %method, "forward peer progress failed");
                                        }
                                    } else {
                                        debug!(method = %method, "dropping unmatched client progress notification");
                                    }
                                    continue;
                                }
                                if let Err(e) = be.send_notification(&raw).await {
                                    warn!(err = %e, method = %method, "forward notification failed");
                                }
                            }
                        }
                    }
                    mcp_interface::JsonRpcMessage::Response { .. } => {
                        if !request_lifetime.take_peer_response(&raw).await {
                            debug!("dropping unmatched client response");
                            continue;
                        }
                        let be = {
                            let guard = backend.lock().await;
                            guard.as_ref().cloned()
                        };
                        if let Some(be) = be {
                            if be.is_alive() {
                                if let Err(e) = be.send_message(&raw).await {
                                    warn!(err = %e, "forward client response failed");
                                }
                            }
                        }
                    }
                }
            }
            Some(event) = notif_rx.recv() => {
                match event {
                    BackendEvent::Notification(notif, ack) => {
                        let method = notif.get("method").and_then(|v| v.as_str()).unwrap_or("");
                        let notification_program =
                            event_plane::notification_program(Dir::ServerToClient, method);
                        if notification_program == Some(EventProgramId::ProgressRequest)
                            && !request_lifetime.backend_progress_is_active(&notif).await
                        {
                            debug!(method = %method, "dropping unmatched backend progress notification");
                            continue;
                        }
                        if notification_program == Some(EventProgramId::CancelRequest) {
                            let guard = backend.lock().await;
                            let be = guard.as_ref().cloned();
                            drop(guard);
                            let Some(be) = be else {
                                debug!(method = %method, "dropping backend cancellation without live backend");
                                continue;
                            };
                            let Some(translated) = request_lifetime
                                .translate_backend_cancel(&notif, &be)
                                .await
                            else {
                                debug!(method = %method, "dropping unmatched backend cancellation");
                                continue;
                            };
                            if out_tx.send(translated.message).is_err() {
                                warn!("stdout: outbound channel closed");
                                break;
                            }
                            continue;
                        }
                        let invalidated_keys = event_plane::list_changed_data_keys(method);
                        if !invalidated_keys.is_empty() {
                            info!(method = %method, "cache invalidation notification");
                            let backend = backend.clone();
                            let backend_slot = backend_slot.clone();
                            let cache = cache.clone();
                            let out_tx = out_tx.clone();
                            tokio::spawn(async move {
                                let guard = backend.lock().await;
                                let be = guard.as_ref().cloned();
                                drop(guard);
                                if let Some(be) = be {
                                    if be.is_alive() {
                                        match cache
                                            .refresh_keys(
                                                &backend_slot,
                                                &invalidated_keys,
                                            )
                                            .await
                                        {
                                            Ok(_) => {
                                                let _ = out_tx.send(notif);
                                            }
                                            Err(error) => {
                                                warn!(err = %error, "cache invalidation refresh aborted");
                                            }
                                        }
                                    }
                                }
                                drop(ack);
                            });
                            continue;
                        }
                        if out_tx.send(notif).is_err() {
                            warn!("stdout: outbound channel closed");
                            break;
                        }
                        drop(ack);
                    }
                    BackendEvent::PeerRequest(req) => {
                        request_lifetime.insert_peer_request(None, &req).await;
                        if out_tx.send(req).is_err() {
                            warn!("stdout: outbound channel closed");
                            break;
                        }
                    }
                    BackendEvent::UnmatchedResponse(resp) => {
                        warn!("backend: unmatched response");
                        if out_tx.send(resp).is_err() {
                            warn!("stdout: outbound channel closed");
                            break;
                        }
                    }
                    BackendEvent::ProcessExit => {
                        warn!("backend: process exit observed");
                        request_lifetime.clear_peer_requests().await;
                    }
                }
            }
            Some(out_msg) = out_rx.recv() => {
                if let Err(e) = mcp_interface::write_message(&mut stdout, &out_msg).await {
                    warn!(err = %e, "stdout: write error");
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("signal: SIGINT");
                break;
            }
            _ = sigterm_recv!() => {
                info!("signal: SIGTERM");
                break;
            }
        }
    }

    kill_all_pgids(&child_pgids);
    info!("shutdown");
}

async fn handle_request(
    client_id: Value,
    method: String,
    raw: Value,
    cache: Arc<mcp_manager::Cache>,
    backend_slot: Arc<backend_manager::BackendSlot>,
    active_calls: Arc<AtomicUsize>,
    last_activity: Arc<AtomicU64>,
    pass_through_lock: Arc<Mutex<()>>,
    request_lifetime: lifetime::RequestLifetime,
    request_timeout: Option<Duration>,
    out_tx: mpsc::UnboundedSender<Value>,
) {
    if event_plane::is_wrapper_tool_request(&raw) {
        let result = tools::invoke(
            &raw,
            tools::InvocationContext {
                cache: &cache,
                backend_slot: &backend_slot,
                active_calls: active_calls.load(Ordering::Acquire),
            },
        )
        .await;
        let _ = out_tx.send(mcp_interface::build_response(client_id, result));
        return;
    }

    let resp = match mcp_interface::route(&method) {
        Route::McpData(key) => {
            debug!(method = %method, "serving from cache");
            if mcp_manager::cached_list_request_has_cursor(&key, &raw) {
                mcp_interface::build_error_response(
                    client_id,
                    mcp_interface::error_codes::INVALID_PARAMS,
                    "cursor is not valid for cached list view",
                    None,
                )
            } else {
                match cache.lookup(&key) {
                    Some(result) => mcp_interface::build_response(client_id, result),
                    None => mcp_interface::build_error_response(
                        client_id,
                        mcp_interface::error_codes::INTERNAL_ERROR,
                        "cache miss",
                        None,
                    ),
                }
            }
        }
        Route::Local => {
            debug!(method = %method, "local handler");
            mcp_interface::build_response(client_id, Value::Object(serde_json::Map::new()))
        }
        Route::WrapperControl(control) => {
            handle_wrapper_control(
                client_id,
                control,
                raw,
                &cache,
                &backend_slot,
                &active_calls,
            )
            .await
        }
        Route::PassThrough => {
            let active_guard = ActiveCallGuard::new(active_calls.clone());
            let cache_for_task = cache.clone();
            tokio::spawn(async move {
                let _serialize_backend_call = pass_through_lock.lock().await;
                let t0 = std::time::Instant::now();
                info!(method = %method, "pass-through");

                let _guard = active_guard;

                let be_result = backend_slot.ensure().await;
                let backend = backend_slot.handle();
                let be = match be_result {
                    Ok(respawned) => {
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
                                let _ = out_tx.send(resp);
                                return;
                            }
                        };
                        if respawned {
                            match cache_for_task.refresh_all(&backend_slot).await {
                                Ok(report) => info!(
                                    old_cache_epoch = report.old_cache_epoch,
                                    new_cache_epoch = report.new_cache_epoch,
                                    changed = report.changed,
                                    "cache refreshed after backend respawn"
                                ),
                                Err(error) => {
                                    warn!(err = %error, "cache refresh after backend respawn aborted")
                                }
                            }
                        }
                        be
                    }
                    Err(msg) => {
                        warn!(method = %method, err = %msg, "backend spawn failed");
                        let resp = mcp_interface::build_error_response(
                            client_id,
                            mcp_interface::error_codes::INTERNAL_ERROR,
                            &msg,
                            None,
                        );
                        let _ = out_tx.send(resp);
                        return;
                    }
                };

                let backend_id = be.next_request_id();
                let mut forwarded = raw;
                forwarded["id"] = backend_id.clone();
                request_lifetime
                    .insert(client_id.clone(), backend_id.clone(), &forwarded)
                    .await;

                let request_result = match request_timeout {
                    Some(timeout) => be.send_request_with_timeout(forwarded, timeout).await,
                    None => be.send_request(forwarded).await,
                };
                if matches!(
                    request_result.as_ref().map_err(|e| e.kind()),
                    Err(std::io::ErrorKind::TimedOut)
                ) {
                    request_lifetime.timeout_client(&client_id).await;
                } else {
                    request_lifetime.remove_client(&client_id).await;
                }

                let resp = match request_result {
                    Ok(mut resp) => {
                        resp["id"] = client_id.clone();
                        let elapsed = t0.elapsed().as_millis();
                        info!(method = %method, elapsed_ms = elapsed, "pass-through done");
                        resp
                    }
                    Err(e) => {
                        let elapsed = t0.elapsed().as_millis();
                        if e.kind() == std::io::ErrorKind::TimedOut {
                            warn!(method = %method, elapsed_ms = elapsed, "pass-through timed out");
                            mcp_interface::build_error_response(
                                client_id,
                                mcp_interface::error_codes::INTERNAL_ERROR,
                                "request timed out",
                                None,
                            )
                        } else if be.is_discarding_response(&backend_id).await {
                            warn!(method = %method, elapsed_ms = elapsed, "pass-through cancelled");
                            return;
                        } else {
                            let stderr = be.stderr_snapshot().await;
                            let detail = if stderr.is_empty() {
                                format!("{}", e)
                            } else {
                                format!("{}\nstderr: {}", e, stderr.trim())
                            };
                            warn!(method = %method, elapsed_ms = elapsed, err = %detail, "pass-through failed");
                            let mut guard = backend.lock().await;
                            if let Some(dead_be) = guard.take() {
                                dead_be.kill().await;
                            }
                            mcp_interface::build_error_response(
                                client_id,
                                mcp_interface::error_codes::INTERNAL_ERROR,
                                &detail,
                                None,
                            )
                        }
                    }
                };

                last_activity.store(now_millis(), Ordering::Relaxed);
                let _ = out_tx.send(resp);
            });
            return;
        }
    };

    let _ = out_tx.send(resp);
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
            match be {
                Some(be) if be.is_alive() => {}
                _ => {
                    return mcp_interface::build_error_response(
                        client_id,
                        mcp_interface::error_codes::INTERNAL_ERROR,
                        "backend unavailable for refresh",
                        None,
                    );
                }
            }
            match cache.refresh_all(backend_slot).await {
                Ok(report) => mcp_interface::build_response(client_id, report.to_value()),
                Err(error) => mcp_interface::build_error_response(
                    client_id,
                    mcp_interface::error_codes::INTERNAL_ERROR,
                    &error,
                    None,
                ),
            }
        }
        mcp_interface::WrapperControlMethod::BackendRestart => {
            let force = raw
                .get("params")
                .and_then(|v| v.get("force"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if active_calls.load(Ordering::Acquire) > 0 && !force {
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
            let backend = backend_slot.handle();
            let be = {
                let guard = backend.lock().await;
                guard.as_ref().cloned()
            };
            match be {
                Some(be) if be.is_alive() => {}
                _ => {
                    return mcp_interface::build_error_response(
                        client_id,
                        mcp_interface::error_codes::INTERNAL_ERROR,
                        "backend unavailable after restart",
                        None,
                    );
                }
            }
            match cache.refresh_all(backend_slot).await {
                Ok(report) => mcp_interface::build_response(client_id, report.to_value()),
                Err(error) => mcp_interface::build_error_response(
                    client_id,
                    mcp_interface::error_codes::INTERNAL_ERROR,
                    &error,
                    None,
                ),
            }
        }
        mcp_interface::WrapperControlMethod::BackendStop => {
            if active_calls.load(Ordering::Acquire) > 0 {
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

fn kill_all_pgids(child_pgids: &ChildPgids) {
    backend_manager::kill_all_pgids(child_pgids);
}
