//! Schema-driven tests derived from MCP JSON Schema + companion test spec.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

// ── schema2object ─────────────────────────────────────────────────────────────
#[path = "support/mod.rs"]
mod support;
use support::ObjectTree;

// ── helpers ───────────────────────────────────────────────────────────────────

fn wrapper_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    // tests run from target/debug/deps/, binary is at target/debug/
    p.pop();
    p.pop();
    p.push("mcp-wrapper-rs");
    p
}

fn echo_server_script() -> String {
    r#"
import sys, json, time, os

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

TOOLS = [
    {"name":"echo","description":"Echo input","inputSchema":{"type":"object","properties":{"msg":{"type":"string"}}}},
    {"name":"slow","description":"Sleep for a bit","inputSchema":{"type":"object","properties":{}}},
    {"name":"die","description":"Exit the process","inputSchema":{"type":"object","properties":{}}},
    {"name":"backend_error","description":"Return JSON-RPC error","inputSchema":{"type":"object","properties":{}}},
    {"name":"pid","description":"Return PID","inputSchema":{"type":"object","properties":{}}}
]

PROMPT = {"name":"hello","description":"Hello prompt"}
RESOURCE = {"name":"readme","uri":"file:///tmp/readme.txt"}
RESOURCE_TEMPLATE = {"name":"repo","uriTemplate":"file:///tmp/{repo}"}

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
            "capabilities":{
                "tools":{},
                "prompts":{},
                "resources":{},
                "completions":{}
            },
            "serverInfo":{"name":"schema-echo","version":"0.1.0"}
        }})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":TOOLS}})
    elif method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name", "")
        args = params.get("arguments", {}) or {}
        if name == "echo":
            text = args.get("msg", "")
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":text}]}})
        elif name == "slow":
            time.sleep(70)
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"slow"}]}})
        elif name == "die":
            sys.stdout.flush()
            sys.exit(0)
        elif name == "backend_error":
            send({"jsonrpc":"2.0","id":mid,"error":{"code":-32001,"message":"backend error"}})
        elif name == "pid":
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":str(os.getpid())}]}})
        else:
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"unknown"}]}})
    elif method == "prompts/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"prompts":[PROMPT]}})
    elif method == "prompts/get":
        send({"jsonrpc":"2.0","id":mid,"result":{
            "description":"hello prompt",
            "messages":[{"role":"assistant","content":{"type":"text","text":"hello"}}]
        }})
    elif method == "resources/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"resources":[RESOURCE]}})
    elif method == "resources/templates/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"resourceTemplates":[RESOURCE_TEMPLATE]}})
    elif method == "resources/read":
        uri = msg.get("params", {}).get("uri", "file:///tmp/readme.txt")
        send({"jsonrpc":"2.0","id":mid,"result":{"contents":[{"uri":uri,"text":"hello"}]}})
    elif method == "resources/subscribe":
        send({"jsonrpc":"2.0","id":mid,"result":{}})
    elif method == "resources/unsubscribe":
        send({"jsonrpc":"2.0","id":mid,"result":{}})
    elif method == "completion/complete":
        send({"jsonrpc":"2.0","id":mid,"result":{"completion":{"values":["hello","help"]}}})
    elif method == "logging/setLevel":
        send({"jsonrpc":"2.0","id":mid,"result":{}})
    elif method == "ping":
        send({"jsonrpc":"2.0","id":mid,"result":{}})
    elif method == "notifications/initialized":
        pass
    else:
        send({"jsonrpc":"2.0","id":mid,"result":{}})
"#.to_string()
}

fn load_json(path: &PathBuf) -> serde_json::Value {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("invalid json {}: {e}", path.display()))
}

fn load_mcp_schema() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp-schema.json");
    load_json(&path)
}

fn load_proxy_tests() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp-proxy-tests.json");
    load_json(&path)
}

fn definition(root: &serde_json::Value, name: &str) -> serde_json::Value {
    root["definitions"][name].clone()
}

