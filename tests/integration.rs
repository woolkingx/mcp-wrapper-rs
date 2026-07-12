//! Integration tests: spawn the mcp-wrapper-rs binary and interact via
//! stdin/stdout JSON-RPC to verify end-to-end behavior.
//!
//! Uses the echo_server.py fixture as the backend MCP server.

use serde_json::{json, Value};
use std::io::ErrorKind;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    backend_args: Vec<String>,
    daemon: bool,
    cleaned: bool,
}

impl Wrapper {
    fn spawn(extra_args: &[&str]) -> Self {
        Self::spawn_with_backend_args(extra_args, &[])
    }

    fn spawn_with_backend_args(extra_args: &[&str], backend_args: &[&str]) -> Self {
        Self::spawn_with_env_backend_args(&[], extra_args, backend_args)
    }

    fn spawn_with_env_backend_args(
        envs: &[(&str, &str)],
        extra_args: &[&str],
        backend_args: &[&str],
    ) -> Self {
        let mut cmd = Command::new(wrapper_binary());
        for (key, value) in envs {
            cmd.env(key, value);
        }
        cmd.args(extra_args);
        cmd.arg("python3").arg(echo_server_path());
        cmd.args(backend_args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("failed to spawn wrapper");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let daemon = extra_args.iter().any(|arg| *arg == "--daemon");
        Wrapper {
            child,
            stdin,
            reader,
            backend_args: backend_args.iter().map(|arg| (*arg).to_string()).collect(),
            daemon,
            cleaned: false,
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
        self.cleanup();
    }

    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.daemon {
            let mut target = vec![
                "python3".to_string(),
                echo_server_path().to_string_lossy().to_string(),
            ];
            target.extend(self.backend_args.clone());
            stop_admin_broker(&target);
        }
    }
}

impl Drop for Wrapper {
    fn drop(&mut self) {
        self.cleanup();
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

fn unique_daemon_arg(label: &str) -> String {
    format!("--test-target-{label}-{}", std::process::id())
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

fn run_admin_json_any(prefix: &[&str], target: &[String]) -> (bool, Value, String) {
    let out = run_cli(&admin_args(prefix, target));
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "bad admin JSON: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            stderr
        )
    });
    (out.status.success(), value, stderr)
}

fn stop_admin_broker(target: &[String]) {
    let _ = run_cli(&admin_args(&["broker", "stop", "--json"], target));
    std::thread::sleep(Duration::from_millis(150));
}

fn wrapper_envelope(resp: &Value) -> &Value {
    &resp["result"]["_meta"]["mcpWrapper"]
}

fn sleep_call_target(suffix: &str, unique_arg: &str) -> Vec<String> {
    vec![
        "python3".to_string(),
        echo_server_path().to_string_lossy().to_string(),
        unique_arg.to_string(),
        "--sleep-call".to_string(),
        "1.0".to_string(),
        format!("--{}", suffix),
    ]
}

fn temp_marker(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mcp-wrapper-rs-{name}-{}", std::process::id()))
}

fn wait_for_marker(path: &PathBuf, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(value) = std::fs::read_to_string(path) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "marker was not written: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn current_protocol_date() -> String {
    let unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut local_time = libc::tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 1,
        tm_mon: 0,
        tm_year: 70,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: -1,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        tm_gmtoff: 0,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        tm_zone: std::ptr::null(),
    };
    let raw_time = unix_seconds as libc::time_t;
    let ok = unsafe { !libc::localtime_r(&raw_time, &mut local_time).is_null() };
    if ok {
        return format!(
            "{:04}-{:02}-{:02}",
            local_time.tm_year + 1900,
            local_time.tm_mon + 1,
            local_time.tm_mday
        );
    }
    protocol_date_for_unix_days(unix_seconds / 86_400)
}

