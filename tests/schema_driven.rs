use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout, Duration};

#[path = "support/mod.rs"]
mod support;
use support::ObjectTree;

fn full_capability_server_script() -> String {
    r#"
import json
import sys
import time


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def next_msg():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        if line.startswith("Content-Length:"):
            try:
                length = int(line.split(":", 1)[1].strip())
            except Exception:
                continue
            while True:
                hdr = sys.stdin.readline()
                if not hdr:
                    return None
                if hdr in ("\n", "\r\n"):
                    break
            body = sys.stdin.read(length)
            if not body:
                return None
            try:
                return json.loads(body)
            except Exception:
                continue
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except Exception:
            continue


while True:
    msg = next_msg()
    if msg is None:
        break

    method = msg.get("method")
    mid = msg.get("id")

    if method == "notifications/initialized":
        continue

    if mid is None:
        continue

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {
                    "tools": {},
                    "prompts": {},
                    "resources": {}
                },
                "serverInfo": {
                    "name": "full-capability-server",
                    "version": "0.1.0"
                }
            }
        })
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo input",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "msg": {"type": "string"}
                            },
                            "required": ["msg"]
                        }
                    }
                ]
            }
        })
    elif method == "tools/call":
        args = (msg.get("params", {}) or {}).get("arguments", {}) or {}
        text = args.get("msg", "")
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "content": [
                    {"type": "text", "text": text}
                ]
            }
        })
    elif method == "prompts/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "prompts": [
                    {
                        "name": "hello",
                        "description": "Say hello"
                    }
                ]
            }
        })
    elif method == "prompts/get":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "description": "hello prompt",
                "messages": [
                    {
                        "role": "assistant",
                        "content": {"type": "text", "text": "hello"}
                    }
                ]
            }
        })
    elif method == "resources/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "resources": [
                    {
                        "uri": "file://test.txt",
                        "name": "test"
                    }
                ]
            }
        })
    elif method == "resources/read":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "contents": [
                    {
                        "uri": "file://test.txt",
                        "text": "test content"
                    }
                ]
            }
        })
    elif method == "resources/templates/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "resourceTemplates": []
            }
        })
    else:
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "error": {
                "code": -32601,
                "message": "Method not found"
            }
        })
"#
    .to_string()
}

fn backend_error_server_script() -> String {
    r#"
import json
import sys


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def next_msg():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        if line.startswith("Content-Length:"):
            try:
                length = int(line.split(":", 1)[1].strip())
            except Exception:
                continue
            while True:
                hdr = sys.stdin.readline()
                if not hdr:
                    return None
                if hdr in ("\n", "\r\n"):
                    break
            body = sys.stdin.read(length)
            if not body:
                return None
            try:
                return json.loads(body)
            except Exception:
                continue
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except Exception:
            continue


while True:
    msg = next_msg()
    if msg is None:
        break

    method = msg.get("method")
    mid = msg.get("id")

    if method == "notifications/initialized":
        continue

    if mid is None:
        continue

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "backend-error-server", "version": "0.1.0"}
            }
        })
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo input",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"msg": {"type": "string"}}
                        }
                    }
                ]
            }
        })
    elif method == "tools/call":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "error": {
                "code": -32001,
                "message": "backend tool failure"
            }
        })
    else:
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "error": {
                "code": -32601,
                "message": "Method not found"
            }
        })
"#
    .to_string()
}

