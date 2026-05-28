//! Integration tests: spawn the mcp-wrapper-rs binary and interact via
//! stdin/stdout JSON-RPC to verify end-to-end behavior.
//!
//! Uses the echo_server.py fixture as the backend MCP server.

use serde_json::{Value, json};
use std::io::ErrorKind;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
        Self::spawn_with_backend_args(extra_args, &[])
    }

    fn spawn_with_backend_args(extra_args: &[&str], backend_args: &[&str]) -> Self {
        let mut cmd = Command::new(wrapper_binary());
        cmd.args(extra_args);
        cmd.arg("python3").arg(echo_server_path());
        cmd.args(backend_args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("failed to spawn wrapper");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        Wrapper {
            child,
            stdin,
            reader,
        }
    }

    fn recv_any(&mut self) -> Value {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).unwrap();
        assert!(n > 0, "unexpected EOF from wrapper stdout");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("bad JSON from wrapper: {e}\nline: {line}"))
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

    fn recv_timeout(&mut self, timeout: Duration) -> Option<Value> {
        let fd = self.reader.get_ref().as_raw_fd();
        let old_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(old_flags >= 0, "fcntl F_GETFL failed");
        let set_nonblock = unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags | libc::O_NONBLOCK) };
        assert!(set_nonblock >= 0, "fcntl F_SETFL O_NONBLOCK failed");

        let deadline = Instant::now() + timeout;
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("unexpected EOF from wrapper stdout"),
                Ok(_) => {
                    let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags) };
                    return Some(
                        serde_json::from_str(line.trim())
                            .unwrap_or_else(|e| panic!("bad JSON from wrapper: {e}\nline: {line}")),
                    );
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags) };
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("read wrapper stdout: {e}"),
            }
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

fn run_cli(args: &[String]) -> std::process::Output {
    Command::new(wrapper_binary())
        .args(args)
        .output()
        .expect("failed to run wrapper")
}

fn admin_target(suffix: &str) -> Vec<String> {
    vec![
        "python3".to_string(),
        echo_server_path().to_string_lossy().to_string(),
        format!("--cli-admin-test-{}-{}", std::process::id(), suffix),
    ]
}

fn admin_args(prefix: &[&str], target: &[String]) -> Vec<String> {
    let mut args: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
    args.push("--".to_string());
    args.extend(target.iter().cloned());
    args
}