fn protocol_date_for_unix_days(unix_days: i64) -> String {
    let z = unix_days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    format!("{year:04}-{month:02}-{day:02}")
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
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().any(|tool| tool["name"] == "echo"));
    assert!(tools.iter().any(|tool| tool["name"] == "mcp.wrapper"));

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
fn initialize_backend_handshake_uses_today_protocol_version() {
    let marker = temp_marker("init-protocol");
    let _ = std::fs::remove_file(&marker);
    let marker_str = marker.to_string_lossy().to_string();
    let mut w = Wrapper::spawn_with_backend_args(
        &["--init-timeout", "10"],
        &["--init-protocol-marker", &marker_str],
    );
    w.handshake();

    let backend_protocol: Value =
        serde_json::from_str(&wait_for_marker(&marker, Duration::from_secs(2))).unwrap();
    assert_eq!(backend_protocol, json!(current_protocol_date()));
    assert_ne!(backend_protocol, json!("2024-11-05"));

    w.kill();
    let _ = std::fs::remove_file(&marker);
}

#[test]
fn tools_list_cache_drains_backend_pages_and_rejects_frontend_cursor() {
    let mut w = Wrapper::spawn_with_backend_args(&["--init-timeout", "10"], &["--paginate-tools"]);
    w.handshake();

    w.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    let tools_resp = w.recv();
    assert!(
        tools_resp["result"].get("nextCursor").is_none(),
        "frontend cached list should not expose backend cursor: {tools_resp}"
    );
    let tools = tools_resp["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "echo"));
    assert!(tools.iter().any(|tool| tool["name"] == "paged"));
    assert!(tools.iter().any(|tool| tool["name"] == "mcp.wrapper"));

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/list",
        "params": {"cursor": "page-2"}
    }));
    let cursor_resp = w.recv();
    assert_eq!(cursor_resp["error"]["code"], -32602);

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/list",
        "params": {"cursor": null}
    }));
    let null_cursor_resp = w.recv();
    assert!(
        null_cursor_resp.get("result").is_some(),
        "{null_cursor_resp}"
    );

    w.kill();
}

#[test]
fn mcp_timeout_env_times_out_pass_through_and_discards_late_response() {
    let mut w = Wrapper::spawn_with_env_backend_args(
        &[("MCP_TIMEOUT", "200")],
        &["--init-timeout", "10"],
        &["--sleep-call", "1.0"],
    );
    w.handshake();

    let start = Instant::now();
    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "too slow"}}
    }));
    let resp = w.recv();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "timeout response should beat backend sleep"
    );
    assert_eq!(resp["id"], 2);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("timed out"),
        "response: {resp}"
    );

    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        w.recv_timeout(Duration::from_millis(200)).is_none(),
        "late backend response should be discarded"
    );
    w.kill();
}

#[test]
fn client_cancel_translates_request_id_and_discards_late_response() {
    let marker = temp_marker("client-cancel");
    let request_marker = temp_marker("client-cancel-ready");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&request_marker);
    let marker_str = marker.to_string_lossy().to_string();
    let request_marker_str = request_marker.to_string_lossy().to_string();
    let mut w = Wrapper::spawn_with_backend_args(
        &["--init-timeout", "10"],
        &[
            "--cancel-marker",
            &marker_str,
            "--request-marker",
            &request_marker_str,
            "--respond-after-cancel",
        ],
    );
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": "client-cancel-1",
        "method": "tools/call",
        "params": {"name": "wait-cancel", "arguments": {}}
    }));
    let backend_request_id: Value =
        serde_json::from_str(&wait_for_marker(&request_marker, Duration::from_secs(2))).unwrap();
    assert_ne!(backend_request_id, json!("client-cancel-1"));
    w.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": "client-cancel-1", "reason": "test cancel"}
    }));

    let backend_cancel_id: Value =
        serde_json::from_str(&wait_for_marker(&marker, Duration::from_secs(2))).unwrap();
    assert_ne!(backend_cancel_id, json!("client-cancel-1"));
    assert!(
        w.recv_timeout(Duration::from_millis(300)).is_none(),
        "late backend response after client cancel should be discarded"
    );

    w.kill();
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&request_marker);
}

