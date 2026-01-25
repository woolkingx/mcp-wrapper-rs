//! MCP Wrapper - Universal lightweight proxy
//!
//! Usage: mcp-wrapper-rs <command> [args...]
//!
//! Design:
//! - On first startup, spawn subprocess to cache tools/list
//! - init/tools/list responses are instant from cache
//! - Only tools/call spawns subprocess for execution

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const SUBPROCESS_TIMEOUT_SECS: u64 = 60;

static LOG_FILE: OnceLock<String> = OnceLock::new();
static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();

fn is_debug() -> bool {
    *DEBUG_ENABLED.get_or_init(|| {
        env::var("MCP_WRAPPER_DEBUG").is_ok()
    })
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
    // First try environment variable
    if let Ok(name) = env::var("MCP_SERVER_NAME") {
        return sanitize_name(&name);
    }

    // Infer from command line
    if cmd == "npx" && !args.is_empty() {
        // npx -y mcp-searxng → mcp-searxng
        // npx @oevortex/ddg_search → ddg_search
        for arg in args {
            if !arg.starts_with('-') {
                let name = arg.split('/').last().unwrap_or(arg);
                let name = name.split('@').next().unwrap_or(name);
                return sanitize_name(name);
            }
        }
    }

    // Infer from path: python3 /path/to/server.py → server
    if (cmd == "python3" || cmd == "python") && !args.is_empty() {
        if let Some(script) = args.first() {
            if let Some(name) = Path::new(script).file_stem() {
                return sanitize_name(name.to_string_lossy().as_ref());
            }
        }
    }

    // Shell script: /path/to/run_server.sh → run_server
    if cmd.ends_with(".sh") {
        if let Some(name) = Path::new(cmd).file_stem() {
            return sanitize_name(name.to_string_lossy().as_ref());
        }
    }

    // Fallback to command itself
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

struct McpCache {
    server_info: Value,
    capabilities: Value,
    tools: Value,
    prompts: Value,
    resources: Value,
}

// === Run subprocess (wait for N responses or timeout) ===

fn run_subprocess(cmd: &str, args: &[String], requests: &str, expected_responses: usize) -> Vec<Value> {
    log(&format!("run_subprocess: cmd={} args={:?} expected={}", cmd, args, expected_responses));

    #[cfg(unix)]
    let mut command = {
        let mut cmd = Command::new(cmd);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0); // Create new process group
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

    // Write requests and close stdin (MCP server needs EOF to process)
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(requests.as_bytes());
        let _ = stdin.flush();
        drop(stdin); // Close stdin, trigger EOF
        log("requests written and stdin closed");
    }

    // Read responses (wait for N responses with id, or timeout)
    let mut responses = Vec::new();
    let start = Instant::now();
    let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);

    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        log("reading stdout...");
        for line in reader.lines() {
            if start.elapsed() > timeout {
                log("timeout!");
                break;
            }
            if let Ok(line) = line {
                log(&format!("got line: {}", &line[..line.len().min(100)]));
                if let Ok(resp) = serde_json::from_str::<Value>(&line) {
                    // Only collect responses with id (skip notifications)
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
        }
    }

    log(&format!("killing subprocess, got {} responses", responses.len()));

    // Terminate subprocess and its entire process group
    #[cfg(unix)]
    {
        let pid = child.id();
        // Kill the entire process group (negative PID kills process group)
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        log("sent SIGTERM to process group");
    }

    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let _ = child.wait();

    responses
}

// === Initialize: fetch server info ===

fn init_cache(cmd: &str, args: &[String]) -> McpCache {
    let init_req = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
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

    let mut cache = McpCache {
        server_info: json!({"name": "mcp-wrapper", "version": "1.0.0"}),
        capabilities: json!({
            "experimental": {},
            "prompts": {"listChanged": false},
            "resources": {"subscribe": false, "listChanged": false},
            "tools": {"listChanged": false}
        }),
        tools: json!([]),
        prompts: json!([]),
        resources: json!([]),
    };

    // Expect 4 responses: init(0), tools(1), prompts(2), resources(3)
    let responses = run_subprocess(cmd, args, &requests, 4);

    for resp in responses {
        let id = resp.get("id").and_then(|v| v.as_i64());
        if let Some(result) = resp.get("result") {
            match id {
                Some(0) => {
                    if let Some(info) = result.get("serverInfo") {
                        cache.server_info = info.clone();
                    }
                    if let Some(caps) = result.get("capabilities") {
                        cache.capabilities = caps.clone();
                    }
                }
                Some(1) => {
                    if let Some(tools) = result.get("tools") {
                        cache.tools = tools.clone();
                    }
                }
                Some(2) => {
                    if let Some(prompts) = result.get("prompts") {
                        cache.prompts = prompts.clone();
                    }
                }
                Some(3) => {
                    if let Some(resources) = result.get("resources") {
                        cache.resources = resources.clone();
                    }
                }
                _ => {}
            }
        }
    }

    cache
}

// === Execute tool ===

fn call_tool(cmd: &str, args: &[String], name: &str, arguments: &Value) -> Value {
    let init_req = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
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

    // Expect 2 responses: init(0), tool_call(1)
    let responses = run_subprocess(cmd, args, &requests, 2);

    // Find response with id=1
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

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: {} <command> [args...]", args[0]);
        eprintln!();
        eprintln!("Example:");
        eprintln!("  {} python3 server.py", args[0]);
        eprintln!("  {} npx -y @anthropics/mcp", args[0]);
        eprintln!();
        eprintln!("Environment:");
        eprintln!("  MCP_WRAPPER_DEBUG=1    Enable debug logging");
        eprintln!("  MCP_SERVER_NAME=xxx    Override log file name");
        std::process::exit(1);
    }

    let cmd = &args[1];
    let cmd_args: Vec<String> = args[2..].to_vec();

    // Initialize log file name
    let mcp_name = infer_mcp_name(cmd, &cmd_args);
    let log_path = format!("/tmp/mcp-wrapper-{}.log", mcp_name);
    let _ = LOG_FILE.set(log_path.clone());

    if is_debug() {
        log(&format!("=== MCP Wrapper started: {} ===", mcp_name));
        log(&format!("cmd: {} args: {:?}", cmd, cmd_args));
        log(&format!("log file: {}", log_path));
    }

    // Initialize: fetch server info
    let cache = init_cache(cmd, &cmd_args);

    // Main loop
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        if line.is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Notifications don't need response
        let id = match req.id {
            Some(id) => id,
            None => continue,
        };

        let method = req.method.unwrap_or_default();

        let response = match method.as_str() {
            "initialize" => Response::success(
                id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": cache.capabilities,
                    "serverInfo": cache.server_info
                }),
            ),

            "tools/list" => Response::success(id, json!({"tools": cache.tools})),
            "prompts/list" => Response::success(id, json!({"prompts": cache.prompts})),
            "resources/list" => Response::success(id, json!({"resources": cache.resources})),

            "tools/call" => {
                let params = req.params.unwrap_or(json!({}));
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let empty = json!({});
                let arguments = params.get("arguments").unwrap_or(&empty);

                let result = call_tool(cmd, &cmd_args, name, arguments);
                Response::success(id, result)
            }

            _ => Response::error(id, -32601, &format!("Method not found: {}", method)),
        };

        if let Ok(json) = serde_json::to_string(&response) {
            let _ = writeln!(stdout, "{}", json);
            let _ = stdout.flush();
        }
    }
}
