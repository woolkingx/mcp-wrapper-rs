//! MCP Wrapper - Universal lightweight proxy
//!
//! Usage: mcp-wrapper-rs <command> [args...]
//!
//! Design:
//! - On first startup, spawn subprocess to cache tools/list
//! - init/tools/list responses are instant from cache
//! - Only tools/call spawns subprocess for execution
//! - Async event loop keeps process alive (like Node/Python MCP servers)

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;
use std::process::Stdio;



const SUBPROCESS_TIMEOUT_SECS: u64 = 60;

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

// === MCP Protocol Structures ===

#[derive(Deserialize)]
struct Request {
    method: Option<String>,
    id: Option<Value>,
    params: Option<Value>,
}

#[derive(Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

impl Response {
    fn success(id: Value, result: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }

    fn error(id: Value, code: i32, message: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(json!({"code": code, "message": message})),
        }
    }
}

// === Cached MCP Information ===
// Stores raw server responses keyed by method. On replay, only the "id" is replaced.

struct McpCache {
    initialize: Value,
    tools_list: Value,
    prompts_list: Value,
    resources_list: Value,
}

// === Run subprocess (wait for N responses or timeout) ===

async fn run_subprocess(
    cmd: &str,
    args: &[String],
    requests: &str,
    expected_responses: usize,
) -> Vec<Value> {
    log(&format!("run_subprocess: cmd={} args={:?} expected={}", cmd, args, expected_responses));

    #[cfg(unix)]
    let mut command = {
        let mut cmd = Command::new(cmd);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        cmd
    };

    #[cfg(not(unix))]
    let mut command = {
        let mut cmd = Command::new(cmd);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        cmd
    };

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            log(&format!("spawn error: {}", e));
            return vec![];
        }
    };

    log("subprocess spawned");

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(requests.as_bytes()).await;
        let _ = stdin.flush().await;
        drop(stdin);
        log("requests written and stdin closed");
    }

    let mut responses = Vec::new();
    let timeout_duration = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);

    if let Some(stdout) = child.stdout.take() {
        log("reading stdout...");
        let mut reader = tokio::io::BufReader::new(stdout).lines();
        let read_result = timeout(timeout_duration, async {
            while let Ok(Some(line)) = reader.next_line().await {
                log(&format!("got line: {}", &line[..line.len().min(100)]));
                if let Ok(resp) = serde_json::from_str::<Value>(&line) {
                    if resp.get("id").is_some() {
                        log(&format!("got response with id, total={}", responses.len() + 1));
                        responses.push(resp);
                        if responses.len() >= expected_responses {
                            log("got all expected responses");
                            break;
                        }
                    }
                }
            }
        })
        .await;

        if read_result.is_err() {
            log("timeout!");
        }
    }

    log(&format!("killing subprocess, got {} responses", responses.len()));

    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
            log("sent SIGTERM to process group");
        }
    }

    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }

    let _ = child.wait().await;

    responses
}

// === Initialize: fetch server info ===

async fn init_cache(cmd: &str, args: &[String]) -> McpCache {
    let init_req = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "mcp-wrapper-rs", "version": "1.0"}
        }
    });
    let init_notif = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let tools_req = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let prompts_req = json!({"jsonrpc": "2.0", "id": 2, "method": "prompts/list"});
    let resources_req = json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"});

    let requests = format!(
        "{}\n{}\n{}\n{}\n{}\n",
        init_req, init_notif, tools_req, prompts_req, resources_req
    );

    // Defaults: used only when server returns no response at all
    let mut cache = McpCache {
        initialize: json!({"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"unknown","version":"0.0.0"}}}),
        tools_list: json!({"jsonrpc":"2.0","id":1,"result":{"tools":[]}}),
        prompts_list: json!({"jsonrpc":"2.0","id":2,"result":{"prompts":[]}}),
        resources_list: json!({"jsonrpc":"2.0","id":3,"result":{"resources":[]}}),
    };

    let responses = run_subprocess(cmd, args, &requests, 4).await;

    // Store raw responses (success or error) keyed by the id we sent
    for resp in responses {
        match resp.get("id").and_then(|v| v.as_i64()) {
            Some(0) => cache.initialize = resp,
            Some(1) => cache.tools_list = resp,
            Some(2) => cache.prompts_list = resp,
            Some(3) => cache.resources_list = resp,
            _ => {}
        }
    }

    cache
}

// === Execute tool ===

async fn call_tool(cmd: &str, args: &[String], name: &str, arguments: &Value) -> Value {
    let init_req = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "mcp-wrapper-rs", "version": "1.0"}
        }
    });
    let init_notif = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let tool_req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    });

    let requests = format!("{}\n{}\n{}\n", init_req, init_notif, tool_req);
    let responses = run_subprocess(cmd, args, &requests, 2).await;

    for resp in responses {
        if resp.get("id").and_then(|v| v.as_i64()) == Some(1) {
            if let Some(result) = resp.get("result") {
                return result.clone();
            }
            if let Some(error) = resp.get("error") {
                return json!({"content": [{"type": "text", "text": error.to_string()}]});
            }
        }
    }

    json!({"content": [{"type": "text", "text": "subprocess error"}]})
}

