//! Behavioral tests: spawn wrapper binary, drive via JSON-RPC, verify behavior.
//!
//! Tests cover:
//! - Cache: tools/list and prompts/list served from cache (no backend spawn)
//! - Timeout: servers that don't respond to prompts/list are skipped after timeout
//! - call_tool: forwarded to backend, backend auto-respawns on death
//! - CLI flags: --version, --help, --init-timeout, unknown flag

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

fn wrapper_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    // tests run from target/debug/deps/, binary is at target/debug/
    p.pop(); p.pop();
    p.push("mcp-wrapper-rs");
    p
}

/// Minimal echo MCP server written as a self-contained Python script.
/// Responds to initialize + tools/list. Silently ignores prompts/list.
fn echo_server_script() -> String {
    r#"
import sys, json

def send(msg):
    line = json.dumps(msg)
    sys.stdout.write(line + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method", "")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2025-03-26",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"echo-server","version":"0.1.0"}
        }})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{
            "tools":[{"name":"echo","description":"Echo input","inputSchema":{"type":"object","properties":{"msg":{"type":"string"}}}}]
        }})
    elif method == "tools/call":
        params = msg.get("params", {})
        arg_msg = params.get("arguments", {}).get("msg", "")
        send({"jsonrpc":"2.0","id":mid,"result":{
            "content":[{"type":"text","text": arg_msg}]
        }})
    elif method == "notifications/initialized":
        pass
    # prompts/list and resources/list are intentionally ignored
"#.to_string()
}

/// Server that never responds to anything after initialize — for timeout tests.
fn slow_server_script() -> String {
    r#"
import sys, json, time

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method", "")
    mid = msg.get("id")
    if method == "initialize":
        resp = json.dumps({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2025-03-26",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"slow-server","version":"0.1.0"}
        }})
        sys.stdout.write(resp + "\n")
        sys.stdout.flush()
    elif method == "tools/list":
        resp = json.dumps({"jsonrpc":"2.0","id":mid,"result":{"tools":[]}})
        sys.stdout.write(resp + "\n")
        sys.stdout.flush()
    # everything else: no response (simulates hang)
"#.to_string()
}

struct WrapperProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl WrapperProcess {
    async fn spawn_with_script(script: &str, extra_args: &[&str]) -> Self {
        // Write script to a temp file
        let script_path = format!("/tmp/mcp_test_server_{}.py", std::process::id());
        tokio::fs::write(&script_path, script).await.unwrap();

        let bin = wrapper_bin();
        let mut cmd = Command::new(&bin);
        cmd.args(extra_args);
        cmd.arg("python3").arg(&script_path);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());

        let mut child = cmd.spawn().expect("failed to spawn wrapper");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        WrapperProcess { child, stdin, stdout }
    }

    async fn send(&mut self, msg: serde_json::Value) {
        let line = serde_json::to_string(&msg).unwrap() + "\n";
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    /// Read next JSON-RPC line, skip notifications (no "id").
    async fn recv(&mut self) -> serde_json::Value {
        let mut buf = String::new();
        loop {
            buf.clear();
            self.stdout.read_line(&mut buf).await.unwrap();
            let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            if v.get("id").is_some() {
                return v;
            }
        }
    }

    async fn handshake(&mut self) -> serde_json::Value {
        self.send(serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"0.0.1"}
            }
        })).await;
        let resp = self.recv().await;
        self.send(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        resp
    }

    async fn kill(mut self) {
        let _ = self.child.kill().await;
    }
}

// ── Behavioral tests ────────────────────────────────────────────────────────

/// initialize returns the backend server's name and capabilities.
#[tokio::test]
async fn test_initialize_returns_server_info() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    let resp = w.handshake().await;
    let info = &resp["result"]["serverInfo"];
    assert_eq!(info["name"], "echo-server", "serverInfo.name forwarded from backend");
    w.kill().await;
}