fn run_admin_json(prefix: &[&str], target: &[String]) -> Value {
    let out = run_cli(&admin_args(prefix, target));
    assert!(
        out.status.success(),
        "admin command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "bad admin JSON: {e}\nstdout: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn stop_admin_broker(target: &[String]) {
    let _ = run_cli(&admin_args(&["broker", "stop", "--json"], target));
    std::thread::sleep(Duration::from_millis(150));
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
    assert!(
        stdout.contains(version),
        "expected version {version} in: {stdout}"
    );
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

#[test]
fn cli_admin_status_without_spawn() {
    let target = admin_target("status-without-spawn");
    stop_admin_broker(&target);

    let status = run_admin_json(&["status", "--json"], &target);
    assert_eq!(status["ok"], true);
    assert_eq!(status["action"], "status");
    assert_eq!(status["broker"]["running"], false);
    assert_eq!(status["result"], Value::Null);
}

#[test]
fn cli_admin_backend_restart_start_json() {
    let target = admin_target("restart-start");
    stop_admin_broker(&target);

    let restart = run_admin_json(
        &[
            "backend",
            "restart",
            "--start",
            "--json",
            "--init-timeout",
            "10",
        ],
        &target,
    );
    assert_eq!(restart["ok"], true);
    assert_eq!(restart["action"], "backend.restart");
    assert_eq!(restart["broker"]["running"], true);
    assert!(restart["result"]["newCacheEpoch"].as_u64().unwrap() >= 1);

    stop_admin_broker(&target);
}

#[test]
fn cli_admin_backend_stop_and_ping_json() {
    let target = admin_target("stop-ping");
    stop_admin_broker(&target);

    let ping_ready = run_admin_json(
        &[
            "backend",
            "ping",
            "--start",
            "--json",
            "--init-timeout",
            "10",
        ],
        &target,
    );
    assert_eq!(ping_ready["ok"], true);
    assert_eq!(ping_ready["result"]["ok"], true);

    let stopped = run_admin_json(&["backend", "stop", "--json"], &target);
    assert_eq!(stopped["ok"], true);
    assert_eq!(stopped["result"]["state"], "stopped");

    let ping_stopped = run_admin_json(&["backend", "ping", "--json"], &target);
    assert_eq!(ping_stopped["ok"], true);
    assert_eq!(ping_stopped["result"]["ok"], false);
    assert_eq!(ping_stopped["result"]["backend"]["state"], "stopped");

    stop_admin_broker(&target);
}

#[test]
fn cli_admin_broker_list_status_stop_json() {
    let target = admin_target("broker-list-status-stop");
    stop_admin_broker(&target);

    let restarted = run_admin_json(
        &["broker", "restart", "--json", "--init-timeout", "10"],
        &target,
    );
    assert_eq!(restarted["ok"], true);
    assert_eq!(restarted["broker"]["running"], true);
    let hash = restarted["broker"]["hash"].as_str().unwrap().to_string();

    let list_out = run_cli(&[
        "broker".to_string(),
        "list".to_string(),
        "--json".to_string(),
    ]);
    assert!(
        list_out.status.success(),
        "broker list failed: {}",
        String::from_utf8_lossy(&list_out.stderr)
    );
    let list: Value = serde_json::from_slice(&list_out.stdout).unwrap();
    let brokers = list["result"]["brokers"].as_array().unwrap();
    assert!(brokers.iter().any(|b| b["hash"] == hash));

    let status = run_admin_json(&["broker", "status", "--json"], &target);
    assert_eq!(status["broker"]["running"], true);

    let stopped = run_admin_json(&["broker", "stop", "--json"], &target);
    assert_eq!(stopped["ok"], true);
    assert_eq!(stopped["broker"]["running"], false);

    let status_after = run_admin_json(&["broker", "status", "--json"], &target);
    assert_eq!(status_after["broker"]["running"], false);
}

#[test]
fn cli_admin_unknown_command_is_error() {
    let target = admin_target("unknown-command");
    let out = run_cli(&admin_args(&["backend", "bounce", "--json"], &target));
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Unknown backend command"),
        "stderr: {stderr}"
    );
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
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .expect("tools array");
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
    let resources = resp["result"]["resources"]
        .as_array()
        .expect("resources array");
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

#[test]
fn server_request_roundtrip() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "ask-client", "arguments": {}}
    }));

    let request = w.recv_any();
    assert_eq!(request["method"], "sampling/createMessage");
    assert_eq!(request["id"], "server-ask-1");

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": "server-ask-1",
        "result": {"content": {"type": "text", "text": "client-answer"}}
    }));

    let call_resp = w.recv();
    assert_eq!(call_resp["id"], 2);
    assert_eq!(call_resp["result"]["content"][0]["text"], "client-answer");

    w.kill();
}

// ── Daemon mode tests ─────────────────────────────────────────────────────────

#[test]
fn daemon_basic_flow() {
    // First wrapper: starts broker
    let mut w1 = Wrapper::spawn(&["--daemon", "--init-timeout", "10"]);
    let init1 = w1.handshake();
    assert_eq!(init1["result"]["serverInfo"]["name"], "echo-server");

    // tools/list from broker cache
    w1.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    let tools = w1.recv();
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 1);

    // tools/call through broker
    w1.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "daemon-test"}}
    }));
    let call = w1.recv();
    assert_eq!(call["result"]["content"][0]["text"], "daemon-test");

    w1.kill();

    // Wait a moment then clean up broker socket
    std::thread::sleep(std::time::Duration::from_millis(200));
}