#[test]
fn backend_progress_requires_active_progress_token() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "progress",
            "arguments": {"msg": "done"},
            "_meta": {"progressToken": "progress-token-1"}
        }
    }));
    let progress = w.recv_any();
    assert_eq!(progress["method"], "notifications/progress");
    assert_eq!(progress["params"]["progressToken"], "progress-token-1");
    let response = w.recv();
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["content"][0]["text"], "done");

    w.kill();
}

#[test]
fn backend_progress_with_wrong_token_is_dropped() {
    let mut w =
        Wrapper::spawn_with_backend_args(&["--init-timeout", "10"], &["--wrong-progress-token"]);
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "progress",
            "arguments": {"msg": "done"},
            "_meta": {"progressToken": "progress-token-1"}
        }
    }));
    let response = w.recv_any();
    assert_eq!(
        response["id"], 2,
        "wrong progress token should be dropped before response: {response}"
    );
    assert_eq!(response["result"]["content"][0]["text"], "done");

    w.kill();
}

#[test]
fn daemon_client_cancel_translates_request_id_and_discards_late_response() {
    let unique_arg = format!("--daemon-client-cancel-{}", std::process::id());
    let marker = temp_marker("daemon-client-cancel");
    let request_marker = temp_marker("daemon-client-cancel-ready");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&request_marker);
    let marker_str = marker.to_string_lossy().to_string();
    let request_marker_str = request_marker.to_string_lossy().to_string();
    let backend_args = [
        unique_arg.as_str(),
        "--cancel-marker",
        &marker_str,
        "--request-marker",
        &request_marker_str,
        "--respond-after-cancel",
    ];
    let target = vec![
        "python3".to_string(),
        echo_server_path().to_string_lossy().to_string(),
        unique_arg.clone(),
        "--cancel-marker".to_string(),
        marker_str.clone(),
        "--request-marker".to_string(),
        request_marker_str.clone(),
        "--respond-after-cancel".to_string(),
    ];
    stop_admin_broker(&target);

    let mut w =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w.handshake();
    w.send(&json!({
        "jsonrpc": "2.0",
        "id": "daemon-cancel-1",
        "method": "tools/call",
        "params": {"name": "wait-cancel", "arguments": {}}
    }));
    let backend_request_id: Value =
        serde_json::from_str(&wait_for_marker(&request_marker, Duration::from_secs(2))).unwrap();
    assert_ne!(backend_request_id, json!("daemon-cancel-1"));
    w.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": "daemon-cancel-1", "reason": "test cancel"}
    }));

    let backend_cancel_id: Value =
        serde_json::from_str(&wait_for_marker(&marker, Duration::from_secs(2))).unwrap();
    assert_ne!(backend_cancel_id, json!("daemon-cancel-1"));
    assert!(
        w.recv_timeout(Duration::from_millis(300)).is_none(),
        "daemon late backend response after client cancel should be discarded"
    );

    w.kill();
    stop_admin_broker(&target);
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&request_marker);
}

#[test]
fn daemon_backend_progress_requires_active_progress_token() {
    let unique_arg = format!("--daemon-progress-{}", std::process::id());
    let backend_args = [unique_arg.as_str()];
    let target = vec![
        "python3".to_string(),
        echo_server_path().to_string_lossy().to_string(),
        unique_arg.clone(),
    ];
    stop_admin_broker(&target);

    let mut w =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w.handshake();
    w.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "progress",
            "arguments": {"msg": "done"},
            "_meta": {"progressToken": "daemon-progress-token-1"}
        }
    }));
    let progress = w.recv_any();
    assert_eq!(progress["method"], "notifications/progress");
    assert_eq!(
        progress["params"]["progressToken"],
        "daemon-progress-token-1"
    );
    let response = w.recv();
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["content"][0]["text"], "done");

    w.kill();
    stop_admin_broker(&target);
}