fn assert_conforms(label: &str, data: &serde_json::Value, schema_def: &serde_json::Value, root: &serde_json::Value) {
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

fn client_request_methods(schema: &serde_json::Value) -> Vec<String> {
    let any_of = schema["definitions"]["ClientRequest"]["anyOf"]
        .as_array()
        .expect("ClientRequest.anyOf array");
    let mut out = Vec::new();
    for item in any_of {
        let r = item["$ref"].as_str().expect("$ref string");
        let name = r.rsplit('/').next().unwrap();
        let def = &schema["definitions"][name];
        let method = def["properties"]["method"]["const"].as_str().unwrap_or("");
        if method.is_empty() {
            continue;
        }
        out.push(method.to_string());
    }
    out
}

fn proxy_x_tests(spec: &serde_json::Value) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let map = spec["x-tests"].as_object().expect("x-tests object");
    for (k, v) in map {
        let tags = v.as_array().expect("x-tests array");
        let mut list = Vec::new();
        for tag in tags {
            list.push(tag.as_str().expect("x-tests string").to_string());
        }
        out.insert(k.clone(), list);
    }
    out
}

fn result_definitions(schema: &serde_json::Value) -> Vec<String> {
    schema["definitions"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.ends_with("Result"))
        .cloned()
        .collect()
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}", nanos)
}

struct WrapperProcess {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::UnboundedReceiver<serde_json::Value>,
    script_path: String,
}

impl WrapperProcess {
    async fn spawn_with_script(script: &str, extra_args: &[&str]) -> Self {
        let script_path = format!("/tmp/mcp_schema_server_{}.py", unique_suffix());
        tokio::fs::write(&script_path, script).await.unwrap();

        let mut cmd = Command::new(wrapper_bin());
        cmd.args(extra_args);
        cmd.arg("python3").arg(&script_path);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        cmd.env_remove("MCP_WRAPPER_DEBUG");

        let mut child = cmd.spawn().expect("failed to spawn wrapper");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = match reader.read_line(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                let line = buf.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    let _ = tx.send(v);
                }
            }
        });

        WrapperProcess { child, stdin, rx, script_path }
    }

    async fn send(&mut self, msg: serde_json::Value) {
        let line = serde_json::to_string(&msg).unwrap() + "\n";
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    async fn recv_any(&mut self) -> serde_json::Value {
        self.rx.recv().await.expect("wrapper stdout closed")
    }

    async fn recv_response(&mut self) -> serde_json::Value {
        loop {
            let v = self.recv_any().await;
            if v.get("id").is_some() && v.get("method").is_none() {
                return v;
            }
        }
    }

    async fn recv_n_responses(&mut self, n: usize) -> Vec<serde_json::Value> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let v = self.recv_response().await;
            out.push(v);
        }
        out
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
        let resp = self.recv_response().await;
        self.send(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        resp
    }

    async fn send_recv(&mut self, msg: serde_json::Value) -> serde_json::Value {
        self.send(msg).await;
        self.recv_response().await
    }

    async fn kill(mut self) {
        let _ = self.child.kill().await;
    }
}

// ── Tests: A. Method coverage ────────────────────────────────────────────────

#[tokio::test]
async fn test_method_coverage_from_schema() {
    let schema = load_mcp_schema();
    let companion = load_proxy_tests();

    let methods = client_request_methods(&schema);
    let x_tests = proxy_x_tests(&companion);

    let mut missing = Vec::new();
    for method in methods {
        match x_tests.get(&method) {
            Some(tags) if !tags.is_empty() => {}
            _ => missing.push(method),
        }
    }

    assert!(missing.is_empty(), "missing x-tests coverage for methods: {missing:?}");
}

// ── Tests: B. Boundary tests from x-tests tags ───────────────────────────────

#[tokio::test]
async fn test_proxy_scenarios_from_x_tests() {
    let companion = load_proxy_tests();
    let x_tests = proxy_x_tests(&companion);

    let targets = ["ensure_backend", "idle_reaper", "call_tool", "capabilities_filter"];
    for method in targets {
        let tags = x_tests.get(method).expect("missing x-tests entry");
        for tag in tags {
            run_proxy_scenario(method, tag).await;
        }
    }
}