fn slow_tool_server_script() -> String {
    r#"
import json
import sys
import time


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def next_msg():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        if line.startswith("Content-Length:"):
            try:
                length = int(line.split(":", 1)[1].strip())
            except Exception:
                continue
            while True:
                hdr = sys.stdin.readline()
                if not hdr:
                    return None
                if hdr in ("\n", "\r\n"):
                    break
            body = sys.stdin.read(length)
            if not body:
                return None
            try:
                return json.loads(body)
            except Exception:
                continue
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except Exception:
            continue


while True:
    msg = next_msg()
    if msg is None:
        break

    method = msg.get("method")
    mid = msg.get("id")

    if method == "notifications/initialized":
        continue

    if mid is None:
        continue

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "slow-tool-server", "version": "0.1.0"}
            }
        })
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo input",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"msg": {"type": "string"}}
                        }
                    }
                ]
            }
        })
    elif method == "tools/call":
        args = (msg.get("params", {}) or {}).get("arguments", {}) or {}
        text = args.get("msg", "")
        time.sleep(3)
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "content": [
                    {"type": "text", "text": text}
                ]
            }
        })
    else:
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "error": {
                "code": -32601,
                "message": "Method not found"
            }
        })
"#
    .to_string()
}

fn exit_after_first_call_server_script() -> String {
    r#"
import json
import sys

served = 0

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

def next_msg():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        if line.startswith("Content-Length:"):
            try:
                length = int(line.split(":", 1)[1].strip())
            except Exception:
                continue
            while True:
                hdr = sys.stdin.readline()
                if not hdr:
                    return None
                if hdr in ("\n", "\r\n"):
                    break
            body = sys.stdin.read(length)
            if not body:
                return None
            try:
                return json.loads(body)
            except Exception:
                continue
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except Exception:
            continue


while True:
    msg = next_msg()
    if msg is None:
        break

    method = msg.get("method")
    mid = msg.get("id")

    if method == "notifications/initialized":
        continue

    if mid is None:
        continue

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}, "prompts": {}, "resources": {}},
                "serverInfo": {"name": "exit-after-call", "version": "0.1.0"}
            }
        })
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo input",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"msg": {"type": "string"}}
                        }
                    }
                ]
            }
        })
    elif method == "tools/call":
        args = (msg.get("params", {}) or {}).get("arguments", {}) or {}
        text = args.get("msg", "")
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "content": [
                    {"type": "text", "text": text}
                ]
            }
        })
        served += 1
        if served >= 1:
            time.sleep(0.2)
            sys.exit(0)
    elif method == "prompts/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {"prompts": [{"name": "hello", "description": "Say hello"}]}
        })
    elif method == "resources/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {"resources": [{"uri": "file://test.txt", "name": "test"}]}
        })
    elif method == "resources/templates/list":
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {"resourceTemplates": []}
        })
    else:
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "error": {
                "code": -32601,
                "message": "Method not found"
            }
        })
"#
    .to_string()
}

fn pid_server_script() -> String {
    r#"
import json
import os
import sys

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

def next_msg():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        if line.startswith("Content-Length:"):
            try:
                length = int(line.split(":", 1)[1].strip())
            except Exception:
                continue
            while True:
                hdr = sys.stdin.readline()
                if not hdr:
                    return None
                if hdr in ("\n", "\r\n"):
                    break
            body = sys.stdin.read(length)
            if not body:
                return None
            try:
                return json.loads(body)
            except Exception:
                continue
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except Exception:
            continue

while True:
    msg = next_msg()
    if msg is None:
        break

    method = msg.get("method")
    mid = msg.get("id")

    if method == "notifications/initialized":
        continue
    if mid is None:
        continue

    if method == "initialize":
        send({
            "jsonrpc":"2.0",
            "id":mid,
            "result":{
                "protocolVersion":"2025-03-26",
                "capabilities":{"tools":{}, "prompts":{}, "resources":{}},
                "serverInfo":{"name":"pid-server","version":"0.1.0"}
            }
        })
    elif method == "tools/list":
        send({
            "jsonrpc":"2.0",
            "id":mid,
            "result":{
                "tools":[
                    {"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"msg":{"type":"string"}}}},
                    {"name":"pid","description":"PID","inputSchema":{"type":"object","properties":{}}}
                ]
            }
        })
    elif method == "tools/call":
        name = (msg.get("params", {}) or {}).get("name", "")
        args = (msg.get("params", {}) or {}).get("arguments", {}) or {}
        if name == "pid":
            text = str(os.getpid())
        else:
            text = args.get("msg", "")
        send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":text}]}})
    elif method == "prompts/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"prompts":[{"name":"hello","description":"Say hello"}]}})
    elif method == "resources/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"resources":[{"uri":"file://test.txt","name":"test"}]}})
    elif method == "resources/templates/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"resourceTemplates":[]}})
    else:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"Method not found"}})
