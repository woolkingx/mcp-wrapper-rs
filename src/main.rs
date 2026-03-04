//! MCP Wrapper - Universal lightweight proxy (raw JSON-RPC, no rmcp)
//!
//! Usage: mcp-wrapper-rs <command> [args...]
//!        mcp-wrapper-rs --init-timeout <secs> <command> [args...]
//!
//! Design:
//! - On startup, spawns temporary backend to cache tools/prompts/resources/server_info
//! - list/init requests served instantly from cache
//! - tools/call and other pass-through requests spawn a persistent backend on demand
//! - Raw JSON-RPC over stdin/stdout — no SDK dependency for protocol handling

mod cache;
mod proxy;
mod router;
mod transport;

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_appender::non_blocking::WorkerGuard;

use crate::proxy::{Backend, ChildPgids};
use crate::router::Route;

// ── Logging helpers ─────────────────────────────────────────────────

/// Resolve log directory: $XDG_RUNTIME_DIR/mcp-wrapper > $TMPDIR > /tmp
fn log_dir() -> String {
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR") {
        let dir = format!("{}/mcp-wrapper", xdg);
        if std::fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    if let Ok(tmp) = env::var("TMPDIR") {
        return tmp;
    }
    "/tmp".to_string()
}

/// Compute 8-char hex hash from cmd + args for unique log file naming.
fn cmd_hash(cmd: &str, args: &[String]) -> String {
    let mut h = DefaultHasher::new();
    cmd.hash(&mut h);
    args.hash(&mut h);
    format!("{:016x}", h.finish())[..8].to_string()
}

