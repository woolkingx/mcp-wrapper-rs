//! MCP Wrapper - Universal lightweight proxy using rmcp SDK
//!
//! Usage: mcp-wrapper-rs <command> [args...]
//!        mcp-wrapper-rs --init-timeout <secs> <command> [args...]
//!
//! Design:
//! - On startup, spawn subprocess via rmcp client to cache tools/prompts/resources
//! - list/init requests are served instantly from cache
//! - tools/call spawns a persistent backend subprocess on demand
//! - rmcp handles all JSON-RPC protocol details

use rmcp::{
    ServerHandler, ServiceExt,
    model::*,
    service::{RequestContext, RoleClient, RoleServer, RunningService},
    transport::{TokioChildProcess, io::stdio},
};
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, info, warn};
use tracing_appender::non_blocking::WorkerGuard;

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
/// Level: MCP_WRAPPER_DEBUG=1 or =info → INFO+, =debug → DEBUG+
fn init_tracing(cmd: &str, args: &[String]) -> Option<WorkerGuard> {
    let level_str = env::var("MCP_WRAPPER_DEBUG").ok()?;

    let level = match level_str.to_lowercase().as_str() {
        "1" | "info"    => "info",
        "2" | "warn"    => "warn",
        "3" | "debug"   => "debug",
        _               => "info",
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

/// Spawn a stderr reader task that tees output to tracing and a shared buffer.
/// The buffer holds the last ~4KB of stderr for error reporting.
fn spawn_stderr_reader(
    mut stderr: tokio::process::ChildStderr,
) -> Arc<Mutex<String>> {
    let buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let buf_clone = buf.clone();
    tokio::spawn(async move {
        let mut chunk = vec![0u8; 1024];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) => break,
                Err(_) => {
                    warn!("backend stderr: read error");
                    break;
                }
                Ok(n) => {
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    for line in text.lines() {
                        debug!(line, "backend stderr");
                    }
                    // Keep last ~4KB in buffer for error reporting
                    if let Ok(mut guard) = buf_clone.lock() {
                        guard.push_str(&text);
                        if guard.len() > 4096 {
                            let trim_at = guard.len() - 4096;
                            *guard = guard[trim_at..].to_string();
                        }
                    }
                }
            }
        }
    });
    buf
}

// === McpProxy: implements ServerHandler, proxies to real MCP server ===

const BACKEND_IDLE_SECS: u64 = 60;

type Backend = tokio::sync::Mutex<Option<(RunningService<RoleClient, ()>, Arc<Mutex<String>>)>>;

struct McpProxy {
    cmd: String,
    cmd_args: Vec<String>,
    init_timeout: Duration,
    cached_tools: ListToolsResult,
    cached_prompts: ListPromptsResult,
    cached_resources: ListResourcesResult,
    cached_resource_templates: ListResourceTemplatesResult,
    server_info: ServerInfo,
    backend: Arc<Backend>,
    idle_notify: Arc<tokio::sync::Notify>,
    active_calls: Arc<AtomicUsize>,
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

impl McpProxy {
    fn spawn_child(cmd: &str, cmd_args: &[String]) -> std::io::Result<(TokioChildProcess, Arc<Mutex<String>>)> {
        let mut command = Command::new(cmd);
        command.args(cmd_args);
        let (proc, stderr_opt) = TokioChildProcess::builder(command)
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr_buf = match stderr_opt {
            Some(stderr) => spawn_stderr_reader(stderr),
            None => Arc::new(Mutex::new(String::new())),
        };
        Ok((proc, stderr_buf))
    }

    async fn new(cmd: String, cmd_args: Vec<String>, init_timeout: Duration) -> Result<Self, Box<dyn std::error::Error>> {
        debug!("init: spawning subprocess");
        let (transport, stderr_buf) = Self::spawn_child(&cmd, &cmd_args)?;
        Self::init_from_transport(cmd, cmd_args, transport, stderr_buf, init_timeout).await
    }

