use serde_json::{json, Value};
use tokio::io::BufReader;

use crate::cli::{AdminAction, AdminArgs, CommandTarget};
use crate::tools::wrapper::WRAPPER_TOOL_NAME;
use crate::{daemon_manager, logging, mcp_interface};

pub async fn run(args: AdminArgs) -> i32 {
    let action = args.action.name();
    let response = match args.action {
        AdminAction::BrokerList => broker_list(action),
        AdminAction::BrokerStatus => broker_status(action, target_ref(&args)),
        AdminAction::BrokerStop => broker_stop(action, target_ref(&args)).await,
        AdminAction::BrokerRestart => {
            broker_restart(action, target_ref(&args), args.init_timeout).await
        }
        AdminAction::Status => {
            status(action, target_ref(&args), args.start, args.init_timeout).await
        }
        AdminAction::Doctor => doctor(action, target_ref(&args)).await,
        AdminAction::BackendStatus
        | AdminAction::BackendPing
        | AdminAction::BackendRefresh
        | AdminAction::BackendRestart
        | AdminAction::BackendStop => backend_control(&args).await,
    };

    if args.output_json {
        println!("{}", serde_json::to_string(&response).unwrap_or_default());
    } else {
        print_text(&response);
    }

    if response
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        0
    } else {
        1
    }
}

fn target_ref(args: &AdminArgs) -> &CommandTarget {
    args.target
        .as_ref()
        .expect("parser guarantees target for this action")
}

fn broker_list(action: &str) -> Value {
    match daemon_manager::list_brokers() {
        Ok(brokers) => envelope(
            true,
            action,
            None,
            None,
            Some(
                json!({ "brokers": brokers.into_iter().map(|b| broker_value(&b)).collect::<Vec<_>>() }),
            ),
            None,
        ),
        Err(e) => envelope(
            false,
            action,
            None,
            None,
            None,
            Some(error("broker.list", &e)),
        ),
    }
}

fn broker_status(action: &str, target: &CommandTarget) -> Value {
    let paths = daemon_manager::daemon_paths(&target.cmd, &target.args);
    let status = daemon_manager::broker_status(&paths);
    envelope(
        true,
        action,
        Some(target),
        Some(broker_value(&status)),
        Some(Value::Null),
        None,
    )
}

async fn broker_stop(action: &str, target: &CommandTarget) -> Value {
    let paths = daemon_manager::daemon_paths(&target.cmd, &target.args);
    match daemon_manager::stop_broker(&paths).await {
        Ok(status) => envelope(
            true,
            action,
            Some(target),
            Some(broker_value(&status)),
            Some(Value::Null),
            None,
        ),
        Err(e) => envelope(
            false,
            action,
            Some(target),
            Some(broker_value(&daemon_manager::broker_status(&paths))),
            None,
            Some(error("broker.stop", &e)),
        ),
    }
}

async fn broker_restart(
    action: &str,
    target: &CommandTarget,
    init_timeout: std::time::Duration,
) -> Value {
    let paths = daemon_manager::daemon_paths(&target.cmd, &target.args);
    match daemon_manager::restart_broker(&paths, &target.cmd, &target.args, init_timeout).await {
        Ok(_) => {
            let status = daemon_manager::broker_status(&paths);
            envelope(
                true,
                action,
                Some(target),
                Some(broker_value(&status)),
                Some(Value::Null),
                None,
            )
        }
        Err(e) => envelope(
            false,
            action,
            Some(target),
            Some(broker_value(&daemon_manager::broker_status(&paths))),
            None,
            Some(error("broker.restart", &e)),
        ),
    }
}

async fn status(
    action: &str,
    target: &CommandTarget,
    start: bool,
    init_timeout: std::time::Duration,
) -> Value {
    let paths = daemon_manager::daemon_paths(&target.cmd, &target.args);
    let stream = if start {
        daemon_manager::connect_or_start_broker(&paths, &target.cmd, &target.args, init_timeout)
            .await
    } else {
        daemon_manager::connect_existing_broker(&paths).await
    };
    let broker = daemon_manager::broker_status(&paths);

    let result = match stream {
        Ok(stream) => match call_wrapper_tool(stream, "backend.status", None).await {
            Ok(envelope) => envelope.get("result").cloned().unwrap_or(Value::Null),
            Err(e) => json!({ "backendError": e }),
        },
        Err(_) => Value::Null,
    };

    envelope(
        true,
        action,
        Some(target),
        Some(broker_value(&broker)),
        Some(result),
        None,
    )
}

async fn doctor(action: &str, target: &CommandTarget) -> Value {
    let paths = daemon_manager::daemon_paths(&target.cmd, &target.args);
    let broker = daemon_manager::broker_status(&paths);
    let backend_ping = if broker.running {
        match daemon_manager::connect_existing_broker(&paths).await {
            Ok(stream) => call_wrapper_tool(stream, "backend.ping", None)
                .await
                .ok()
                .and_then(|envelope| envelope.get("result").cloned()),
            Err(_) => None,
        }
    } else {
        None
    };

    envelope(
        true,
        action,
        Some(target),
        Some(broker_value(&broker)),
        Some(json!({
            "targetHash": paths.hash,
            "backendPing": backend_ping,
        })),
        None,
    )
}

