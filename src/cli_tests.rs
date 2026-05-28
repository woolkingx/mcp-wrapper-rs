use std::time::Duration;

use crate::cli::{AdminAction, CliMode, parse};

fn args(items: &[&str]) -> Vec<String> {
    let mut out = vec!["mcp-wrapper-rs".to_string()];
    out.extend(items.iter().map(|s| s.to_string()));
    out
}

#[test]
fn parses_proxy_defaults() {
    let mode = parse(args(&["python3", "server.py"])).unwrap();
    match mode {
        CliMode::Proxy(proxy) => {
            assert!(!proxy.daemon);
            assert_eq!(proxy.init_timeout, Duration::from_secs(30));
            assert_eq!(proxy.cmd, "python3");
            assert_eq!(proxy.args, vec!["server.py"]);
        }
        other => panic!("unexpected mode: {other:?}"),
    }
}

#[test]
fn parses_proxy_flags_before_command() {
    let mode = parse(args(&[
        "--init-timeout",
        "10",
        "--daemon",
        "python3",
        "server.py",
    ]))
    .unwrap();
    match mode {
        CliMode::Proxy(proxy) => {
            assert!(proxy.daemon);
            assert_eq!(proxy.init_timeout, Duration::from_secs(10));
            assert_eq!(proxy.cmd, "python3");
            assert_eq!(proxy.args, vec!["server.py"]);
        }
        other => panic!("unexpected mode: {other:?}"),
    }
}

#[test]
fn parses_backend_restart_admin_command() {
    let mode = parse(args(&[
        "backend",
        "restart",
        "--start",
        "--force",
        "--json",
        "--",
        "python3",
        "server.py",
    ]))
    .unwrap();
    match mode {
        CliMode::Admin(admin) => {
            assert_eq!(admin.action, AdminAction::BackendRestart);
            assert!(admin.start);
            assert!(admin.force);
            assert!(admin.output_json);
            let target = admin.target.unwrap();
            assert_eq!(target.cmd, "python3");
            assert_eq!(target.args, vec!["server.py"]);
        }
        other => panic!("unexpected mode: {other:?}"),
    }
}

#[test]
fn parses_broker_list_without_target() {
    let mode = parse(args(&["broker", "list", "--json"])).unwrap();
    match mode {
        CliMode::Admin(admin) => {
            assert_eq!(admin.action, AdminAction::BrokerList);
            assert!(admin.output_json);
            assert!(admin.target.is_none());
        }
        other => panic!("unexpected mode: {other:?}"),
    }
}

#[test]
fn rejects_targetless_backend_status() {
    let err = parse(args(&["backend", "status", "--json"])).unwrap_err();
    assert!(err.message.contains("Missing --"));
}

#[test]
fn rejects_unknown_admin_command() {
    let err = parse(args(&["backend", "bounce", "--", "python3"])).unwrap_err();
    assert!(err.message.contains("Unknown backend command"));
}

#[test]
fn parses_hidden_broker_internal() {
    let mode = parse(args(&[
        "--broker-internal",
        "--init-timeout",
        "3",
        "python3",
    ]))
    .unwrap();
    match mode {
        CliMode::BrokerInternal(rest) => {
            assert_eq!(rest, vec!["--init-timeout", "3", "python3"]);
        }
        other => panic!("unexpected mode: {other:?}"),
    }
}