#[test]
fn initialized_notification_is_not_reforwarded_to_live_backend() {
    let marker = temp_marker("duplicate-initialized");
    let _ = std::fs::remove_file(&marker);
    let marker_str = marker.to_string_lossy().to_string();
    let mut w = Wrapper::spawn_with_backend_args(
        &["--init-timeout", "10"],
        &["--duplicate-initialized-marker", &marker_str],
    );
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "first"}}
    }));
    let first = w.recv();
    assert_eq!(first["result"]["content"][0]["text"], "first");

    w.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !marker.exists(),
        "duplicate initialized should be dropped before backend"
    );

    w.kill();
    let _ = std::fs::remove_file(&marker);
}

#[test]
fn daemon_mcp_timeout_env_times_out_pass_through() {
    let unique_arg = format!("--daemon-request-timeout-test-{}", std::process::id());
    let backend_args = [unique_arg.as_str(), "--sleep-call", "1.0"];
    let target = vec![
        "python3".to_string(),
        echo_server_path().to_string_lossy().to_string(),
        unique_arg.clone(),
        "--sleep-call".to_string(),
        "1.0".to_string(),
    ];
    let mut w = Wrapper::spawn_with_env_backend_args(
        &[("MCP_TIMEOUT", "200")],
        &["--daemon", "--init-timeout", "10"],
        &backend_args,
    );
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "too slow"}}
    }));
    let resp = w.recv();
    assert_eq!(resp["id"], 2);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("timed out"),
        "response: {resp}"
    );

    w.kill();
    stop_admin_broker(&target);
}

#[test]
fn wrapper_tool_status_is_injected_and_local() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    let tools_resp = w.recv();
    let tools = tools_resp["result"]["tools"].as_array().unwrap();
    let wrapper_tool = tools
        .iter()
        .find(|tool| tool["name"] == "mcp.wrapper")
        .expect("mcp.wrapper tool");
    assert_eq!(wrapper_tool["inputSchema"]["required"][0], "action");

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "mcp.wrapper",
            "arguments": {
                "action": "backend.status"
            }
        }
    }));
    let status = w.recv();
    let envelope = wrapper_envelope(&status);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["action"], "backend.status");
    assert_eq!(envelope["target"], "default");
    assert_eq!(envelope["result"]["backend"]["state"], "cold");
    assert_eq!(envelope["result"]["backend"]["alive"], false);
    assert_eq!(status["result"]["isError"], false);

    w.kill();
}

#[test]
fn wrapper_tool_restart_keeps_transport_alive() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "before-restart"}}
    }));
    let before = w.recv();
    assert_eq!(before["result"]["content"][0]["text"], "before-restart");

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "mcp.wrapper",
            "arguments": {
                "action": "backend.restart",
                "params": {"force": true}
            }
        }
    }));
    let restart = w.recv();
    let envelope = wrapper_envelope(&restart);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["action"], "backend.restart");
    assert!(envelope["result"]["newCacheEpoch"].as_u64().unwrap() >= 1);

    w.send(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "after-restart"}}
    }));
    let after = w.recv();
    assert_eq!(after["result"]["content"][0]["text"], "after-restart");

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

#[test]
fn unmatched_client_response_is_not_forwarded_to_backend() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "backend-live"}}
    }));
    let call = w.recv();
    assert_eq!(call["result"]["content"][0]["text"], "backend-live");

    w.send(&json!({
        "jsonrpc": "2.0",
        "id": "not-an-active-peer-request",
        "result": {"content": {"type": "text", "text": "stray"}}
    }));
    assert!(
        w.recv_timeout(Duration::from_millis(300)).is_none(),
        "unmatched client response must not be forwarded to backend"
    );

    w.kill();
}

