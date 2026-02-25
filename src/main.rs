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
    service::{RequestContext, RoleClient, RoleServer, RunningService, ServiceError},
    transport::{TokioChildProcess, io::stdio},
};
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
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

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
                    let mut guard = buf_clone.lock().await;
                    guard.push_str(&text);
                    if guard.len() > 4096 {
                        let trim_at = guard.len() - 4096;
                        *guard = guard[trim_at..].to_string();
                    }
                }
            }
        }
    });
    buf
}

// === McpProxy: implements ServerHandler, proxies to real MCP server ===

const BACKEND_IDLE_SECS: u64 = 60;

enum BackendState {
    Empty,
    Spawning(Arc<tokio::sync::Notify>),
    Ready(RunningService<RoleClient, ()>, Arc<Mutex<String>>),
}

type Backend = tokio::sync::Mutex<BackendState>;

struct McpProxy {
    cmd: String,
    cmd_args: Vec<String>,
    init_timeout: Duration,
    cached_tools: ListToolsResult,
    cached_prompts: ListPromptsResult,
    cached_resources: ListResourcesResult,
    cached_resource_templates: ListResourceTemplatesResult,
    server_info: ServerInfo,
    backend_capabilities: ServerCapabilities,
    backend: Arc<Backend>,
    last_activity: Arc<AtomicU64>,
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
        let backend_server_info = peer.peer_info()
            .cloned()
            .unwrap_or_default();
        let mut server_info = backend_server_info.clone();
        let backend_capabilities = backend_server_info.capabilities.clone();
        // Advertise only capabilities implemented by this proxy.
        // For now: tools only.
        server_info.capabilities = ServerCapabilities {
            tools: backend_capabilities.tools.clone(),
            ..ServerCapabilities::default()
        };

        if backend_capabilities.tools.is_some() && cached_tools.tools.is_empty() {
            warn!("init: tools capability advertised but list empty - capability may be stale");
        }
        if backend_capabilities.prompts.is_some() && cached_prompts.prompts.is_empty() {
            warn!("init: prompts capability advertised but list empty - capability may be stale");
        }
        if backend_capabilities.resources.is_some() && cached_resources.resources.is_empty() {
            warn!("init: resources capability advertised but list empty - capability may be stale");
        }

        debug!(
            tools = cached_tools.tools.len(),
            prompts = cached_prompts.prompts.len(),
            resources = cached_resources.resources.len(),
            resource_templates = cached_resource_templates.resource_templates.len(),
            server = ?backend_server_info.server_info,
            advertised_capabilities = ?server_info.capabilities,
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
            backend_capabilities,
            backend: Arc::new(tokio::sync::Mutex::new(BackendState::Ready(client, stderr_buf))),
            last_activity: Arc::new(AtomicU64::new(now_millis())),
            active_calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Lazy-spawn persistent backend. First call does init handshake,
    /// subsequent calls reuse the existing connection.
    /// All subprocess operations use init_timeout as the unified timeout.
    /// Backend is killed after BACKEND_IDLE_SECS of inactivity.
    async fn ensure_backend(&self) -> Result<(rmcp::service::Peer<RoleClient>, Arc<Mutex<String>>), ErrorData> {
        loop {
            enum Next {
                Wait(Arc<tokio::sync::Notify>),
                Spawn(Arc<tokio::sync::Notify>),
            }

            let next = {
                let mut guard = self.backend.lock().await;
                match &*guard {
                    BackendState::Ready(running, stderr_buf) => {
                        if !running.is_closed() {
                            debug!("backend: reusing");
                            return Ok((running.peer().clone(), stderr_buf.clone()));
                        }
                        warn!("backend: died, respawning");
                        let notify = Arc::new(tokio::sync::Notify::new());
                        *guard = BackendState::Spawning(notify.clone());
                        Next::Spawn(notify)
                    }
                    BackendState::Spawning(notify) => {
                        Next::Wait(notify.clone())
                    }
                    BackendState::Empty => {
                        let notify = Arc::new(tokio::sync::Notify::new());
                        *guard = BackendState::Spawning(notify.clone());
                        Next::Spawn(notify)
                    }
                }
            };

            match next {
                Next::Wait(notify) => {
                    notify.notified().await;
                }
                Next::Spawn(notify) => {
                    info!("backend: spawning");
                    let (transport, stderr_buf) = match Self::spawn_child(&self.cmd, &self.cmd_args) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(err = %e, "backend: spawn failed");
                            let mut guard = self.backend.lock().await;
                            *guard = BackendState::Empty;
                            notify.notify_waiters();
                            return Err(ErrorData::internal_error(format!("spawn: {}", e), None));
                        }
                    };

                    let running_result = tokio::time::timeout(
                        self.init_timeout,
                        ().serve(transport),
                    ).await
                        .map_err(|_| {
                            warn!(timeout_secs = self.init_timeout.as_secs(), "backend: init timeout");
                            ErrorData::internal_error("backend init timeout", None)
                        });
                    let running: RunningService<RoleClient, ()> = match running_result {
                        Ok(Ok(v)) => v,
                        Ok(Err(e)) => {
                            warn!(err = %e, "backend: init failed");
                            let mut guard = self.backend.lock().await;
                            *guard = BackendState::Empty;
                            notify.notify_waiters();
                            return Err(ErrorData::internal_error(format!("backend init: {}", e), None));
                        }
                        Err(err_data) => {
                            let mut guard = self.backend.lock().await;
                            *guard = BackendState::Empty;
                            notify.notify_waiters();
                            return Err(err_data);
                        }
                    };
                    info!("backend: ready");

                    let peer = running.peer().clone();
                    let buf = stderr_buf.clone();
                    let mut guard = self.backend.lock().await;
                    *guard = BackendState::Ready(running, stderr_buf);
                    notify.notify_waiters();
                    return Ok((peer, buf));
                }
            }
        }
    }

    /// Reset the idle timer. Call after each tool call completes.
    fn touch_idle(&self) {
        self.last_activity.store(now_millis(), Ordering::Relaxed);
    }
}