/// Initialize tracing to file if MCP_WRAPPER_DEBUG is set.
/// Returns WorkerGuard that must be kept alive for the duration of the program.
fn init_tracing(cmd: &str, args: &[String]) -> Option<WorkerGuard> {
    let level_str = env::var("MCP_WRAPPER_DEBUG").ok()?;

    let level = match level_str.to_lowercase().as_str() {
        "1" | "info" => "info",
        "2" | "warn" => "warn",
        "3" | "debug" => "debug",
        _ => "info",
    };

    let file_name = if let Ok(name) = env::var("MCP_SERVER_NAME") {
        format!("mcp-wrapper-{}.log", sanitize_name(&name))
    } else {
        let name = infer_mcp_name(cmd, args);
        let hash = cmd_hash(cmd, args);
        format!("mcp-wrapper-{}-{}.log", name, hash)
    };

    let appender = tracing_appender::rolling::never(log_dir(), &file_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(format!("mcp_wrapper_rs={}", level))
        .with_target(false)
        .with_thread_ids(false)
        .init();

    Some(guard)
}

fn infer_mcp_name(cmd: &str, args: &[String]) -> String {
    if let Ok(name) = env::var("MCP_SERVER_NAME") {
        return sanitize_name(&name);
    }
    if cmd == "npx" && !args.is_empty() {
        for arg in args {
            if !arg.starts_with('-') {
                let name = arg.split('/').last().unwrap_or(arg);
                let name = name.split('@').next().unwrap_or(name);
                return sanitize_name(name);
            }
        }
    }
    if (cmd == "python3" || cmd == "python") && !args.is_empty() {
        if let Some(script) = args.first() {
            if let Some(name) = Path::new(script).file_stem() {
                return sanitize_name(name.to_string_lossy().as_ref());
            }
        }
    }
    if cmd.ends_with(".sh") {
        if let Some(name) = Path::new(cmd).file_stem() {
            return sanitize_name(name.to_string_lossy().as_ref());
        }
    }
    sanitize_name(cmd)
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Idle reaper / active call guard ─────────────────────────────────

const BACKEND_IDLE_SECS: u64 = 60;

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

// ── CLI ─────────────────────────────────────────────────────────────

fn print_usage(program: &str) {
    eprintln!("mcp-wrapper-rs - Universal lightweight MCP proxy");
    eprintln!();
    eprintln!("Usage: {} [--init-timeout <secs>] <command> [args...]", program);
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --version, -V              Show version and exit");
    eprintln!("  --help, -h                 Show this help and exit");
    eprintln!("  --init-timeout <secs>      Seconds to wait for subprocess init handshake (default: 30)");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  {} python3 server.py", program);
    eprintln!("  {} npx -y @anthropics/mcp-searxng", program);
    eprintln!("  {} --init-timeout 10 codex mcp-server", program);
    eprintln!();
    eprintln!("Environment Variables:");
    eprintln!("  MCP_WRAPPER_DEBUG=N    Enable logging: 1/info, 2/warn, 3/debug (to $XDG_RUNTIME_DIR/mcp-wrapper/ or /tmp)");
    eprintln!("  MCP_SERVER_NAME=xxx    Override inferred server name for logs");
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Handle flags before starting runtime
    if args.len() >= 2 && args[1].starts_with('-') {
        match args[1].as_str() {
            "--version" | "-V" => {
                println!("mcp-wrapper-rs {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                print_usage(&args[0]);
                return;
            }
            "--init-timeout" => {
                if args.len() < 3 {
                    eprintln!("Error: --init-timeout requires a value");
                    eprintln!();
                    print_usage(&args[0]);
                    std::process::exit(1);
                }
            }
            unknown_flag => {
                eprintln!("Error: Unknown option: {}", unknown_flag);
                eprintln!();
                print_usage(&args[0]);
                std::process::exit(1);
            }
        }
    }

    if args.len() < 2 {
        eprintln!("Error: Missing <command> argument");
        eprintln!();
        print_usage(&args[0]);
        std::process::exit(1);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    rt.block_on(async_main(args));
}

// ── Async main loop ─────────────────────────────────────────────────

async fn async_main(args: Vec<String>) {
    // Parse --init-timeout <secs>
    let (init_timeout, cmd_start) = if args.len() >= 2 && args[1] == "--init-timeout" {
        let secs: u64 = match args.get(2).and_then(|s| s.parse().ok()) {
            Some(n) => n,
            None => {
                eprintln!("Error: --init-timeout requires a positive integer");
                std::process::exit(1);
            }
        };
        if secs == 0 {
            eprintln!("Error: --init-timeout must be greater than 0");
            std::process::exit(1);
        }
        (Duration::from_secs(secs), 3)
    } else {
        (Duration::from_secs(30), 1)
    };

    if args.len() <= cmd_start {
        eprintln!("Error: Missing <command> argument");
        eprintln!();
        print_usage(&args[0]);
        std::process::exit(1);
    }

    let cmd = args[cmd_start].clone();
    let cmd_args: Vec<String> = args[cmd_start + 1..].to_vec();

    let _tracing_guard = init_tracing(&cmd, &cmd_args);
    info!(
        cmd = %cmd,
        init_timeout_secs = init_timeout.as_secs(),
        version = env!("CARGO_PKG_VERSION"),
        "started"
    );
    debug!(args = ?cmd_args, "startup args");

    // Register signal handlers before init (interruptible init)
    #[cfg(unix)]
    let mut sigterm = match tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    ) {
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

    let child_pgids: ChildPgids = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Build cache — race against signals so init is interruptible
    let cache = tokio::select! {
        result = cache::init_cache(&cmd, &cmd_args, init_timeout, &child_pgids) => {
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

    // Shared state for idle reaper
    let last_activity = Arc::new(AtomicU64::new(now_millis()));
    let active_calls = Arc::new(AtomicUsize::new(0));

    // Backend notification channel: backend→client
    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<Value>();

    // Lazy backend: None until first PassThrough request
    let backend: Arc<tokio::sync::Mutex<Option<Backend>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // Stdin/stdout
    let stdin = tokio::io::stdin();
    let mut stdin_reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();

    // Idle reaper task
    let reaper_backend = backend.clone();
    let reaper_last_activity = last_activity.clone();
    let reaper_active_calls = active_calls.clone();
    let reaper_child_pgids = child_pgids.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(BACKEND_IDLE_SECS)).await;
            let now = now_millis();
            let last = reaper_last_activity.load(Ordering::Relaxed);
            let idle_for = now.saturating_sub(last);
            if idle_for < BACKEND_IDLE_SECS * 1000 {
                continue;
            }
            if reaper_active_calls.load(Ordering::Relaxed) > 0 {
                continue;
            }
            let mut guard = reaper_backend.lock().await;
            if let Some(be) = guard.take() {
                info!("backend: idle timeout, shutting down");
                be.kill().await;
                // pgid stays in child_pgids for final cleanup — harmless double-kill
                let _ = &reaper_child_pgids;
            }
        }
    });

    // ── Main event loop ─────────────────────────────────────────────
    loop {
        tokio::select! {
            msg_opt = transport::read_message(&mut stdin_reader) => {
                let raw = match msg_opt {
                    Some(v) => v,
                    None => {
                        info!("stdin: EOF");
                        break;
                    }
                };
                match transport::classify(&raw) {
                    transport::JsonRpcMessage::Request { id, method, .. } => {
                        let resp = handle_request(
                            id,
                            &method,
                            raw,
                            &cache,
                            &backend,
                            &cmd,
                            &cmd_args,
                            init_timeout,
                            &child_pgids,
                            &notif_tx,
                            &active_calls,
                            &last_activity,
                        ).await;
                        if let Err(e) = transport::write_message(&mut stdout, &resp).await {
                            warn!(err = %e, "stdout: write error");
                            break;
                        }
                    }
                    transport::JsonRpcMessage::Notification { method, .. } => {
                        if method.is_empty() {
                            debug!("malformed message (no method, no id), skipping");
                            continue;
                        }
                        // Forward client notifications to backend if alive
                        let guard = backend.lock().await;
                        if let Some(be) = guard.as_ref() {
                            if be.is_alive() {
                                if let Err(e) = be.send_notification(&raw).await {
                                    warn!(err = %e, method = %method, "forward notification failed");
                                }
                            }
                        }
                    }
                    transport::JsonRpcMessage::Response { .. } => {
                        // Client should not send responses to us; ignore
                        debug!("unexpected response from client, ignoring");
                    }
                }
            }
            Some(notif) = notif_rx.recv() => {
                // Backend→client notification relay
                let method = notif.get("method").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(key) = router::is_list_changed_notification(method) {
                    info!(method = %method, "cache invalidation notification");
                    // Refresh cache by querying backend
                    let guard = backend.lock().await;
                    if let Some(be) = guard.as_ref() {
                        if be.is_alive() {
                            let list_method = router::list_method_for_key(&key);
                            let req = transport::build_request(
                                be.next_request_id(),
                                list_method,
                                None,
                            );
                            match tokio::time::timeout(
                                Duration::from_secs(5),
                                be.send_request(req),
                            ).await {
                                Ok(Ok(resp)) => {
                                    if let Some(result) = resp.get("result").cloned() {
                                        cache.update(&key, result);
                                        info!(method = list_method, "cache refreshed");
                                    }
                                }
                                Ok(Err(e)) => warn!(err = %e, "cache refresh failed"),
                                Err(_) => warn!("cache refresh timeout"),
                            }
                        }
                    }
                }
                // Always relay the notification to the client
                if let Err(e) = transport::write_message(&mut stdout, &notif).await {
                    warn!(err = %e, "stdout: write notification error");
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

    // Shutdown: kill all child process groups
    kill_all_pgids(&child_pgids);
    info!("shutdown");
}

// ── Request handler ─────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    client_id: Value,
    method: &str,
    raw: Value,
    cache: &Arc<cache::Cache>,
    backend: &Arc<tokio::sync::Mutex<Option<Backend>>>,
    cmd: &str,
    cmd_args: &[String],
    init_timeout: Duration,
    child_pgids: &ChildPgids,
    notif_tx: &mpsc::UnboundedSender<Value>,
    active_calls: &Arc<AtomicUsize>,
    last_activity: &Arc<AtomicU64>,
) -> Value {
    match router::route(method) {
        Route::Cache(key) => {
            debug!(method = %method, "serving from cache");
            match cache.lookup(&key) {
                Some(result) => transport::build_response(client_id, result),
                None => transport::build_error_response(
                    client_id,
                    transport::error_codes::INTERNAL_ERROR,
                    "cache miss",
                    None,
                ),
            }
        }
        Route::Local => {
            // ping: empty success
            debug!(method = %method, "local handler");
            transport::build_response(client_id, Value::Object(serde_json::Map::new()))
        }
        Route::PassThrough => {
            let t0 = std::time::Instant::now();
            info!(method = %method, "pass-through");

            let _guard = ActiveCallGuard::new(active_calls.clone());

            // Ensure backend is alive, spawn with retry if needed
            let be_result =
                ensure_backend(backend, cmd, cmd_args, init_timeout, child_pgids, notif_tx).await;
            let be_lock = match be_result {
                Ok(()) => backend.lock().await,
                Err(msg) => {
                    warn!(method = %method, err = %msg, "backend spawn failed");
                    return transport::build_error_response(
                        client_id,
                        transport::error_codes::INTERNAL_ERROR,
                        &msg,
                        None,
                    );
                }
            };
            let be = match be_lock.as_ref() {
                Some(be) => be,
                None => {
                    return transport::build_error_response(
                        client_id,
                        transport::error_codes::INTERNAL_ERROR,
                        "backend unavailable",
                        None,
                    );
                }
            };

            // Remap ID: replace client's id with backend-generated id
            let backend_id = be.next_request_id();
            let mut forwarded = raw;
            forwarded["id"] = backend_id.clone();

            // No proxy-side timeout: the client controls cancellation by
            // closing the connection. Imposing an artificial limit here
            // would cut off long-running tool calls prematurely.
            let resp = match be.send_request(forwarded).await {
                Ok(mut resp) => {
                    // Remap response id back to client's original
                    resp["id"] = client_id.clone();
                    let elapsed = t0.elapsed().as_millis();
                    info!(method = %method, elapsed_ms = elapsed, "pass-through done");
                    resp
                }
                Err(e) => {
                    let elapsed = t0.elapsed().as_millis();
                    let stderr = be.stderr_snapshot().await;
                    let detail = if stderr.is_empty() {
                        format!("{}", e)
                    } else {
                        format!("{}\nstderr: {}", e, stderr.trim())
                    };
                    warn!(method = %method, elapsed_ms = elapsed, err = %detail, "pass-through failed");
                    // Backend likely dead — drop it so next request respawns
                    drop(be_lock);
                    let mut guard = backend.lock().await;
                    if let Some(dead_be) = guard.take() {
                        dead_be.kill().await;
                    }
                    transport::build_error_response(
                        client_id,
                        transport::error_codes::INTERNAL_ERROR,
                        &detail,
                        None,
                    )
                }
            };

            last_activity.store(now_millis(), Ordering::Relaxed);
            resp
        }
    }
}

// ── Backend lifecycle ───────────────────────────────────────────────

/// Ensure a live backend exists. Spawns with retry (up to 3 attempts,
/// exponential backoff 1s/2s/4s) if no backend is running.
async fn ensure_backend(
    backend: &Arc<tokio::sync::Mutex<Option<Backend>>>,
    cmd: &str,
    cmd_args: &[String],
    init_timeout: Duration,
    child_pgids: &ChildPgids,
    notif_tx: &mpsc::UnboundedSender<Value>,
) -> Result<(), String> {
    {
        let guard = backend.lock().await;
        if let Some(be) = guard.as_ref() {
            if be.is_alive() {
                debug!("backend: reusing");
                return Ok(());
            }
            warn!("backend: died, will respawn");
        }
    }

    // Spawn outside the lock, then re-acquire to install.
    // TOCTOU: between the is_alive() check above (lock released) and
    // spawn below, another task could also spawn. We accept this race —
    // the guard.take() below kills any stale backend that appeared in
    // the gap, so at most one extra process is briefly alive.
    let backoff = [1, 2, 4];
    for (attempt, delay) in backoff.iter().enumerate() {
        info!(attempt = attempt + 1, "backend: spawning");
        match spawn_and_init_backend(cmd, cmd_args, init_timeout, child_pgids, notif_tx).await {
            Ok(new_be) => {
                info!("backend: ready");
                let mut guard = backend.lock().await;
                if let Some(old) = guard.take() {
                    old.kill().await;
                }
                *guard = Some(new_be);
                return Ok(());
            }
            Err(e) => {
                warn!(attempt = attempt + 1, err = %e, "backend: spawn failed");
                if attempt < backoff.len() - 1 {
                    tokio::time::sleep(Duration::from_secs(*delay)).await;
                }
            }
        }
    }

    // Clear dead backend
    let mut guard = backend.lock().await;
    if let Some(dead) = guard.take() {
        dead.kill().await;
    }
    Err("backend spawn failed after 3 attempts".to_string())
}

/// Spawn a new backend and run the MCP initialize handshake.
///
/// Performs the full sequence: spawn → initialize request → wait for response
/// → send notifications/initialized. Without this, the backend MCP server
/// won't serve any requests.
async fn spawn_and_init_backend(
    cmd: &str,
    cmd_args: &[String],
    init_timeout: Duration,
    child_pgids: &ChildPgids,
    notif_tx: &mpsc::UnboundedSender<Value>,
) -> Result<Backend, String> {
    let backend = Backend::spawn(cmd, cmd_args, notif_tx.clone(), child_pgids)
        .map_err(|e| format!("spawn: {}", e))?;

    let init_req = cache::build_initialize_request(backend.next_request_id());
    let init_resp = tokio::time::timeout(init_timeout, backend.send_request(init_req))
        .await
        .map_err(|_| "initialize handshake timeout".to_string())?
        .map_err(|e| format!("initialize handshake: {}", e))?;

    if init_resp.get("error").is_some() {
        return Err(format!(
            "initialize rejected: {}",
            init_resp.get("error").unwrap()
        ));
    }

    let initialized_notif =
        crate::transport::build_notification("notifications/initialized", None);
    backend
        .send_notification(&initialized_notif)
        .await
        .map_err(|e| format!("send initialized notification: {}", e))?;

    Ok(backend)
}

/// Kill all child process groups. Safe to call multiple times.
///
/// Uses SIGKILL intentionally: this is last-resort cleanup on wrapper exit.
/// Individual backends are already killed gracefully via `Backend::kill()`
/// (SIGTERM → wait → SIGKILL) during idle reaper or normal shutdown.
fn kill_all_pgids(child_pgids: &ChildPgids) {
    let pgids = child_pgids.lock().unwrap().clone();
    for pgid in &pgids {
        info!(pgid = pgid, "killing child process group");
        unsafe {
            libc::kill(-(*pgid as libc::pid_t), libc::SIGKILL);
        }
    }
}
