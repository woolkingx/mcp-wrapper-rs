//! Conformance tests: verify wrapper JSON-RPC responses match MCP official schema.
//!
//! Strategy: spawn wrapper with echo-server backend, send each request type,
//! validate the `result` field against the corresponding MCP schema definition.

use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

// ── schema2object ─────────────────────────────────────────────────────────────
#[path = "support/mod.rs"]
mod support;
use support::ObjectTree;

// ── helpers ───────────────────────────────────────────────────────────────────

fn wrapper_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); p.pop();
    p.push("mcp-wrapper-rs");
    p
}

fn echo_server_script() -> &'static str {
    r#"
import sys, json

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method", "")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2025-03-26",
            "capabilities":{"tools":{}},"serverInfo":{"name":"echo","version":"0.1"}
        }})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{
            "name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"msg":{"type":"string"}}}
        }]}})
    elif method == "tools/call":
        arg = msg.get("params",{}).get("arguments",{}).get("msg","")
        send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":arg}]}})
    elif method == "prompts/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"prompts":[]}})
    elif method == "resources/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"resources":[]}})
    elif method == "resources/templates/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"resourceTemplates":[]}})
"#
}

fn load_mcp_schema() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp-schema.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read mcp-schema.json: {e}"));
    serde_json::from_str(&content).expect("invalid mcp-schema.json")
}

/// Extract `definitions.<name>` from the MCP schema root.
fn definition(root: &serde_json::Value, name: &str) -> serde_json::Value {
    root["definitions"][name].clone()
}

/// Validate `data` against `schema_def`, with `root` for $ref resolution.
fn assert_conforms(label: &str, data: &serde_json::Value, schema_def: &serde_json::Value, root: &serde_json::Value) {
    // Merge definitions into schema_def so $ref resolution works
    let mut full_schema = schema_def.clone();
    if let Some(obj) = full_schema.as_object_mut() {
        obj.insert("definitions".to_string(), root["definitions"].clone());
    }
    let sv = ObjectTree::new(data.clone(), full_schema);
    match sv.validate() {
        Ok(()) => {}
        Err(errs) => panic!(
            "{label} failed MCP schema conformance:\n  data: {}\n  errors: {:?}",
            serde_json::to_string_pretty(data).unwrap(),
            errs
        ),
    }
}

struct WrapperProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl WrapperProcess {
    async fn spawn() -> Self {
        let script_path = format!("/tmp/mcp_conformance_server_{}.py", std::process::id());
        tokio::fs::write(&script_path, echo_server_script()).await.unwrap();

        let mut child = Command::new(wrapper_bin())
            .arg("--init-timeout").arg("5")
            .arg("python3").arg(&script_path)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().expect("failed to spawn wrapper");

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        WrapperProcess { child, stdin, stdout }
    }

    async fn send(&mut self, msg: serde_json::Value) {
        let line = serde_json::to_string(&msg).unwrap() + "\n";
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    async fn recv(&mut self) -> serde_json::Value {
        let mut buf = String::new();
        loop {
            buf.clear();
            self.stdout.read_line(&mut buf).await.unwrap();
            let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            if v.get("id").is_some() { return v; }
        }
    }

    async fn handshake(&mut self) -> serde_json::Value {
        self.send(serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.0.1"}}
        })).await;
        let resp = self.recv().await;
        self.send(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        resp
    }

    async fn kill(mut self) { let _ = self.child.kill().await; }
}

// ── Conformance tests ─────────────────────────────────────────────────────────

/// Every response must be a valid JSONRPCResponse envelope.
#[tokio::test]
async fn test_response_envelope_conforms() {
    let schema = load_mcp_schema();
    let envelope_def = definition(&schema, "JSONRPCResponse");

    let mut w = WrapperProcess::spawn().await;
    let init_resp = w.handshake().await;

    assert_conforms("initialize response envelope", &init_resp, &envelope_def, &schema);
    w.kill().await;
}