#[test]
fn daemon_two_clients_share_broker() {
    // First client starts broker
    let mut w1 = Wrapper::spawn(&["--daemon", "--init-timeout", "10"]);
    w1.handshake();

    // Second client connects to same broker
    let mut w2 = Wrapper::spawn(&["--daemon", "--init-timeout", "10"]);
    let init2 = w2.handshake();
    assert_eq!(init2["result"]["serverInfo"]["name"], "echo-server");

    // Both can make calls
    w1.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "from-w1"}}
    }));
    w2.send(&json!({
        "jsonrpc": "2.0", "id": 20, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "from-w2"}}
    }));

    let r1 = w1.recv();
    let r2 = w2.recv();
    assert_eq!(r1["result"]["content"][0]["text"], "from-w1");
    assert_eq!(r2["result"]["content"][0]["text"], "from-w2");

    w1.kill();
    w2.kill();

    std::thread::sleep(std::time::Duration::from_millis(200));
}

#[test]
fn daemon_ping_local() {
    let mut w = Wrapper::spawn(&["--daemon", "--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    let resp = w.recv();
    assert!(resp.get("result").is_some());
    assert!(resp.get("error").is_none());

    w.kill();
}

#[test]
fn daemon_server_request_roundtrip() {
    let unique_arg = format!("--daemon-peer-test-{}", std::process::id());
    let mut w = Wrapper::spawn_with_backend_args(
        &["--daemon", "--init-timeout", "10"],
        &[unique_arg.as_str()],
    );
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "ask-client", "arguments": {}}
    }));

    let request = w.recv_any();
    assert_eq!(request["method"], "sampling/createMessage");
    assert_eq!(request["id"], "server-ask-1");

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": "server-ask-1",
        "result": {"content": {"type": "text", "text": "daemon-client-answer"}}
    }));

    let call_resp = w.recv();
    assert_eq!(call_resp["id"], 2);
    assert_eq!(
        call_resp["result"]["content"][0]["text"],
        "daemon-client-answer"
    );

    w.kill();
    std::thread::sleep(std::time::Duration::from_millis(200));
}

#[test]
fn daemon_server_request_id_collision_roundtrip() {
    let unique_arg = format!("--daemon-peer-collision-test-{}", std::process::id());
    let mut w = Wrapper::spawn_with_backend_args(
        &["--daemon", "--init-timeout", "10"],
        &[unique_arg.as_str(), "--peer-id-collides"],
    );
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "ask-client", "arguments": {}}
    }));

    let request = w
        .recv_timeout(Duration::from_secs(2))
        .expect("server request with colliding id");
    assert_eq!(request["method"], "sampling/createMessage");
    let peer_id = request["id"].clone();
    assert!(
        peer_id.is_number(),
        "fixture should collide with backend id"
    );

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": peer_id,
        "result": {"content": {"type": "text", "text": "daemon-collision-answer"}}
    }));

    let call_resp = w
        .recv_timeout(Duration::from_secs(2))
        .expect("final tools/call response after peer id collision");
    assert_eq!(call_resp["id"], 2);
    assert_eq!(
        call_resp["result"]["content"][0]["text"],
        "daemon-collision-answer"
    );

    w.kill();
    std::thread::sleep(std::time::Duration::from_millis(200));
}

#[test]
fn daemon_broker_uses_cli_init_timeout() {
    let unique_arg = format!("--daemon-timeout-test-{}", std::process::id());
    let out = Command::new(wrapper_binary())
        .args([
            "--daemon",
            "--init-timeout",
            "1",
            "python3",
            echo_server_path().to_str().unwrap(),
            unique_arg.as_str(),
            "--sleep-init",
            "2",
        ])
        .output()
        .expect("failed to run daemon timeout test");

    assert!(
        !out.status.success(),
        "daemon should propagate short init timeout to broker"
    );
}