#[test]
fn client_progress_for_backend_peer_request_is_forwarded() {
    let marker = temp_marker("peer-progress");
    let _ = std::fs::remove_file(&marker);
    let marker_str = marker.to_string_lossy().to_string();
    let mut w = Wrapper::spawn_with_backend_args(
        &["--init-timeout", "10"],
        &["--peer-progress-marker", &marker_str],
    );
    w.handshake();

    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "ask-client", "arguments": {}}
    }));
    let request = w.recv_any();
    assert_eq!(request["method"], "sampling/createMessage");
    assert_eq!(
        request["params"]["_meta"]["progressToken"],
        "peer-progress-token-1"
    );
    w.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {"progressToken": "peer-progress-token-1", "progress": 1}
    }));
    let token: Value = serde_json::from_str(&wait_for_marker(&marker, Duration::from_secs(2)))
        .expect("peer progress marker JSON");
    assert_eq!(token, json!("peer-progress-token-1"));
    w.send(&json!({
        "jsonrpc": "2.0",
        "id": "server-ask-1",
        "result": {"content": {"type": "text", "text": "client-answer"}}
    }));
    let call_resp = w.recv();
    assert_eq!(call_resp["id"], 2);

    w.kill();
    let _ = std::fs::remove_file(&marker);
}

// ── Daemon mode tests ─────────────────────────────────────────────────────────

#[test]
fn daemon_basic_flow() {
    let target_arg = unique_daemon_arg("basic");
    // First wrapper: starts broker
    let mut w1 = Wrapper::spawn_with_backend_args(
        &["--daemon", "--init-timeout", "10"],
        &[target_arg.as_str()],
    );
    let init1 = w1.handshake();
    assert_eq!(init1["result"]["serverInfo"]["name"], "echo-server");

    // tools/list from broker cache
    w1.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    let tools = w1.recv();
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 2);
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "mcp.wrapper"));

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
fn daemon_wrapper_tool_restart_rejects_active_call() {
    let unique_arg = format!("--daemon-wrapper-restart-active-{}", std::process::id());
    let target = sleep_call_target("target", &unique_arg);
    stop_admin_broker(&target);
    let backend_args = [unique_arg.as_str(), "--sleep-call", "1.0", "--target"];

    let mut w1 =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w1.handshake();
    let mut w2 =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w2.handshake();

    w1.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "slow-restart"}}
    }));
    std::thread::sleep(Duration::from_millis(150));

    w2.send(&json!({
        "jsonrpc": "2.0", "id": 20, "method": "tools/call",
        "params": {
            "name": "mcp.wrapper",
            "arguments": {"action": "backend.restart"}
        }
    }));
    let rejected = w2.recv();
    let envelope = wrapper_envelope(&rejected);
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "activeCalls");
    assert_eq!(rejected["result"]["isError"], true);

    let delayed = w1.recv();
    assert_eq!(delayed["result"]["content"][0]["text"], "slow-restart");

    w1.kill();
    w2.kill();
    stop_admin_broker(&target);
}