async fn run_proxy_scenario(method: &str, tag: &str) {
    match (method, tag) {
        ("ensure_backend", "valid_normal") => scenario_call_tool_echo().await,
        ("ensure_backend", "custom:spawn_failure") => scenario_spawn_failure().await,
        ("ensure_backend", "custom:concurrent_callers") => scenario_concurrent_calls("echo", 5).await,
        ("ensure_backend", "custom:dead_backend_respawn") => scenario_dead_backend_respawn().await,

        ("idle_reaper", "custom:reaper_persists_after_kill") => scenario_reaper_persists().await,
        ("idle_reaper", "custom:active_call_defers_kill") => scenario_active_call_defers_kill().await,

        ("call_tool", "valid_normal") => scenario_call_tool_echo().await,
        ("call_tool", "missing") => scenario_call_tool_missing_name().await,
        ("call_tool", "empty") => scenario_call_tool_empty_args().await,
        ("call_tool", "custom:backend_error_code_preserved") => scenario_backend_error_code_preserved().await,
        ("call_tool", "custom:concurrent_calls") => scenario_concurrent_calls("echo", 5).await,

        ("capabilities_filter", "valid_normal") => scenario_initialize_ok().await,
        ("capabilities_filter", "custom:tools_only_advertised") => scenario_tools_only_advertised().await,

        _ => panic!("unhandled scenario: {method} {tag}"),
    }
}

async fn scenario_initialize_ok() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    let resp = w.handshake().await;
    assert!(resp.get("result").is_some());
    w.kill().await;
}

async fn scenario_call_tool_echo() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;
    let resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"hello"}}
    })).await;
    assert_eq!(resp["result"]["content"][0]["text"], "hello");
    w.kill().await;
}

async fn scenario_call_tool_missing_name() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;
    let resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{}
    })).await;
    assert!(resp.get("error").is_some(), "expected error for missing name");
    w.kill().await;
}

async fn scenario_call_tool_empty_args() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;
    let resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"echo","arguments":{}}
    })).await;
    assert_eq!(resp["result"]["content"][0]["text"], "");
    w.kill().await;
}

async fn scenario_backend_error_code_preserved() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;
    let resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"backend_error","arguments":{}}
    })).await;
    assert_eq!(resp["error"]["code"], -32001);
    w.kill().await;
}

async fn scenario_concurrent_calls(tool_name: &str, count: usize) {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;

    for i in 0..count {
        w.send(serde_json::json!({
            "jsonrpc":"2.0","id":(10 + i as i64),"method":"tools/call",
            "params":{"name":tool_name,"arguments":{"msg":format!("hi-{i}")}}
        })).await;
    }

    let responses = w.recv_n_responses(count).await;
    let mut texts = HashSet::new();
    for resp in responses {
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        texts.insert(text.to_string());
    }
    assert_eq!(texts.len(), count);
    w.kill().await;
}

async fn scenario_dead_backend_respawn() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;

    let _ = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"alive"}}
    })).await;

    tokio::time::sleep(Duration::from_secs(125)).await;
    let resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"alive"}}
    })).await;
    assert_eq!(resp["result"]["content"][0]["text"], "alive");
    w.kill().await;
}

async fn scenario_spawn_failure() {
    let script = echo_server_script();
    let mut w = WrapperProcess::spawn_with_script(&script, &[]).await;
    w.handshake().await;

    let _ = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"pid","arguments":{}}
    })).await;
    let _ = tokio::fs::remove_file(&w.script_path).await;
    tokio::time::sleep(Duration::from_secs(125)).await;

    let resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"hi"}}
    })).await;
    assert!(resp.get("error").is_some(), "expected error on respawn failure");
    w.kill().await;
}

async fn scenario_reaper_persists() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;

    let pid1 = fetch_pid(&mut w, 2).await;
    tokio::time::sleep(Duration::from_secs(125)).await;
    let pid2 = fetch_pid(&mut w, 3).await;
    assert_ne!(pid1, pid2, "backend should respawn after idle kill");

    tokio::time::sleep(Duration::from_secs(125)).await;
    let pid3 = fetch_pid(&mut w, 4).await;
    assert_ne!(pid2, pid3, "idle reaper should persist after respawn");

    w.kill().await;
}

async fn fetch_pid(w: &mut WrapperProcess, id: i64) -> String {
    let resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":id,"method":"tools/call",
        "params":{"name":"pid","arguments":{}}
    })).await;
    resp["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()
}

