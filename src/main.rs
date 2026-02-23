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
    service::{RequestContext, RoleClient, RoleServer, RunningService, Peer},
    transport::{TokioChildProcess, io::stdio},
};
use std::env;
use std::future::Future;
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

static LOG_FILE: OnceLock<String> = OnceLock::new();
static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();

fn is_debug() -> bool {
    *DEBUG_ENABLED.get_or_init(|| env::var("MCP_WRAPPER_DEBUG").is_ok())
}

fn log(msg: &str) {
    if !is_debug() {
        return;
    }
    if let Some(log_file) = LOG_FILE.get() {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_file) {
            let _ = writeln!(f, "{}", msg);
        }
    }
}

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

/// Spawn a stderr reader task that tees output to the log file and a shared buffer.
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
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    // Tee to log file
                    if is_debug() {
                        for line in text.lines() {
                            log(&format!("[stderr] {}", line));
                        }
                    }
                    // Keep last ~4KB in buffer for error reporting
                    if let Ok(mut guard) = buf_clone.lock() {
                        guard.push_str(&text);
                        // Trim to avoid unbounded growth
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

struct McpProxy {
    cmd: String,
    cmd_args: Vec<String>,
    cached_tools: ListToolsResult,
    cached_prompts: ListPromptsResult,
    cached_resources: ListResourcesResult,
    cached_resource_templates: ListResourceTemplatesResult,
    server_info: ServerInfo,
    backend: tokio::sync::Mutex<Option<(RunningService<RoleClient, ()>, Arc<Mutex<String>>)>>,
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
        log("init_cache: spawning subprocess via rmcp client");
        let (transport, _stderr_buf) = Self::spawn_child(&cmd, &cmd_args)?;

        let client: RunningService<RoleClient, ()> = ().serve(transport).await?;
        let peer = client.peer().clone();

        // Query and cache all lists (with pagination support via list_all_*)
        // Use timeout for each call — some servers don't implement prompts/resources
        // and will silently ignore the request, causing a hang without a timeout.
        let tools = tokio::time::timeout(init_timeout, peer.list_all_tools())
            .await.ok().and_then(|r| r.ok()).unwrap_or_default();
        log(&format!("cached {} tools", tools.len()));
        let cached_tools = ListToolsResult::with_all_items(tools);

        let prompts = tokio::time::timeout(init_timeout, peer.list_all_prompts())
            .await.ok().and_then(|r| r.ok()).unwrap_or_default();
        log(&format!("cached {} prompts", prompts.len()));
        let cached_prompts = ListPromptsResult::with_all_items(prompts);

        let resources = tokio::time::timeout(init_timeout, peer.list_all_resources())
            .await.ok().and_then(|r| r.ok()).unwrap_or_default();
        log(&format!("cached {} resources", resources.len()));
        let cached_resources = ListResourcesResult::with_all_items(resources);

        let resource_templates = tokio::time::timeout(init_timeout, peer.list_all_resource_templates())
            .await.ok().and_then(|r| r.ok()).unwrap_or_default();
        log(&format!("cached {} resource_templates", resource_templates.len()));
        let cached_resource_templates = ListResourceTemplatesResult::with_all_items(resource_templates);

        // Cache server info from the initialize handshake
        let server_info = peer.peer_info()
            .cloned()
            .unwrap_or_default();
        log(&format!("server_info: {:?}", server_info.server_info));

        // Shutdown init subprocess
        drop(client);
        log("init subprocess shutdown");

        Ok(Self {
            cmd,
            cmd_args,
            cached_tools,
            cached_prompts,
            cached_resources,
            cached_resource_templates,
            server_info,
            backend: tokio::sync::Mutex::new(None),
        })
    }

    async fn ensure_backend(&self) -> Result<(Peer<RoleClient>, Arc<Mutex<String>>), ErrorData> {
        let mut guard = self.backend.lock().await;
        if let Some((ref running, ref stderr_buf)) = *guard {
            if !running.is_closed() {
                return Ok((running.peer().clone(), stderr_buf.clone()));
            }
            log("backend connection died, re-spawning");
        }

        log("spawning persistent backend subprocess");
        let (transport, stderr_buf) = Self::spawn_child(&self.cmd, &self.cmd_args)
            .map_err(|e| ErrorData::internal_error(format!("spawn backend: {}", e), None))?;

        let running: RunningService<RoleClient, ()> = ().serve(transport).await
            .map_err(|e| ErrorData::internal_error(format!("backend init: {}", e), None))?;

        let peer = running.peer().clone();
        let buf = stderr_buf.clone();
        *guard = Some((running, stderr_buf));
        Ok((peer, buf))
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
        log("[handler] list_tools → from cache");
        std::future::ready(Ok(self.cached_tools.clone()))
    }

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, ErrorData>> + Send + '_ {
        log("[handler] list_prompts → from cache");
        std::future::ready(Ok(self.cached_prompts.clone()))
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + Send + '_ {
        log("[handler] list_resources → from cache");
        std::future::ready(Ok(self.cached_resources.clone()))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> + Send + '_ {
        log("[handler] list_resource_templates → from cache");
        std::future::ready(Ok(self.cached_resource_templates.clone()))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        async move {
            log(&format!("[handler] call_tool: {}", request.name));
            let (peer, stderr_buf) = self.ensure_backend().await?;
            peer.call_tool(request).await.map_err(|e| {
                // Attach stderr output to the error message for easier diagnosis
                let stderr = stderr_buf.lock()
                    .ok()
                    .and_then(|g| if g.is_empty() { None } else { Some(g.clone()) })
                    .map(|s| format!("\nstderr: {}", s.trim()))
                    .unwrap_or_default();
                ErrorData::internal_error(
                    format!("backend call_tool: {}{}", e, stderr),
                    None,
                )
            })
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
    eprintln!("  --init-timeout <secs>      Seconds to wait for prompts/resources during init (default: 5)");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  {} python3 server.py", program);
    eprintln!("  {} npx -y @anthropics/mcp-searxng", program);
    eprintln!("  {} --init-timeout 10 codex mcp-server", program);
    eprintln!();
    eprintln!("Environment Variables:");
    eprintln!("  MCP_WRAPPER_DEBUG=1    Enable debug logging to $XDG_RUNTIME_DIR/mcp-wrapper/ (or /tmp)");
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
        (Duration::from_secs(secs), 3)
    } else {
        (Duration::from_secs(5), 1)
    };

    if args.len() <= cmd_start {
        eprintln!("Error: Missing <command> argument");
        eprintln!();
        print_usage(&args[0]);
        std::process::exit(1);
    }

    let cmd = args[cmd_start].clone();
    let cmd_args: Vec<String> = args[cmd_start + 1..].to_vec();

    // Initialize log file
    let mcp_name = infer_mcp_name(&cmd, &cmd_args);
    let log_path = format!("{}/mcp-wrapper-{}-{}.log", log_dir(), mcp_name, std::process::id());
    let _ = LOG_FILE.set(log_path.clone());

    if is_debug() {
        log(&format!("=== MCP Wrapper v{} started (rmcp) ===", env!("CARGO_PKG_VERSION")));
        log(&format!("cmd: {} args: {:?}", cmd, cmd_args));
        log(&format!("init_timeout: {}s", init_timeout.as_secs()));
    }

    // Build proxy with cached server info
    let proxy = match McpProxy::new(cmd, cmd_args, init_timeout).await {
        Ok(p) => p,
        Err(e) => {
            log(&format!("init failed: {}", e));
            eprintln!("Error: Failed to initialize MCP server: {}", e);
            std::process::exit(1);
        }
    };

    log("serving via rmcp on stdio");

    // Serve on stdin/stdout — rmcp handles the full JSON-RPC protocol
    match proxy.serve(stdio()).await {
        Ok(server) => {
            let _ = server.waiting().await;
        }
        Err(e) => {
            log(&format!("serve error: {}", e));
            eprintln!("Error: Failed to start server: {}", e);
            std::process::exit(1);
        }
    }

    log("shutdown complete");
}