#[test]
fn daemon_wrapper_tool_mutations_notify_peers_after_success_only() {
    let unique_arg = format!("--daemon-wrapper-notify-{}", std::process::id());
    let target = vec![
        "python3".to_string(),
        echo_server_path().to_string_lossy().to_string(),
        unique_arg.clone(),
    ];
    stop_admin_broker(&target);

    let mut w1 = Wrapper::spawn_with_backend_args(
        &["--daemon", "--init-timeout", "10"],
        &[unique_arg.as_str()],
    );
    w1.handshake();
    let mut w2 = Wrapper::spawn_with_backend_args(
        &["--daemon", "--init-timeout", "10"],
        &[unique_arg.as_str()],
    );
    w2.handshake();

    w1.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": {"name": "mcp.wrapper", "arguments": {"action": "backend.status"}}
    }));
    assert_eq!(wrapper_envelope(&w1.recv())["ok"], true);
    assert!(
        w2.recv_timeout(Duration::from_millis(200)).is_none(),
        "non-mutating wrapper action must not notify peers"
    );

    w1.send(&json!({
        "jsonrpc": "2.0", "id": 10_001, "method": "tools/call",
        "params": {
            "name": "mcp.wrapper",
            "arguments": {"action": "backend.restart", "params": {"force": "yes"}}
        }
    }));
    assert_eq!(wrapper_envelope(&w1.recv())["ok"], false);
    assert!(
        w2.recv_timeout(Duration::from_millis(200)).is_none(),
        "failed wrapper mutation must not notify peers"
    );

    for (id, action) in [(11, "backend.refresh"), (12, "backend.restart")] {
        w1.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "mcp.wrapper", "arguments": {"action": action}}
        }));
        let response = w1.recv();
        assert_eq!(wrapper_envelope(&response)["ok"], true, "{response}");

        let mut methods = Vec::new();
        for _ in 0..3 {
            let notification = w2
                .recv_timeout(Duration::from_secs(1))
                .expect("successful mutation should notify peer");
            methods.push(
                notification["method"]
                    .as_str()
                    .expect("notification method")
                    .to_string(),
            );
        }
        methods.sort();
        assert_eq!(
            methods,
            vec![
                "notifications/prompts/list_changed".to_string(),
                "notifications/resources/list_changed".to_string(),
                "notifications/tools/list_changed".to_string(),
            ]
        );
        assert!(w2.recv_timeout(Duration::from_millis(200)).is_none());
    }

    w1.kill();
    w2.kill();
    stop_admin_broker(&target);
}

#[test]
fn daemon_wrapper_tool_stop_rejects_active_call() {
    let unique_arg = format!("--daemon-wrapper-stop-active-{}", std::process::id());
    let target = sleep_call_target("target", &unique_arg);
    stop_admin_broker(&target);
    let backend_args = [unique_arg.as_str(), "--sleep-call", "1.0", "--target"];

    let mut w1 =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w1.handshake();
    let mut w2 =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w2.handshake();

    w1.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "slow-stop"}}
    }));
    std::thread::sleep(Duration::from_millis(150));

    w2.send(&json!({
        "jsonrpc": "2.0", "id": 20, "method": "tools/call",
        "params": {
            "name": "mcp.wrapper",
            "arguments": {"action": "backend.stop"}
        }
    }));
    let rejected = w2.recv();
    let envelope = wrapper_envelope(&rejected);
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "activeCalls");
    assert_eq!(rejected["result"]["isError"], true);

    let delayed = w1.recv();
    assert_eq!(delayed["result"]["content"][0]["text"], "slow-stop");

    w1.kill();
    w2.kill();
    stop_admin_broker(&target);
}

#[test]
fn daemon_two_clients_share_broker() {
    let target_arg = unique_daemon_arg("two-clients");
    // First client starts broker
    let mut w1 = Wrapper::spawn_with_backend_args(
        &["--daemon", "--init-timeout", "10"],
        &[target_arg.as_str()],
    );
    w1.handshake();

    // Second client connects to same broker
    let mut w2 = Wrapper::spawn_with_backend_args(
        &["--daemon", "--init-timeout", "10"],
        &[target_arg.as_str()],
    );
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
fn daemon_legacy_restart_rejects_active_call() {
    let unique_arg = format!("--daemon-legacy-restart-active-{}", std::process::id());
    let target = sleep_call_target("target", &unique_arg);
    stop_admin_broker(&target);
    let backend_args = [unique_arg.as_str(), "--sleep-call", "1.0", "--target"];

    let mut w1 =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w1.handshake();
    let mut w2 =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w2.handshake();

    w1.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "slow-legacy"}}
    }));
    std::thread::sleep(Duration::from_millis(150));

    w2.send(&json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "mcp-wrapper/backend/restart",
        "params": {}
    }));
    let rejected = w2.recv();
    assert_eq!(rejected["error"]["code"], -32603);
    assert_eq!(
        rejected["error"]["message"],
        "backend restart rejected while calls are active"
    );

    let delayed = w1.recv();
    assert_eq!(delayed["result"]["content"][0]["text"], "slow-legacy");

    w1.kill();
    w2.kill();
    stop_admin_broker(&target);
}

