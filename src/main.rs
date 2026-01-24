//! MCP Wrapper - 通用輕量代理
//!
//! Usage: mcp-wrapper-rs <command> [args...]
//!
//! 設計：
//! - 首次啟動時呼叫 subprocess 取得 tools/list 緩存
//! - init/tools/list 由 wrapper 瞬間回應
//! - 只有 tools/call 才啟動 subprocess 執行

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SUBPROCESS_TIMEOUT_SECS: u64 = 60;
const LOG_FILE: &str = "/tmp/mcp-wrapper-rs.log";

fn log(msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_FILE) {
        let _ = writeln!(f, "{}", msg);
    }
}

// === MCP 協議結構 ===

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

// === 緩存的 MCP 資訊 ===

struct McpCache {
    server_info: Value,
    capabilities: Value,
    tools: Value,
    prompts: Value,
    resources: Value,
}

// === 呼叫 subprocess（等 N 個回應或超時） ===

fn run_subprocess(cmd: &str, args: &[String], requests: &str, expected_responses: usize) -> Vec<Value> {
    log(&format!("run_subprocess: cmd={} args={:?} expected={}", cmd, args, expected_responses));

    let mut child = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log(&format!("spawn error: {}", e));
            return vec![];
        }
    };

    log("subprocess spawned");

    // 寫入請求（不關閉 stdin）
    if let Some(ref mut stdin) = child.stdin {
        let _ = stdin.write_all(requests.as_bytes());
        let _ = stdin.flush();
        log("requests written");
    }

    // 讀取回應（等 N 個有 id 的回應，或超時）
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
                    // 只收集有 id 的回應（跳過 notifications）
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
    // 關閉 subprocess
    let _ = child.kill();
    let _ = child.wait();

    responses
}

// === 初始化：取得 server 資訊 ===

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

    // 期望 4 個回應：init(0), tools(1), prompts(2), resources(3)
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

// === 執行 tool ===

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

    // 期望 2 個回應：init(0), tool_call(1)
    let responses = run_subprocess(cmd, args, &requests, 2);

    // 找 id=1 的回應
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

// === 主程式 ===

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: {} <command> [args...]", args[0]);
        eprintln!();
        eprintln!("Example:");
        eprintln!("  {} python3 server.py", args[0]);
        eprintln!("  {} npx -y @anthropics/mcp", args[0]);
        std::process::exit(1);
    }

    let cmd = &args[1];
    let cmd_args: Vec<String> = args[2..].to_vec();

    // 初始化：取得 server 資訊
    let cache = init_cache(cmd, &cmd_args);

    // 主循環
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

        // 通知不需回應
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