"#
    .to_string()
}

fn wrapper_bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("failed to resolve current_exe");
    path.pop();
    path.pop();
    path.push("mcp-wrapper-rs");
    path
}

fn load_schema(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("invalid json {}: {e}", path.display()))
}

fn definition(root: &Value, name: &str) -> Value {
    root["definitions"][name].clone()
}

fn assert_conforms(label: &str, data: &Value, schema_def: &Value, root: &Value) {
    let mut full_schema = schema_def.clone();
    if let Some(obj) = full_schema.as_object_mut() {
        obj.insert("definitions".to_string(), root["definitions"].clone());
    }

    let tree = ObjectTree::new(data.clone(), full_schema);
    match tree.validate() {
        Ok(()) => {}
        Err(errs) => panic!(
            "{label} failed MCP schema conformance:\n  data: {}\n  errors: {:?}",
            serde_json::to_string_pretty(data).expect("pretty json"),
            errs
        ),
    }
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos()
}

struct WrapperProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    #[allow(dead_code)]
    script_path: PathBuf,
}

impl WrapperProcess {
    async fn spawn(extra_args: &[&str]) -> Self {
        Self::spawn_with_script(&full_capability_server_script(), extra_args).await
    }

    async fn spawn_with_script(script: &str, extra_args: &[&str]) -> Self {
        let script_path = PathBuf::from(format!(
            "/tmp/mcp_schema_server_{}_{}.py",
            std::process::id(),
            unique_suffix()
        ));

        tokio::fs::write(&script_path, script)
            .await
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", script_path.display()));

        let mut cmd = Command::new(wrapper_bin());
        cmd.args(extra_args);
        cmd.arg("python3").arg(&script_path);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        cmd.env_remove("MCP_WRAPPER_DEBUG");

        let mut child = cmd.spawn().expect("failed to spawn wrapper");
        let stdin = child.stdin.take().expect("missing stdin");
        let stdout = BufReader::new(child.stdout.take().expect("missing stdout"));

        Self {
            child,
            stdin,
            stdout,
            script_path,
        }
    }

    async fn send(&mut self, msg: Value) {
        let line = serde_json::to_string(&msg).expect("serialize json") + "\n";
        self.stdin
            .write_all(line.as_bytes())
            .await
            .expect("failed to write wrapper stdin");
    }

    async fn recv(&mut self) -> Value {
        let mut buf = String::new();

        loop {
            buf.clear();
            let n = self
                .stdout
                .read_line(&mut buf)
                .await
                .expect("failed reading wrapper stdout");
            assert!(n > 0, "wrapper stdout closed before response");

            let v: Value = serde_json::from_str(buf.trim())
                .unwrap_or_else(|e| panic!("invalid json from wrapper: {e}; line={}", buf.trim()));
            if v.get("id").is_some() {
                return v;
            }
        }
    }

    async fn handshake(&mut self) -> Value {
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.0.1"}
            }
        }))
        .await;

        let init = self.recv().await;

        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await;

        init
    }

    async fn kill(mut self) {
        let _ = self.child.kill().await;
    }
}