    /// Build a proxy by querying an already-created transport.
    /// Exposed for testing with in-process transports (tokio::io::duplex).
    pub async fn init_from_transport<T>(
        cmd: String,
        cmd_args: Vec<String>,
        transport: T,
        stderr_buf: Arc<Mutex<String>>,
        init_timeout: Duration,
    ) -> Result<Self, Box<dyn std::error::Error>>
    where
        T: rmcp::transport::Transport<RoleClient> + Send + 'static,
        T::Error: std::error::Error + Send + Sync + 'static,
    {
        let client: RunningService<RoleClient, ()> = tokio::time::timeout(
            init_timeout,
            ().serve(transport),
        ).await
            .map_err(|_| format!("init handshake timeout ({}s)", init_timeout.as_secs()))??;
        let peer = client.peer().clone();

        // Query and cache all lists (with pagination support via list_all_*)
        // Short timeout: supported lists respond in ms; unsupported ones hang forever.
        let list_timeout = Duration::from_secs(5);

        let (tools, prompts, resources, resource_templates) = tokio::join!(
            tokio::time::timeout(list_timeout, peer.list_all_tools()),
            tokio::time::timeout(list_timeout, peer.list_all_prompts()),
            tokio::time::timeout(list_timeout, peer.list_all_resources()),
            tokio::time::timeout(list_timeout, peer.list_all_resource_templates()),
        );

        let tools = match tools {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { warn!(err = %e, "init: list_tools failed"); vec![] }
            Err(_) => { warn!("init: list_tools timeout"); vec![] }
        };
        let cached_tools = ListToolsResult::with_all_items(tools);

        let prompts = match prompts {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { warn!(err = %e, "init: list_prompts failed"); vec![] }
            Err(_) => { warn!("init: list_prompts timeout"); vec![] }
        };
        let cached_prompts = ListPromptsResult::with_all_items(prompts);

        let resources = match resources {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { warn!(err = %e, "init: list_resources failed"); vec![] }
            Err(_) => { warn!("init: list_resources timeout"); vec![] }
        };
        let cached_resources = ListResourcesResult::with_all_items(resources);

        let resource_templates = match resource_templates {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { warn!(err = %e, "init: list_resource_templates failed"); vec![] }
            Err(_) => { warn!("init: list_resource_templates timeout"); vec![] }
        };
        let cached_resource_templates = ListResourceTemplatesResult::with_all_items(resource_templates);

        // Cache server info from the initialize handshake
        let server_info = peer.peer_info()
            .cloned()
            .unwrap_or_default();

        debug!(
            tools = cached_tools.tools.len(),
            prompts = cached_prompts.prompts.len(),
            resources = cached_resources.resources.len(),
            resource_templates = cached_resource_templates.resource_templates.len(),
            server = ?server_info.server_info,
            "init: cached"
        );

        Ok(Self {
            cmd,
            cmd_args,
            init_timeout,
            cached_tools,
            cached_prompts,
            cached_resources,
            cached_resource_templates,
            server_info,
            backend: Arc::new(tokio::sync::Mutex::new(Some((client, stderr_buf)))),
            idle_notify: Arc::new(tokio::sync::Notify::new()),
            active_calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Lazy-spawn persistent backend. First call does init handshake,
    /// subsequent calls reuse the existing connection.
    /// All subprocess operations use init_timeout as the unified timeout.
    /// Backend is killed after BACKEND_IDLE_SECS of inactivity.
    async fn ensure_backend(&self) -> Result<(rmcp::service::Peer<RoleClient>, Arc<Mutex<String>>), ErrorData> {
        let mut guard = self.backend.lock().await;
        if let Some((ref running, ref stderr_buf)) = *guard {
            if !running.is_closed() {
                debug!("backend: reusing");
                return Ok((running.peer().clone(), stderr_buf.clone()));
            }
            warn!("backend: died, respawning");
        }

        info!("backend: spawning");
        let (transport, stderr_buf) = Self::spawn_child(&self.cmd, &self.cmd_args)
            .map_err(|e| {
                warn!(err = %e, "backend: spawn failed");
                ErrorData::internal_error(format!("spawn: {}", e), None)
            })?;

        let running: RunningService<RoleClient, ()> = tokio::time::timeout(
            self.init_timeout,
            ().serve(transport),
        ).await
            .map_err(|_| {
                warn!(timeout_secs = self.init_timeout.as_secs(), "backend: init timeout");
                ErrorData::internal_error("backend init timeout", None)
            })?
            .map_err(|e| {
                warn!(err = %e, "backend: init failed");
                ErrorData::internal_error(format!("backend init: {}", e), None)
            })?;
        info!("backend: ready");

        let peer = running.peer().clone();
        let buf = stderr_buf.clone();
        *guard = Some((running, stderr_buf));
        Ok((peer, buf))
    }

    /// Reset the idle timer. Call after each tool call completes.
    fn touch_idle(&self) {
        self.idle_notify.notify_one();
    }
}

impl ServerHandler for McpProxy {
    fn get_info(&self) -> ServerInfo {
        self.server_info.clone()
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(self.cached_tools.clone()))
    }

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(self.cached_prompts.clone()))
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(self.cached_resources.clone()))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(self.cached_resource_templates.clone()))
    }

    fn ping(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + Send + '_ {
        std::future::ready(Ok(()))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        async move {
            let name = request.name.clone();
            let args_str = request.arguments.as_ref()
                .map(|v| format!("{:?}", v))
                .unwrap_or_default();
            let arg_size = args_str.len();
            let t0 = std::time::Instant::now();
            info!(name = %name, arg_size = arg_size, "call_tool");
            debug!(name = %name, args = %args_str, "call_tool args");

            let _active_call_guard = ActiveCallGuard::new(self.active_calls.clone());
            self.touch_idle();
            let (peer, stderr_buf) = self.ensure_backend().await?;

            // No wrapper timeout — client decides how long to wait.
            // If backend dies, peer.call_tool() returns Err (pipe broken).
            let result = peer.call_tool(request).await.map_err(|e| {
                let stderr = stderr_buf.lock()
                    .ok()
                    .and_then(|g| if g.is_empty() { None } else { Some(g.clone()) })
                    .map(|s| format!("\nstderr: {}", s.trim()))
                    .unwrap_or_default();
                ErrorData::internal_error(format!("call_tool: {}{}", e, stderr), None)
            });

            let elapsed = t0.elapsed().as_millis();
            match &result {
                Ok(r) => {
                    let result_str = format!("{:?}", r);
                    let result_size = result_str.len();
                    info!(name = %name, elapsed_ms = elapsed, arg_size = arg_size, result_size = result_size, "call_tool done");
                    debug!(name = %name, result = %result_str, "call_tool result");
                }
                Err(ref e) => {
                    warn!(name = %name, elapsed_ms = elapsed, err = %e.message, "call_tool failed");
                }
            }
            self.touch_idle();
            result
        }
    }
}

