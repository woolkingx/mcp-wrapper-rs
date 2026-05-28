//! Backend hook integration tests.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

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

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{}", serde_json::to_string(msg).unwrap()).unwrap();
        self.stdin.flush().unwrap();
    }

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
        }
    }

    fn handshake(&mut self) {
        self.send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.0.1"}
            }
        }));
        let resp = self.recv();
        assert_eq!(resp["id"], 1);
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    fn status(&mut self, id: i64) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "mcp-wrapper/backend/status"
        }));
        self.recv()
    }

    fn refresh(&mut self, id: i64) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "mcp-wrapper/backend/refresh"
        }));
        self.recv()
    }

    fn restart(&mut self, id: i64) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "mcp-wrapper/backend/restart"
        }));
        self.recv()
    }

    fn restart_force(&mut self, id: i64) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "mcp-wrapper/backend/restart",
            "params": {"force": true}
        }));
        self.recv()
    }

    fn stop(&mut self, id: i64) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "mcp-wrapper/backend/stop"
        }));
        self.recv()
    }

    fn backend_ping(&mut self, id: i64) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "mcp-wrapper/backend/ping"
        }));
        self.recv()
    }

    fn call_echo(&mut self, id: i64, msg: &str) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"msg": msg}}
        }));
        self.recv()
    }

    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn backend_status_reports_generation() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    let cold = w.status(2);
    assert_eq!(cold["result"]["backend"]["state"], "cold");
    assert_eq!(cold["result"]["backend"]["alive"], false);
    assert_eq!(cold["result"]["backend"]["generation"], 0);
    assert_eq!(cold["result"]["mcp"]["cacheEpoch"], 1);
    assert!(
        cold["result"]["mcp"]["capabilitiesHash"]
            .as_str()
            .unwrap()
            .len()
            >= 8
    );
    assert!(
        cold["result"]["mcp"]["discoveryHash"]
            .as_str()
            .unwrap()
            .len()
            >= 8
    );

    let call = w.call_echo(3, "status-ready");
    assert_eq!(call["result"]["content"][0]["text"], "status-ready");

    let ready = w.status(4);
    assert_eq!(ready["result"]["backend"]["state"], "ready");
    assert_eq!(ready["result"]["backend"]["alive"], true);
    assert_eq!(ready["result"]["backend"]["generation"], 1);
    assert_eq!(ready["result"]["backend"]["restartCount"], 0);

    w.kill();
}

#[test]
fn daemon_backend_status_reports_shared_backend() {
    let unique_arg = format!("--status-hook-test-{}", std::process::id());
    let mut w = Wrapper::spawn_with_backend_args(
        &["--daemon", "--init-timeout", "10"],
        &[unique_arg.as_str()],
    );
    w.handshake();

    let status = w.status(2);
    assert_eq!(status["result"]["backend"]["state"], "ready");
    assert_eq!(status["result"]["backend"]["alive"], true);
    assert!(status["result"]["backend"]["generation"].as_u64().unwrap() >= 1);
    assert!(status["result"]["mcp"]["cacheEpoch"].as_u64().unwrap() >= 1);

    w.kill();
    std::thread::sleep(std::time::Duration::from_millis(200));
}

#[test]
fn backend_refresh_notifies_only_on_discovery_change() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    let unavailable = w.refresh(2);
    assert!(unavailable.get("error").is_some());
    assert_eq!(
        unavailable["error"]["message"],
        "backend unavailable for refresh"
    );

    let call = w.call_echo(3, "refresh-ready");
    assert_eq!(call["result"]["content"][0]["text"], "refresh-ready");

    let status = w.status(4);
    let old_epoch = status["result"]["mcp"]["cacheEpoch"].as_u64().unwrap();
    let old_hash = status["result"]["mcp"]["discoveryHash"].clone();

    let first_refresh = w.refresh(5);
    assert_eq!(first_refresh["result"]["changed"], true);
    assert_eq!(
        first_refresh["result"]["oldCacheEpoch"].as_u64().unwrap(),
        old_epoch
    );
    assert_eq!(
        first_refresh["result"]["newCacheEpoch"].as_u64().unwrap(),
        old_epoch + 1
    );
    assert_eq!(first_refresh["result"]["oldDiscoveryHash"], old_hash);
    assert_eq!(first_refresh["result"]["newDiscoveryHash"], old_hash);

    let second_refresh = w.refresh(6);
    assert_eq!(second_refresh["result"]["changed"], false);
    assert_eq!(
        second_refresh["result"]["oldCacheEpoch"],
        first_refresh["result"]["newCacheEpoch"]
    );
    assert_eq!(
        second_refresh["result"]["newCacheEpoch"],
        first_refresh["result"]["newCacheEpoch"]
    );

    w.kill();
}

#[test]
fn backend_restart_refreshes_cache_epoch() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    let call = w.call_echo(2, "restart-ready");
    assert_eq!(call["result"]["content"][0]["text"], "restart-ready");

    let before = w.status(3);
    let old_epoch = before["result"]["mcp"]["cacheEpoch"].as_u64().unwrap();
    assert_eq!(before["result"]["backend"]["generation"], 1);

    let restart = w.restart(4);
    assert_eq!(
        restart["result"]["oldCacheEpoch"].as_u64().unwrap(),
        old_epoch
    );
    assert_eq!(
        restart["result"]["newCacheEpoch"].as_u64().unwrap(),
        old_epoch + 1
    );
    assert_eq!(restart["result"]["changed"], true);

    let after = w.status(5);
    assert_eq!(after["result"]["backend"]["state"], "ready");
    assert_eq!(after["result"]["backend"]["alive"], true);
    assert_eq!(after["result"]["backend"]["generation"], 2);
    assert_eq!(after["result"]["backend"]["restartCount"], 1);
    assert_eq!(after["result"]["mcp"]["backendGeneration"], 2);

    w.kill();
}