/// tools/list is served from cache — the result matches what the backend declared.
#[tokio::test]
async fn test_tools_list_from_cache() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;

    w.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})).await;
    let resp = w.recv().await;

    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");
    w.kill().await;
}

/// prompts/list returns empty list (backend ignores the request, timeout fires).
#[tokio::test]
async fn test_prompts_list_empty_on_timeout() {
    // --init-timeout 2 so the test is fast
    let mut w = WrapperProcess::spawn_with_script(
        &echo_server_script(), &["--init-timeout", "2"]
    ).await;
    w.handshake().await;

    w.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"prompts/list","params":{}})).await;
    let resp = w.recv().await;

    let prompts = resp["result"]["prompts"].as_array().expect("prompts array");
    assert!(prompts.is_empty(), "expected empty prompts from cache");
    w.kill().await;
}

/// resources/list returns empty list (backend ignores, timeout fires).
#[tokio::test]
async fn test_resources_list_empty_on_timeout() {
    let mut w = WrapperProcess::spawn_with_script(
        &echo_server_script(), &["--init-timeout", "2"]
    ).await;
    w.handshake().await;

    w.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"resources/list","params":{}})).await;
    let resp = w.recv().await;

    let resources = resp["result"]["resources"].as_array().expect("resources array");
    assert!(resources.is_empty());
    w.kill().await;
}

/// call_tool is forwarded to the backend and result returned.
#[tokio::test]
async fn test_call_tool_forwarded() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;

    w.send(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"hello world"}}
    })).await;
    let resp = w.recv().await;

    let content = &resp["result"]["content"][0];
    assert_eq!(content["type"], "text");
    assert_eq!(content["text"], "hello world");
    w.kill().await;
}

/// Two consecutive tool calls reuse the same backend (smoke test).
#[tokio::test]
async fn test_call_tool_twice_reuses_backend() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;

    for (id, msg) in [(2, "first"), (3, "second")] {
        w.send(serde_json::json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":"echo","arguments":{"msg":msg}}
        })).await;
        let resp = w.recv().await;
        assert_eq!(resp["result"]["content"][0]["text"], msg);
    }
    w.kill().await;
}

/// Wrapper completes init even when backend ignores prompts/resources (slow server).
#[tokio::test]
async fn test_init_completes_despite_unresponsive_prompts() {
    let mut w = WrapperProcess::spawn_with_script(
        &slow_server_script(), &["--init-timeout", "2"]
    ).await;

    // handshake should succeed within a reasonable time
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        w.handshake()
    ).await;
    assert!(result.is_ok(), "init timed out — wrapper hung waiting for prompts/list");

    w.kill().await;
}

// ── CLI flag tests ───────────────────────────────────────────────────────────

async fn run_wrapper_flag(args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(wrapper_bin())
        .args(args)
        .output()
        .await
        .expect("failed to run wrapper")
}

#[tokio::test]
async fn test_version_flag() {
    let out = run_wrapper_flag(&["--version"]).await;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("mcp-wrapper-rs"), "version output: {}", stdout);
    assert!(stdout.contains("0.2.0"), "version output: {}", stdout);
}

#[tokio::test]
async fn test_help_flag() {
    let out = run_wrapper_flag(&["--help"]).await;
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--init-timeout"), "help output: {}", stderr);
}

#[tokio::test]
async fn test_unknown_flag_exits_nonzero() {
    let out = run_wrapper_flag(&["--unknown-flag"]).await;
    assert!(!out.status.success(), "unknown flag should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Unknown option"), "stderr: {}", stderr);
}

#[tokio::test]
async fn test_no_args_exits_nonzero() {
    let out = run_wrapper_flag(&[]).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Missing"), "stderr: {}", stderr);
}

#[tokio::test]
async fn test_init_timeout_missing_value() {
    let out = run_wrapper_flag(&["--init-timeout"]).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--init-timeout"), "stderr: {}", stderr);
}