#[test]
fn cli_admin_backend_stop_rejects_active_daemon_call() {
    let unique_arg = format!("--cli-stop-active-{}", std::process::id());
    let target = sleep_call_target("target", &unique_arg);
    stop_admin_broker(&target);
    let backend_args = [unique_arg.as_str(), "--sleep-call", "1.0", "--target"];

    let mut w =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w.handshake();
    w.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "slow-cli-stop"}}
    }));
    std::thread::sleep(Duration::from_millis(150));

    let (success, stopped, _stderr) = run_admin_json_any(&["backend", "stop", "--json"], &target);
    assert!(!success);
    assert_eq!(stopped["ok"], false);
    assert_eq!(stopped["error"]["code"], "activeCalls");

    let delayed = w.recv();
    assert_eq!(delayed["result"]["content"][0]["text"], "slow-cli-stop");

    w.kill();
    stop_admin_broker(&target);
}

#[test]
fn cli_admin_backend_restart_rejects_active_daemon_call() {
    let unique_arg = format!("--cli-restart-active-{}", std::process::id());
    let target = sleep_call_target("target", &unique_arg);
    stop_admin_broker(&target);
    let backend_args = [unique_arg.as_str(), "--sleep-call", "1.0", "--target"];

    let mut w =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w.handshake();
    w.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"msg": "slow-cli-restart"}}
    }));
    std::thread::sleep(Duration::from_millis(150));

    let (success, restarted, _stderr) =
        run_admin_json_any(&["backend", "restart", "--json"], &target);
    assert!(!success);
    assert_eq!(restarted["ok"], false);
    assert_eq!(restarted["error"]["code"], "activeCalls");

    let delayed = w.recv();
    assert_eq!(delayed["result"]["content"][0]["text"], "slow-cli-restart");

    w.kill();
    stop_admin_broker(&target);
}

#[test]
fn daemon_ping_local() {
    let target_arg = unique_daemon_arg("ping");
    let mut w = Wrapper::spawn_with_backend_args(
        &["--daemon", "--init-timeout", "10"],
        &[target_arg.as_str()],
    );
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
fn daemon_client_progress_for_backend_peer_request_is_forwarded() {
    let unique_arg = format!("--daemon-peer-progress-{}", std::process::id());
    let marker = temp_marker("daemon-peer-progress");
    let _ = std::fs::remove_file(&marker);
    let marker_str = marker.to_string_lossy().to_string();
    let backend_args = [unique_arg.as_str(), "--peer-progress-marker", &marker_str];
    let target = vec![
        "python3".to_string(),
        echo_server_path().to_string_lossy().to_string(),
        unique_arg.clone(),
        "--peer-progress-marker".to_string(),
        marker_str.clone(),
    ];
    stop_admin_broker(&target);

    let mut w =
        Wrapper::spawn_with_backend_args(&["--daemon", "--init-timeout", "10"], &backend_args);
    w.handshake();
    w.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "ask-client", "arguments": {}}
    }));

    let request = w.recv_any();
    assert_eq!(request["method"], "sampling/createMessage");
    assert_eq!(
        request["params"]["_meta"]["progressToken"],
        "peer-progress-token-1"
    );
    w.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {"progressToken": "peer-progress-token-1", "progress": 1}
    }));
    let token: Value = serde_json::from_str(&wait_for_marker(&marker, Duration::from_secs(2)))
        .expect("daemon peer progress marker JSON");
    assert_eq!(token, json!("peer-progress-token-1"));
    w.send(&json!({
        "jsonrpc": "2.0",
        "id": "server-ask-1",
        "result": {"content": {"type": "text", "text": "daemon-client-answer"}}
    }));
    let call_resp = w.recv();
    assert_eq!(call_resp["id"], 2);

    w.kill();
    stop_admin_broker(&target);
    let _ = std::fs::remove_file(&marker);
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