#[tokio::test]
async fn test_tools_only_advertised() {
    let mut w = WrapperProcess::spawn(&[]).await;
    let init = w.handshake().await;
    let caps = &init["result"]["capabilities"];
    assert!(caps.get("tools").is_some(), "tools capability should be advertised");
    assert!(
        caps.get("prompts").is_none() || caps["prompts"].is_null(),
        "prompts capability should be absent or null"
    );
    assert!(
        caps.get("resources").is_none() || caps["resources"].is_null(),
        "resources capability should be absent or null"
    );
    w.kill().await;
}

#[tokio::test]
async fn test_call_tool_valid_normal() {
    let mut w = WrapperProcess::spawn(&[]).await;
    w.handshake().await;

    w.send(serde_json::json!({
        "jsonrpc":"2.0",
        "id": 2,
        "method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"sdd-test"}}
    }))
    .await;
    let resp = w.recv().await;

    assert_eq!(resp["result"]["content"][0]["text"], "sdd-test");

    let schema = load_schema("mcp-schema.json");
    let def = definition(&schema, "CallToolResult");
    assert_conforms("CallToolResult", &resp["result"], &def, &schema);

    w.kill().await;
}

#[tokio::test]
async fn test_call_tool_empty_arguments() {
    let mut w = WrapperProcess::spawn(&[]).await;
    w.handshake().await;

    w.send(serde_json::json!({
        "jsonrpc":"2.0",
        "id": 3,
        "method":"tools/call",
        "params":{"name":"echo","arguments":{}}
    }))
    .await;
    let resp = timeout(Duration::from_secs(10), w.recv())
        .await
        .expect("tools/call with empty args should not hang");
    assert!(
        resp.get("result").is_some() || resp.get("error").is_some(),
        "expected either result or error"
    );
    w.kill().await;
}

#[tokio::test]
async fn test_call_tool_backend_error_code_preserved() {
    let mut w = WrapperProcess::spawn_with_script(&backend_error_server_script(), &[]).await;
    w.handshake().await;

    w.send(serde_json::json!({
        "jsonrpc":"2.0",
        "id": 4,
        "method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"x"}}
    }))
    .await;
    let resp = w.recv().await;
    assert_eq!(resp["error"]["code"], -32001);

    w.kill().await;
}

#[tokio::test]
async fn test_call_tool_concurrent() {
    let wrapper = Arc::new(Mutex::new(WrapperProcess::spawn(&[]).await));
    {
        let mut w = wrapper.lock().await;
        w.handshake().await;
    }

    let mut set = JoinSet::new();
    for i in 0..5 {
        let w = Arc::clone(&wrapper);
        set.spawn(async move {
            let msg = format!("concurrent-{i}");
            let mut guard = w.lock().await;
            guard
                .send(serde_json::json!({
                    "jsonrpc":"2.0",
                    "id": 100 + i,
                    "method":"tools/call",
                    "params":{"name":"echo","arguments":{"msg":msg}}
                }))
                .await;
            let resp = guard.recv().await;
            (i, resp)
        });
    }

    let mut seen = [false; 5];
    while let Some(joined) = set.join_next().await {
        let (i, resp) = joined.expect("task join");
        assert!(resp.get("result").is_some(), "call should produce result");
        assert_eq!(
            resp["result"]["content"][0]["text"],
            format!("concurrent-{i}")
        );
        seen[i] = true;
    }
    assert!(seen.into_iter().all(|v| v), "all 5 calls should return");

    let mutex = match Arc::try_unwrap(wrapper) {
        Ok(m) => m,
        Err(_) => panic!("single owner"),
    };
    let w = mutex.into_inner();
    w.kill().await;
}