// === CLI and main ===

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
                // Validated below after runtime setup; just verify arg exists
                if args.len() < 3 {
                    eprintln!("Error: --init-timeout requires a value");
                    eprintln!();
                    print_usage(&args[0]);
                    std::process::exit(1);
                }
                // Falls through to runtime
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

async fn async_main(args: Vec<String>) {
    // Parse --init-timeout <secs> if present
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

    // Initialize tracing
    let _tracing_guard = init_tracing(&cmd, &cmd_args);

    info!(cmd = %cmd, init_timeout_secs = init_timeout.as_secs(), version = env!("CARGO_PKG_VERSION"), "started");
    debug!(args = ?cmd_args, "startup args");

    // Register signal handlers BEFORE init_cache (mirrors v0.1.2 design).
    // Claude Code sends SIGINT on exit. If we only poll signals after serve(),
    // a SIGINT during init hits unguarded code → process::exit → OS closes
    // pipes abruptly → Claude Code's stdio onclose fires → marks "failed".
    // By arming signals first we can return cleanly at any phase.
    //
    // SIGTERM is Unix-only. On non-Unix, sigterm_recv() returns a future that
    // never resolves (std::future::pending), so the branch never fires.
    #[cfg(unix)]
    let mut sigterm = match tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate()
    ) {
        Ok(signal) => Some(signal),
        Err(e) => {
            warn!(err = %e, "failed to register SIGTERM handler; continuing without SIGTERM handling");
            None
        }
    };

    macro_rules! sigterm_recv {
        () => {{
            #[cfg(unix)] {
                async {
                    match sigterm.as_mut() {
                        Some(signal) => signal.recv().await,
                        None => std::future::pending::<Option<()>>().await,
                    }
                }
            }
            #[cfg(not(unix))] { std::future::pending::<Option<()>>() }
        }}
    }

    // Build proxy — race against signals so init is interruptible.
    let proxy = tokio::select! {
        result = McpProxy::new(cmd, cmd_args, init_timeout) => {
            match result {
                Ok(p) => p,
                Err(e) => {
                    warn!(err = %e, "init failed");
                    eprintln!("Error: Failed to initialize MCP server: {}", e);
                    std::process::exit(1);
                }
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("signal: SIGINT during init");
            info!("shutdown");
            return; // clean return — drops tokio runtime, closes pipes gracefully
        }
        _ = sigterm_recv!() => {
            info!("signal: SIGTERM during init");
            info!("shutdown");
            return;
        }
    };

    info!("serving");

    // Grab Arc handles before proxy is moved into serve()
    let backend_handle = proxy.backend.clone();
    let idle_handle = proxy.idle_notify.clone();
    let active_calls_handle = proxy.active_calls.clone();

    let server = match proxy.serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            warn!(err = %e, "serve error");
            eprintln!("Error: Failed to start server: {}", e);
            std::process::exit(1);
        }
    };
    idle_handle.notify_one();

    // Idle reaper: kill backend after BACKEND_IDLE_SECS of inactivity
    tokio::spawn(async move {
        loop {
            // Wait for first activity, then start idle countdown
            idle_handle.notified().await;
            debug!("idle: timer started");
            loop {
                let timeout = tokio::time::timeout(
                    Duration::from_secs(BACKEND_IDLE_SECS),
                    idle_handle.notified(),
                ).await;
                if timeout.is_err() {
                    // Timed out — but don't kill if calls are in flight
                    if active_calls_handle.load(Ordering::Relaxed) > 0 {
                        debug!("idle: calls in flight, deferring");
                        continue; // reset timer, check again later
                    }
                    let mut guard = backend_handle.lock().await;
                    if guard.is_some() {
                        info!("backend: idle timeout, shutting down");
                        *guard = None; // drop kills the subprocess
                    }
                    break; // back to waiting for first activity
                }
                // Got notified — reset timer
                debug!("idle: timer reset");
            }
        }
    });

    match server.waiting().await {
        Ok(_) => {}
        Err(e) => warn!(err = %e, "server waiting error"),
    }
    info!("shutdown");
}