async fn backend_control(args: &AdminArgs) -> Value {
    let target = target_ref(&args);
    let paths = daemon_manager::daemon_paths(&target.cmd, &target.args);
    let action = args.action.name();
    let stream = if args.start {
        daemon_manager::connect_or_start_broker(
            &paths,
            &target.cmd,
            &target.args,
            args.init_timeout,
        )
        .await
    } else {
        daemon_manager::connect_existing_broker(&paths).await
    };

    let stream = match stream {
        Ok(stream) => stream,
        Err(e) => {
            return envelope(
                false,
                action,
                Some(target),
                Some(broker_value(&daemon_manager::broker_status(&paths))),
                None,
                Some(error("broker.notRunning", &e)),
            );
        }
    };

    let action_name = match args.action {
        AdminAction::BackendStatus
        | AdminAction::BackendPing
        | AdminAction::BackendRefresh
        | AdminAction::BackendRestart
        | AdminAction::BackendStop => args.action.name(),
        _ => unreachable!("backend_control called for non-backend action"),
    };
    let params = if args.force {
        Some(json!({ "force": true }))
    } else {
        None
    };

    match call_wrapper_tool(stream, action_name, params).await {
        Ok(wrapper) => {
            let broker = daemon_manager::broker_status(&paths);
            if wrapper.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                envelope(
                    true,
                    action,
                    Some(target),
                    Some(broker_value(&broker)),
                    wrapper.get("result").cloned(),
                    None,
                )
            } else {
                envelope(
                    false,
                    action,
                    Some(target),
                    Some(broker_value(&broker)),
                    None,
                    wrapper.get("error").cloned(),
                )
            }
        }
        Err(e) => envelope(
            false,
            action,
            Some(target),
            Some(broker_value(&daemon_manager::broker_status(&paths))),
            None,
            Some(error("backend.control", &e)),
        ),
    }
}

async fn call_wrapper_tool(
    stream: tokio::net::UnixStream,
    action: &str,
    params: Option<Value>,
) -> Result<Value, String> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut arguments = json!({
        "action": action,
        "target": "default"
    });
    if let Some(params) = params {
        arguments["params"] = params;
    }
    let req = mcp_interface::build_request(
        json!(1),
        "tools/call",
        Some(json!({
            "name": WRAPPER_TOOL_NAME,
            "arguments": arguments
        })),
    );
    mcp_interface::write_message(&mut writer, &req)
        .await
        .map_err(|e| format!("write broker request: {}", e))?;
    let resp = mcp_interface::read_message(&mut reader)
        .await
        .ok_or_else(|| "broker closed without response".to_string())?;
    if let Some(err) = resp.get("error") {
        return Err(err.to_string());
    }
    resp.get("result")
        .and_then(|v| v.get("_meta"))
        .and_then(|v| v.get("mcpWrapper"))
        .cloned()
        .ok_or_else(|| "broker response missing mcpWrapper envelope".to_string())
}

fn envelope(
    ok: bool,
    action: &str,
    target: Option<&CommandTarget>,
    broker: Option<Value>,
    result: Option<Value>,
    err: Option<Value>,
) -> Value {
    let mut body = json!({
        "ok": ok,
        "action": action,
    });
    if let Some(target) = target {
        body["target"] = target_value(target);
    }
    if let Some(broker) = broker {
        body["broker"] = broker;
    }
    if let Some(result) = result {
        body["result"] = result;
    }
    if let Some(err) = err {
        body["error"] = err;
    }
    body
}

fn target_value(target: &CommandTarget) -> Value {
    json!({
        "cmd": target.cmd,
        "args": target.args,
        "hash": logging::cmd_hash(&target.cmd, &target.args),
    })
}

fn broker_value(status: &daemon_manager::BrokerStatus) -> Value {
    json!({
        "hash": status.hash,
        "running": status.running,
        "pid": status.pid,
        "socketExists": status.socket_exists,
        "lockExists": status.lock_exists,
        "socket": status.socket.to_string_lossy(),
        "lock": status.lock.to_string_lossy(),
        "pidPath": status.pid_path.to_string_lossy(),
        "metaPath": status.meta_path.to_string_lossy(),
        "meta": status.meta,
        "metaMatchesCurrentExe": status.meta_matches_current_exe,
    })
}

fn error(code: &str, message: &str) -> Value {
    json!({
        "code": code,
        "message": message,
    })
}

fn print_text(response: &Value) {
    let ok = response
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let action = response
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("admin");
    if ok {
        println!("{action}: ok");
        if let Some(running) = response
            .get("broker")
            .and_then(|v| v.get("running"))
            .and_then(|v| v.as_bool())
        {
            println!("broker.running: {running}");
        }
    } else {
        println!("{action}: failed");
        if let Some(message) = response
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str())
        {
            println!("error: {message}");
        }
    }
}