#[tokio::test]
async fn test_spawn_failure() {
    let mut child = Command::new(wrapper_bin())
        .arg("nonexistent_binary_xyz")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("MCP_WRAPPER_DEBUG")
        .spawn()
        .expect("spawn wrapper");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    let init = serde_json::json!({
        "jsonrpc":"2.0",
        "id": 1,
        "method":"initialize",
        "params":{
            "protocolVersion":"2025-03-26",
            "capabilities":{},
            "clientInfo":{"name":"test","version":"0.0.1"}
        }
    });
    let line = serde_json::to_string(&init).expect("json") + "\n";
    stdin.write_all(line.as_bytes()).await.expect("write init");

    let observed = timeout(Duration::from_secs(10), async {
        let mut buf = String::new();
        let n = reader.read_line(&mut buf).await.expect("read line");
        if n == 0 {
            "process_exit".to_string()
        } else {
            let v: Value = serde_json::from_str(buf.trim()).expect("json response");
            if v.get("error").is_some() {
                "error".to_string()
            } else {
                "result".to_string()
            }
        }
    })
    .await;

    assert!(observed.is_ok(), "spawn failure path must not hang");
    let _ = child.kill().await;
}

#[tokio::test]
async fn test_dead_backend_respawn() {
    let mut w = WrapperProcess::spawn_with_script(&pid_server_script(), &[]).await;
    w.handshake().await;

    w.send(serde_json::json!({
        "jsonrpc":"2.0",
        "id": 10,
        "method":"tools/call",
        "params":{"name":"pid","arguments":{}}
    }))
    .await;
    let pid_resp = w.recv().await;
    let pid = pid_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("pid text")
        .to_string();

    let status = Command::new("kill")
        .arg("-9")
        .arg(&pid)
        .status()
        .await
        .expect("kill backend by pid");
    assert!(status.success(), "kill command should succeed");
    sleep(Duration::from_millis(200)).await;

    w.send(serde_json::json!({
        "jsonrpc":"2.0",
        "id": 11,
        "method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"second"}}
    }))
    .await;
    let second = timeout(Duration::from_secs(10), w.recv())
        .await
        .expect("second tools/call should not hang");
    assert!(
        second.get("result").is_some() || second.get("error").is_some(),
        "expected either result or error after backend death"
    );

    w.kill().await;
}

#[tokio::test]
#[ignore = "needs a short BACKEND_IDLE_SECS build to exercise idle reaper timing deterministically"]
async fn test_reaper_persists_after_kill() {}

#[tokio::test]
async fn test_all_result_types_conform() {
    let schema = load_schema("mcp-schema.json");
    let defs = schema["definitions"]
        .as_object()
        .expect("definitions object");

    let mut w = WrapperProcess::spawn(&[]).await;
    let init = w.handshake().await;

    for name in defs.keys().filter(|k| k.ends_with("Result")) {
        let maybe_data = match name.as_str() {
            "InitializeResult" => Some(init["result"].clone()),
            "ListToolsResult" => {
                w.send(serde_json::json!({"jsonrpc":"2.0","id":20,"method":"tools/list","params":{}}))
                    .await;
                Some(w.recv().await["result"].clone())
            }
            "CallToolResult" => {
                w.send(serde_json::json!({
                    "jsonrpc":"2.0","id":21,"method":"tools/call",
                    "params":{"name":"echo","arguments":{"msg":"schema"}}
                }))
                .await;
                Some(w.recv().await["result"].clone())
            }
            "ListPromptsResult" => {
                w.send(serde_json::json!({"jsonrpc":"2.0","id":22,"method":"prompts/list","params":{}}))
                    .await;
                Some(w.recv().await["result"].clone())
            }
            "ListResourcesResult" => {
                w.send(serde_json::json!({"jsonrpc":"2.0","id":23,"method":"resources/list","params":{}}))
                    .await;
                Some(w.recv().await["result"].clone())
            }
            "ListResourceTemplatesResult" => {
                w.send(serde_json::json!({"jsonrpc":"2.0","id":24,"method":"resources/templates/list","params":{}}))
                    .await;
                Some(w.recv().await["result"].clone())
            }
            _ => None,
        };

        if let Some(data) = maybe_data {
            let def = definition(&schema, name);
            assert_conforms(name, &data, &def, &schema);
        }
    }

    w.kill().await;
}