impl ServerHandler for McpProxy {
    fn get_info(&self) -> ServerInfo {
        self.server_info.clone()
    }

    fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        // Cursor ignored: all items are preloaded at init time.
        let _ = request;
        std::future::ready(Ok(self.cached_tools.clone()))
    }

    fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, ErrorData>> + Send + '_ {
        // Cursor ignored: all items are preloaded at init time.
        let _ = request;
        std::future::ready(Ok(self.cached_prompts.clone()))
    }

    fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + Send + '_ {
        // Cursor ignored: all items are preloaded at init time.
        let _ = request;
        std::future::ready(Ok(self.cached_resources.clone()))
    }

    fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> + Send + '_ {
        // Cursor ignored: all items are preloaded at init time.
        let _ = request;
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
            let arg_keys = request.arguments.as_ref()
                .map(|v| v.len())
                .unwrap_or(0);
            let t0 = std::time::Instant::now();
            info!(name = %name, arg_keys = arg_keys, "call_tool");
            if tracing::enabled!(tracing::Level::DEBUG) {
                let args_str = format!("{:?}", request.arguments);
                debug!(name = %name, args = %args_str, "call_tool args");
            }

            let _active_call_guard = ActiveCallGuard::new(self.active_calls.clone());
            let (peer, stderr_buf) = self.ensure_backend().await?;

            // No wrapper timeout — client decides how long to wait.
            // If backend dies, peer.call_tool() returns Err (pipe broken).
            let result = match peer.call_tool(request).await {
                Ok(v) => Ok(v),
                Err(ServiceError::McpError(err_data)) => Err(err_data),
                Err(other) => {
                    let stderr = {
                        let g = stderr_buf.lock().await;
                        if g.is_empty() { None } else { Some(g.clone()) }
                    }
                    .map(|s| format!("\nstderr: {}", s.trim()))
                    .unwrap_or_default();
                    Err(ErrorData::internal_error(format!("call_tool: {}{}", other, stderr), None))
                }
            };

            let elapsed = t0.elapsed().as_millis();
            match &result {
                Ok(r) => {
                    let result_size = r.content.len();
                    info!(name = %name, elapsed_ms = elapsed, arg_keys = arg_keys, result_size = result_size, "call_tool done");
                    if tracing::enabled!(tracing::Level::DEBUG) {
                        let result_str = format!("{:?}", r);
                        debug!(name = %name, result = %result_str, "call_tool result");
                    }
                }
                Err(ref e) => {
                    warn!(name = %name, elapsed_ms = elapsed, err = %e.message, "call_tool failed");
                }
            }
            self.touch_idle();
            result
        }
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResult, ErrorData>> + Send + '_ {
        async move {
            if self.backend_capabilities.resources.is_none() {
                return Err(ErrorData::method_not_found::<ReadResourceRequestMethod>());
            }
            let uri = request.uri.clone();
            info!(uri = %uri, "read_resource");
            let t0 = std::time::Instant::now();
            let _active_call_guard = ActiveCallGuard::new(self.active_calls.clone());
            let (peer, _stderr_buf) = self.ensure_backend().await?;
            let result = peer.read_resource(request).await.map_err(|e| match e {
                ServiceError::McpError(err_data) => err_data,
                other => ErrorData::internal_error(format!("read_resource: {}", other), None),
            });
            let elapsed = t0.elapsed().as_millis();
            match &result {
                Ok(_) => info!(uri = %uri, elapsed_ms = elapsed, "read_resource done"),
                Err(e) => warn!(uri = %uri, elapsed_ms = elapsed, err = %e.message, "read_resource failed"),
            }
            self.touch_idle();
            result
        }
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetPromptResult, ErrorData>> + Send + '_ {
        async move {
            if self.backend_capabilities.prompts.is_none() {
                return Err(ErrorData::method_not_found::<GetPromptRequestMethod>());
            }
            let name = request.name.clone();
            info!(name = %name, "get_prompt");
            let t0 = std::time::Instant::now();
            let _active_call_guard = ActiveCallGuard::new(self.active_calls.clone());
            let (peer, _stderr_buf) = self.ensure_backend().await?;
            let result = peer.get_prompt(request).await.map_err(|e| match e {
                ServiceError::McpError(err_data) => err_data,
                other => ErrorData::internal_error(format!("get_prompt: {}", other), None),
            });
            let elapsed = t0.elapsed().as_millis();
            match &result {
                Ok(_) => info!(name = %name, elapsed_ms = elapsed, "get_prompt done"),
                Err(e) => warn!(name = %name, elapsed_ms = elapsed, err = %e.message, "get_prompt failed"),
            }
            self.touch_idle();
            result
        }
    }

    fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CompleteResult, ErrorData>> + Send + '_ {
        async move {
            if self.backend_capabilities.completions.is_none() {
                return Err(ErrorData::method_not_found::<CompleteRequestMethod>());
            }
            let arg_name = request.argument.name.clone();
            info!(argument = %arg_name, "complete");
            let t0 = std::time::Instant::now();
            let _active_call_guard = ActiveCallGuard::new(self.active_calls.clone());
            let (peer, _stderr_buf) = self.ensure_backend().await?;
            let result = peer.complete(request).await.map_err(|e| match e {
                ServiceError::McpError(err_data) => err_data,
                other => ErrorData::internal_error(format!("complete: {}", other), None),
            });
            let elapsed = t0.elapsed().as_millis();
            match &result {
                Ok(_) => info!(argument = %arg_name, elapsed_ms = elapsed, "complete done"),
                Err(e) => warn!(argument = %arg_name, elapsed_ms = elapsed, err = %e.message, "complete failed"),
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
    let last_activity_handle = proxy.last_activity.clone();
    let active_calls_handle = proxy.active_calls.clone();

    let server = match proxy.serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            warn!(err = %e, "serve error");
            eprintln!("Error: Failed to start server: {}", e);
            std::process::exit(1);
        }
    };
    // Idle reaper: kill backend after BACKEND_IDLE_SECS of inactivity
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(BACKEND_IDLE_SECS)).await;
            let now = now_millis();
            let last = last_activity_handle.load(Ordering::Relaxed);
            let idle_for = now.saturating_sub(last);
            if idle_for < BACKEND_IDLE_SECS * 1000 {
                continue;
            }
            if active_calls_handle.load(Ordering::Relaxed) > 0 {
                continue;
            }
            let mut guard = backend_handle.lock().await;
            if let BackendState::Ready(_, _) = &*guard {
                info!("backend: idle timeout, shutting down");
                *guard = BackendState::Empty; // drop kills the subprocess
            }
        }
    });

    match server.waiting().await {
        Ok(_) => {}
        Err(e) => warn!(err = %e, "server waiting error"),
    }
    info!("shutdown");
}