async fn scenario_active_call_defers_kill() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    w.handshake().await;

    let resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"slow","arguments":{}}
    })).await;
    assert_eq!(resp["result"]["content"][0]["text"], "slow");
    w.kill().await;
}

async fn scenario_tools_only_advertised() {
    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    let resp = w.handshake().await;
    let caps = resp["result"]["capabilities"].as_object().expect("capabilities object");
    assert!(caps.contains_key("tools"));
    assert!(caps.get("prompts").is_none() || caps["prompts"].is_null());
    assert!(caps.get("resources").is_none() || caps["resources"].is_null());
    assert!(caps.get("completions").is_none() || caps["completions"].is_null());
    w.kill().await;
}

// ── Tests: C. Conformance for all *Result definitions ────────────────────────

#[tokio::test]
async fn test_all_result_definitions_conform() {
    let schema = load_mcp_schema();
    let result_defs = result_definitions(&schema);

    let mut w = WrapperProcess::spawn_with_script(&echo_server_script(), &[]).await;
    let init_resp = w.handshake().await;

    let mut results: HashMap<String, serde_json::Value> = HashMap::new();
    results.insert("InitializeResult".to_string(), init_resp["result"].clone());

    let tools_resp = w.send_recv(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})).await;
    results.insert("ListToolsResult".to_string(), tools_resp["result"].clone());

    let call_resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"echo","arguments":{"msg":"hi"}}
    })).await;
    results.insert("CallToolResult".to_string(), call_resp["result"].clone());

    let prompts_resp = w.send_recv(serde_json::json!({"jsonrpc":"2.0","id":4,"method":"prompts/list","params":{}})).await;
    results.insert("ListPromptsResult".to_string(), prompts_resp["result"].clone());

    let get_prompt_resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":5,"method":"prompts/get",
        "params":{"name":"hello","arguments":{}}
    })).await;
    results.insert("GetPromptResult".to_string(), get_prompt_resp["result"].clone());

    let resources_resp = w.send_recv(serde_json::json!({"jsonrpc":"2.0","id":6,"method":"resources/list","params":{}})).await;
    results.insert("ListResourcesResult".to_string(), resources_resp["result"].clone());

    let templates_resp = w.send_recv(serde_json::json!({"jsonrpc":"2.0","id":7,"method":"resources/templates/list","params":{}})).await;
    results.insert("ListResourceTemplatesResult".to_string(), templates_resp["result"].clone());

    let read_resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":8,"method":"resources/read",
        "params":{"uri":"file:///tmp/readme.txt"}
    })).await;
    results.insert("ReadResourceResult".to_string(), read_resp["result"].clone());

    let complete_resp = w.send_recv(serde_json::json!({
        "jsonrpc":"2.0","id":9,"method":"completion/complete",
        "params":{
            "argument":{"name":"q","value":"he"},
            "ref":{"type":"ref/prompt","name":"hello"}
        }
    })).await;
    results.insert("CompleteResult".to_string(), complete_resp["result"].clone());

    results.insert("Result".to_string(), init_resp["result"].clone());
    results.insert("EmptyResult".to_string(), init_resp["result"].clone());
    results.insert("PaginatedResult".to_string(), resources_resp["result"].clone());
    results.insert("ServerResult".to_string(), init_resp["result"].clone());

    // Client-side results (wrapper does not emit these directly).
    let create_result = serde_json::json!({
        "content":{"type":"text","text":"sampled"},
        "model":"test-model",
        "role":"assistant",
        "stopReason":"end"
    });
    results.insert("CreateMessageResult".to_string(), create_result);

    let roots_result = serde_json::json!({
        "roots":[{"uri":"file:///tmp"}]
    });
    results.insert("ListRootsResult".to_string(), roots_result);

    results.insert("ClientResult".to_string(), results["CreateMessageResult"].clone());

    for def_name in result_defs {
        let schema_def = definition(&schema, &def_name);
        let data = results.get(&def_name).unwrap_or_else(|| panic!("missing result for {def_name}"));
        assert_conforms(&def_name, data, &schema_def, &schema);
    }

    w.kill().await;
}