#[test]
fn backend_restart_refreshes_server_info_from_new_backend() {
    let version_file =
        std::env::temp_dir().join(format!("mcp-wrapper-version-{}", std::process::id()));
    std::fs::write(&version_file, "0.1.0").unwrap();
    let version_arg = version_file.to_string_lossy().to_string();

    let mut w = Wrapper::spawn_with_backend_args(
        &["--init-timeout", "10"],
        &["--version-file", version_arg.as_str()],
    );
    w.handshake();

    let call = w.call_echo(2, "version-ready");
    assert_eq!(call["result"]["content"][0]["text"], "version-ready");

    let before = w.status(3);
    assert_eq!(
        before["result"]["mcp"]["serverInfo"]["serverInfo"]["version"],
        "0.1.0"
    );

    std::fs::write(&version_file, "0.2.0").unwrap();
    let restart = w.restart(4);
    assert!(restart.get("result").is_some(), "restart: {restart}");

    let after = w.status(5);
    assert_eq!(
        after["result"]["mcp"]["serverInfo"]["serverInfo"]["version"],
        "0.2.0"
    );

    let _ = std::fs::remove_file(&version_file);
    w.kill();
}

#[test]
fn normal_respawn_refreshes_server_info() {
    let version_file = std::env::temp_dir().join(format!(
        "mcp-wrapper-respawn-version-{}",
        std::process::id()
    ));
    let exit_marker = std::env::temp_dir().join(format!(
        "mcp-wrapper-exit-after-call-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&exit_marker);
    std::fs::write(&version_file, "0.1.0").unwrap();
    let version_arg = version_file.to_string_lossy().to_string();
    let exit_arg = exit_marker.to_string_lossy().to_string();

    let mut w = Wrapper::spawn_with_backend_args(
        &["--init-timeout", "10"],
        &[
            "--version-file",
            version_arg.as_str(),
            "--exit-after-call-once",
            exit_arg.as_str(),
        ],
    );
    w.handshake();

    let first = w.call_echo(2, "first");
    assert_eq!(first["result"]["content"][0]["text"], "first");
    std::thread::sleep(std::time::Duration::from_millis(200));

    std::fs::write(&version_file, "0.2.0").unwrap();
    let second = w.call_echo(3, "second");
    assert_eq!(second["result"]["content"][0]["text"], "second");

    let after = w.status(4);
    assert_eq!(
        after["result"]["mcp"]["serverInfo"]["serverInfo"]["version"],
        "0.2.0"
    );

    let _ = std::fs::remove_file(&version_file);
    let _ = std::fs::remove_file(&exit_marker);
    w.kill();
}

#[test]
fn backend_stop_and_ping_hooks_are_implemented() {
    let mut w = Wrapper::spawn(&["--init-timeout", "10"]);
    w.handshake();

    let cold_ping = w.backend_ping(2);
    assert_eq!(cold_ping["result"]["ok"], false);

    let call = w.call_echo(3, "hook-ready");
    assert_eq!(call["result"]["content"][0]["text"], "hook-ready");

    let ready_ping = w.backend_ping(4);
    assert_eq!(ready_ping["result"]["ok"], true);
    assert_eq!(ready_ping["result"]["backend"]["state"], "ready");

    let stopped = w.stop(5);
    assert_eq!(stopped["result"]["state"], "stopped");
    assert_eq!(stopped["result"]["alive"], false);

    let after_stop = w.backend_ping(6);
    assert_eq!(after_stop["result"]["ok"], false);
    assert_eq!(after_stop["result"]["backend"]["state"], "stopped");

    w.kill();
}

#[test]
fn backend_circuit_breaker_rejects_spawn_storm_until_force_restart() {
    let marker = std::env::temp_dir().join(format!(
        "mcp-wrapper-fail-after-first-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let marker_arg = marker.to_string_lossy().to_string();

    let mut w = Wrapper::spawn_with_backend_args(
        &["--init-timeout", "1"],
        &["--fail-after-first", marker_arg.as_str()],
    );
    w.handshake();

    let first = w.call_echo(2, "will-fail");
    assert!(
        first["error"]["message"]
            .as_str()
            .unwrap()
            .contains("backend spawn failed after 3 attempts")
    );

    let degraded = w.status(3);
    assert_eq!(degraded["result"]["backend"]["state"], "degraded");
    assert!(
        degraded["result"]["backend"]["consecutiveSpawnFailures"]
            .as_u64()
            .unwrap()
            >= 3
    );
    assert!(degraded["result"]["backend"]["cooldownUntilMs"].is_number());

    let start = std::time::Instant::now();
    let second = w.call_echo(4, "should-not-spawn");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "cooldown rejection should not run spawn backoff"
    );
    assert!(
        second["error"]["message"]
            .as_str()
            .unwrap()
            .contains("backend temporarily degraded")
    );

    let restart = w.restart(5);
    assert!(
        restart["error"]["message"]
            .as_str()
            .unwrap()
            .contains("restart rejected during cooldown")
    );

    let _ = std::fs::remove_file(&marker);
    let forced = w.restart_force(6);
    assert!(forced.get("result").is_some(), "forced restart: {forced}");

    let ready = w.status(7);
    assert_eq!(ready["result"]["backend"]["state"], "ready");
    assert_eq!(
        ready["result"]["backend"]["consecutiveSpawnFailures"],
        json!(0)
    );
    assert!(ready["result"]["backend"]["cooldownUntilMs"].is_null());

    let _ = std::fs::remove_file(&marker);
    w.kill();
}