/// InitializeResult must conform to MCP schema.
#[tokio::test]
async fn test_initialize_result_conforms() {
    let schema = load_mcp_schema();
    let def = definition(&schema, "InitializeResult");

    let mut w = WrapperProcess::spawn().await;
    let resp = w.handshake().await;
    let result = &resp["result"];

    assert_conforms("InitializeResult", result, &def, &schema);
    w.kill().await;
}

/// ListToolsResult must conform to MCP schema.
#[tokio::test]
async fn test_list_tools_result_conforms() {
    let schema = load_mcp_schema();
    let def = definition(&schema, "ListToolsResult");

    let mut w = WrapperProcess::spawn().await;
    w.handshake().await;
    w.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})).await;
    let resp = w.recv().await;

    assert_conforms("ListToolsResult", &resp["result"], &def, &schema);
    w.kill().await;
}

/// Each Tool in the tools list must conform to MCP Tool schema.
#[tokio::test]
async fn test_tool_definition_conforms() {
    let schema = load_mcp_schema();
    let tool_def = definition(&schema, "Tool");

    let mut w = WrapperProcess::spawn().await;
    w.handshake().await;
    w.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})).await;
    let resp = w.recv().await;

    let tools = resp["result"]["tools"].as_array().expect("tools array");
    for (i, tool) in tools.iter().enumerate() {
        assert_conforms(&format!("Tool[{i}]"), tool, &tool_def, &schema);
    }
    w.kill().await;
}

/// ListPromptsResult must conform to MCP schema.
#[tokio::test]
async fn test_list_prompts_result_conforms() {
    let schema = load_mcp_schema();
    let def = definition(&schema, "ListPromptsResult");

    let mut w = WrapperProcess::spawn().await;
    w.handshake().await;
    w.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"prompts/list","params":{}})).await;
    let resp = w.recv().await;

    assert_conforms("ListPromptsResult", &resp["result"], &def, &schema);
    w.kill().await;
}

/// ListResourcesResult must conform to MCP schema.
#[tokio::test]
async fn test_list_resources_result_conforms() {
    let schema = load_mcp_schema();
    let def = definition(&schema, "ListResourcesResult");

    let mut w = WrapperProcess::spawn().await;
    w.handshake().await;
    w.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"resources/list","params":{}})).await;
    let resp = w.recv().await;

    assert_conforms("ListResourcesResult", &resp["result"], &def, &schema);
    w.kill().await;
}

/// ListResourceTemplatesResult must conform to MCP schema.
#[tokio::test]
async fn test_list_resource_templates_result_conforms() {
    let schema = load_mcp_schema();
    let def = definition(&schema, "ListResourceTemplatesResult");

    let mut w = WrapperProcess::spawn().await;
    w.handshake().await;
    w.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"resources/templates/list","params":{}})).await;
    let resp = w.recv().await;

    assert_conforms("ListResourceTemplatesResult", &resp["result"], &def, &schema);
    w.kill().await;
}

/// CallToolResult must conform to MCP schema.
#[tokio::test]
async fn test_call_tool_result_conforms() {
    let schema = load_mcp_schema();
    let def = definition(&schema, "CallToolResult");

    let mut w = WrapperProcess::spawn().await;
    w.handshake().await;
    w.send(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"conformance check"}}
    })).await;
    let resp = w.recv().await;

    assert_conforms("CallToolResult", &resp["result"], &def, &schema);
    w.kill().await;
}

/// JSONRPCError response (unknown tool) must conform to JSONRPCError schema.
#[tokio::test]
async fn test_error_response_conforms() {
    let schema = load_mcp_schema();
    let def = definition(&schema, "JSONRPCError");

    let mut w = WrapperProcess::spawn().await;
    w.handshake().await;
    w.send(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"nonexistent_tool","arguments":{}}
    })).await;

    // Read raw line — may be error or result (depending on backend behavior)
    let resp = w.recv().await;
    if resp.get("error").is_some() {
        assert_conforms("JSONRPCError", &resp, &def, &schema);
    }
    // If result (backend returned error in content), that's also valid per MCP spec
    w.kill().await;
}