// === Main ===

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
    if args.len() >= 2 && args[1].starts_with("-") {
        match args[1].as_str() {
            "--version" | "-V" => {
                println!("mcp-wrapper-rs 0.1.2");
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

    // Single-thread runtime: ~1-2MB RSS, no thread pool overhead
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
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

    // Register signal handlers FIRST — before any blocking work.
    // ctrl_c() is cross-platform (SIGINT on Unix, Ctrl+C on Windows).
    // SIGTERM/SIGHUP are Unix-only; on other platforms, recv_sigterm/recv_sighup
    // resolve to pending() (never fires).
    #[cfg(unix)]
    let (mut sigterm, mut sighup) = {
        use tokio::signal::unix::SignalKind;
        (
            tokio::signal::unix::signal(SignalKind::terminate())
                .expect("failed to register SIGTERM handler"),
            tokio::signal::unix::signal(SignalKind::hangup())
                .expect("failed to register SIGHUP handler"),
        )
    };

    if is_debug() {
        log(&format!("=== MCP Wrapper started (async): {} ===", mcp_name));
        log(&format!("cmd: {} args: {:?}", cmd, cmd_args));
    }

    // Helper closures for cross-platform signal handling.
    // On Unix: wait for real SIGTERM/SIGHUP. On Windows: pending (never fires).
    #[cfg(unix)]
    macro_rules! recv_sigterm { () => { sigterm.recv() } }
    #[cfg(not(unix))]
    macro_rules! recv_sigterm { () => { std::future::pending::<Option<()>>() } }
    #[cfg(unix)]
    macro_rules! recv_sighup { () => { sighup.recv() } }
    #[cfg(not(unix))]
    macro_rules! recv_sighup { () => { std::future::pending::<Option<()>>() } }

    let cache = tokio::select! {
        c = init_cache(&cmd, &cmd_args) => c,
        _ = tokio::signal::ctrl_c() => {
            log("[EVENT] SIGINT during init");
            return;
        }
        _ = recv_sigterm!() => {
            log("[EVENT] SIGTERM during init");
            return;
        }
        _ = recv_sighup!() => {
            log("[EVENT] SIGHUP during init");
            return;
        }
    };

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = tokio::io::BufReader::new(stdin).lines();

    // Unified event loop: every IO event is logged
    loop {
        tokio::select! {
            line = reader.next_line() => {
                match line {
                    Ok(Some(ref line)) if line.is_empty() => continue,
                    Ok(Some(line)) => {
                        log(&format!("[IN] {}", &line[..line.len().min(300)]));
                        if let Some(out) = handle_request(&line, &cache, &cmd, &cmd_args).await {
                            log(&format!("[OUT] {}", &out[..out.len().min(300)]));
                            let _ = stdout.write_all(out.as_bytes()).await;
                            let _ = stdout.write_all(b"\n").await;
                            let _ = stdout.flush().await;
                        }
                    }
                    Ok(None) => {
                        log("[EVENT] stdin EOF");
                        return;
                    }
                    Err(e) => {
                        log(&format!("[EVENT] stdin error: {}", e));
                        return;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                log("[EVENT] SIGINT");
                return;
            }
            _ = recv_sigterm!() => {
                log("[EVENT] SIGTERM");
                return;
            }
            _ = recv_sighup!() => {
                log("[EVENT] SIGHUP");
                return;
            }
        }
    }
}

/// Handle a single JSON-RPC request. Returns serialized response or None (for notifications).
/// Replay a cached raw response, replacing its "id" with the client's request id.
/// For "initialize", also patch protocolVersion to match the client's requested version.
fn replay_cached(cached: &Value, client_id: &Value, protocol_version: Option<&str>) -> Option<String> {
    let mut resp = cached.clone();
    resp["id"] = client_id.clone();
    if let Some(pv) = protocol_version {
        if let Some(result) = resp.get_mut("result") {
            result["protocolVersion"] = json!(pv);
        }
    }
    serde_json::to_string(&resp).ok()
}

async fn handle_request(
    line: &str,
    cache: &McpCache,
    cmd: &str,
    cmd_args: &[String],
) -> Option<String> {
    let req: Request = serde_json::from_str(line).ok()?;
    let id = req.id?; // notifications have no id → return None
    let method = req.method.unwrap_or_default();

    match method.as_str() {
        "initialize" => {
            let pv = req.params.as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .unwrap_or("2025-11-25");
            replay_cached(&cache.initialize, &id, Some(pv))
        }
        "tools/list" => replay_cached(&cache.tools_list, &id, None),
        "prompts/list" => replay_cached(&cache.prompts_list, &id, None),
        "resources/list" => replay_cached(&cache.resources_list, &id, None),
        "ping" => serde_json::to_string(&Response::success(id, json!({}))).ok(),
        "shutdown" => serde_json::to_string(&Response::success(id, json!({}))).ok(),
        "tools/call" => {
            let params = req.params.unwrap_or(json!({}));
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let empty = json!({});
            let arguments = params.get("arguments").unwrap_or(&empty);
            let result = call_tool(cmd, cmd_args, name, arguments).await;
            serde_json::to_string(&Response::success(id, result)).ok()
        }
        _ => serde_json::to_string(&Response::error(id, -32601, &format!("Method not found: {}", method))).ok(),
    }
}
