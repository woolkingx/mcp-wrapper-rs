//! Integration tests: spawn the mcp-wrapper-rs binary and interact via
//! stdin/stdout JSON-RPC to verify end-to-end behavior.
//!
//! Uses the echo_server.py fixture as the backend MCP server.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use serde_json::{json, Value};

// ── Test helpers ─────────────────────────────────────────────────────────────

fn wrapper_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mcp-wrapper-rs"))
}

fn echo_server_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo_server.py")
}

struct Wrapper {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
}

impl Wrapper {
    fn spawn(extra_args: &[&str]) -> Self {
        let mut cmd = Command::new(wrapper_binary());
        cmd.args(extra_args);
        cmd.arg("python3").arg(echo_server_path());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("failed to spawn wrapper");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        Wrapper { child, stdin, reader }
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{}", serde_json::to_string(msg).unwrap()).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Read next JSON-RPC response (skips notifications that lack "id").
    fn recv(&mut self) -> Value {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(n > 0, "unexpected EOF from wrapper stdout");
            let v: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("bad JSON from wrapper: {e}\nline: {line}"));
            if v.get("id").is_some() {
                return v;
            }
            // notification — skip and keep reading
        }
    }

    fn handshake(&mut self) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.0.1"}
            }
        }));
        let resp = self.recv();
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        resp
    }

    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_flag(args: &[&str]) -> std::process::Output {
    Command::new(wrapper_binary())
        .args(args)
        .output()
        .expect("failed to run wrapper")
}

// ── CLI tests ────────────────────────────────────────────────────────────────

#[test]
fn cli_version() {
    let out = run_flag(&["--version"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("mcp-wrapper-rs"), "output: {stdout}");
    // Version from Cargo.toml
    let version = env!("CARGO_PKG_VERSION");
    assert!(stdout.contains(version), "expected version {version} in: {stdout}");
}

#[test]
fn cli_version_short() {
    let out = run_flag(&["-V"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("mcp-wrapper-rs"), "output: {stdout}");
}

#[test]
fn cli_help() {
    let out = run_flag(&["--help"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--init-timeout"), "help output: {stderr}");
    assert!(stderr.contains("Usage"), "help output: {stderr}");
}

#[test]
fn cli_help_short() {
    let out = run_flag(&["-h"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Usage"), "help output: {stderr}");
}

#[test]
fn cli_unknown_flag() {
    let out = run_flag(&["--unknown"]);
    assert!(!out.status.success(), "unknown flag should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Unknown option"), "stderr: {stderr}");
}

#[test]
fn cli_no_args() {
    let out = run_flag(&[]);
    assert!(!out.status.success(), "no args should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Missing"), "stderr: {stderr}");
}

#[test]
fn cli_init_timeout_missing_value() {
    let out = run_flag(&["--init-timeout"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--init-timeout"), "stderr: {stderr}");
}

// ── MCP flow tests ───────────────────────────────────────────────────────────

#[test]
fn basic_flow() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);

    // 1. Initialize
    let init = w.handshake();
    let info = &init["result"]["serverInfo"];
    assert_eq!(info["name"], "echo-server");

    // 2. tools/list from cache
    w.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    let tools_resp = w.recv();
    let tools = tools_resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");

    // 3. tools/call echo
    w.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "hello world"}}
    }));
    let call_resp = w.recv();
    assert_eq!(call_resp["result"]["content"][0]["type"], "text");
    assert_eq!(call_resp["result"]["content"][0]["text"], "hello world");

    w.kill();
}

#[test]
fn ping_returns_empty_result() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    let resp = w.recv();
    assert!(resp.get("result").is_some(), "ping should return result");
    assert!(resp.get("error").is_none(), "ping should not return error");

    w.kill();
}

#[test]
fn consecutive_calls_reuse_backend() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    // First call
    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "first"}}
    }));
    let r1 = w.recv();
    assert_eq!(r1["result"]["content"][0]["text"], "first");

    // Second call — should reuse the same backend (no extra spawn delay)
    w.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "second"}}
    }));
    let r2 = w.recv();
    assert_eq!(r2["result"]["content"][0]["text"], "second");

    w.kill();
}

#[test]
fn prompts_list_from_cache() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "prompts/list", "params": {}}));
    let resp = w.recv();
    // Echo server returns empty prompts list
    let prompts = resp["result"]["prompts"].as_array().expect("prompts array");
    assert!(prompts.is_empty());

    w.kill();
}

#[test]
fn resources_list_from_cache() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "resources/list", "params": {}}));
    let resp = w.recv();
    let resources = resp["result"]["resources"].as_array().expect("resources array");
    assert!(resources.is_empty());

    w.kill();
}

#[test]
fn call_tool_with_empty_args() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "echo", "arguments": {}}
    }));
    let resp = w.recv();
    // Empty msg argument → empty text
    assert_eq!(resp["result"]["content"][0]["text"], "");

    w.kill();
}
