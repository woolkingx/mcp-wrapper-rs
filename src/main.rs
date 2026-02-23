//! MCP Wrapper - Universal lightweight proxy using rmcp SDK
//!
//! Usage: mcp-wrapper-rs <command> [args...]
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
use std::sync::OnceLock;
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

// === McpProxy: implements ServerHandler, proxies to real MCP server ===

struct McpProxy {
    cmd: String,
    cmd_args: Vec<String>,
    cached_tools: ListToolsResult,
    cached_prompts: ListPromptsResult,
    cached_resources: ListResourcesResult,
    cached_resource_templates: ListResourceTemplatesResult,
    server_info: ServerInfo,
    backend: tokio::sync::Mutex<Option<RunningService<RoleClient, ()>>>,
}

impl McpProxy {
    fn spawn_child(cmd: &str, cmd_args: &[String]) -> std::io::Result<TokioChildProcess> {
        let mut command = Command::new(cmd);
        command.args(cmd_args);
        let (proc, _stderr) = TokioChildProcess::builder(command)
            .stderr(Stdio::null())
            .spawn()?;
        Ok(proc)
    }

    async fn new(cmd: String, cmd_args: Vec<String>) -> Result<Self, Box<dyn std::error::Error>> {
        log("init_cache: spawning subprocess via rmcp client");
        let transport = Self::spawn_child(&cmd, &cmd_args)?;

        let client: RunningService<RoleClient, ()> = ().serve(transport).await?;
        let peer = client.peer().clone();

        // Query and cache all lists (with pagination support via list_all_*)
        let tools = peer.list_all_tools().await.unwrap_or_default();
        log(&format!("cached {} tools", tools.len()));
        let cached_tools = ListToolsResult::with_all_items(tools);

        let prompts = peer.list_all_prompts().await.unwrap_or_default();
        log(&format!("cached {} prompts", prompts.len()));
        let cached_prompts = ListPromptsResult::with_all_items(prompts);

        let resources = peer.list_all_resources().await.unwrap_or_default();
        log(&format!("cached {} resources", resources.len()));
        let cached_resources = ListResourcesResult::with_all_items(resources);

        let resource_templates = peer.list_all_resource_templates().await.unwrap_or_default();
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

    async fn ensure_backend(&self) -> Result<Peer<RoleClient>, ErrorData> {
        let mut guard = self.backend.lock().await;
        if let Some(ref running) = *guard {
            if !running.is_closed() {
                return Ok(running.peer().clone());
            }
            log("backend connection died, re-spawning");
        }

        log("spawning persistent backend subprocess");
        let transport = Self::spawn_child(&self.cmd, &self.cmd_args)
            .map_err(|e| ErrorData::internal_error(format!("spawn backend: {}", e), None))?;

        let running: RunningService<RoleClient, ()> = ().serve(transport).await
            .map_err(|e| ErrorData::internal_error(format!("backend init: {}", e), None))?;

        let peer = running.peer().clone();
        *guard = Some(running);
        Ok(peer)
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
            let peer = self.ensure_backend().await?;
            peer.call_tool(request).await.map_err(|e| {
                ErrorData::internal_error(format!("backend call_tool: {}", e), None)
            })
        }
    }
}

// === CLI and main ===

fn print_usage(program: &str) {
    eprintln!("mcp-wrapper-rs - Universal lightweight MCP proxy");
    eprintln!();
    eprintln!("Usage: {} <command> [args...]", program);
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --version, -V    Show version and exit");
    eprintln!("  --help, -h       Show this help and exit");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  {} python3 server.py", program);
    eprintln!("  {} npx -y @anthropics/mcp-searxng", program);
    eprintln!("  {} /path/to/run_server.sh", program);
    eprintln!();
    eprintln!("Environment Variables:");
    eprintln!("  MCP_WRAPPER_DEBUG=1    Enable debug logging to /tmp/mcp-wrapper-*.log");
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
    let cmd = args[1].clone();
    let cmd_args: Vec<String> = args[2..].to_vec();

    // Initialize log file
    let mcp_name = infer_mcp_name(&cmd, &cmd_args);
    let log_path = format!("/tmp/mcp-wrapper-{}-{}.log", mcp_name, std::process::id());
    let _ = LOG_FILE.set(log_path.clone());

    if is_debug() {
        log(&format!("=== MCP Wrapper v{} started (rmcp) ===", env!("CARGO_PKG_VERSION")));
        log(&format!("cmd: {} args: {:?}", cmd, cmd_args));
    }

    // Build proxy with cached server info
    let proxy = match McpProxy::new(cmd, cmd_args).await {
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
