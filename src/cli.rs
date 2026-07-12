use std::time::Duration;

use crate::timeouts;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliMode {
    Version,
    Help,
    Proxy(ProxyArgs),
    BrokerInternal(Vec<String>),
    Admin(AdminArgs),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyArgs {
    pub daemon: bool,
    pub init_timeout: Duration,
    pub cmd: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminArgs {
    pub action: AdminAction,
    pub output_json: bool,
    pub start: bool,
    pub force: bool,
    pub init_timeout: Duration,
    pub target: Option<CommandTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminAction {
    Status,
    BackendStatus,
    BackendPing,
    BackendRefresh,
    BackendRestart,
    BackendStop,
    BrokerList,
    BrokerStatus,
    BrokerStop,
    BrokerRestart,
    Doctor,
}

impl AdminAction {
    pub fn name(self) -> &'static str {
        match self {
            AdminAction::Status => "status",
            AdminAction::BackendStatus => "backend.status",
            AdminAction::BackendPing => "backend.ping",
            AdminAction::BackendRefresh => "backend.refresh",
            AdminAction::BackendRestart => "backend.restart",
            AdminAction::BackendStop => "backend.stop",
            AdminAction::BrokerList => "broker.list",
            AdminAction::BrokerStatus => "broker.status",
            AdminAction::BrokerStop => "broker.stop",
            AdminAction::BrokerRestart => "broker.restart",
            AdminAction::Doctor => "doctor",
        }
    }

    fn requires_target(self) -> bool {
        !matches!(self, AdminAction::BrokerList)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTarget {
    pub cmd: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub fn parse(args: Vec<String>) -> Result<CliMode, CliError> {
    if args.len() < 2 {
        return Err(CliError::new("Missing <command> argument"));
    }

    match args[1].as_str() {
        "--version" | "-V" => return Ok(CliMode::Version),
        "--help" | "-h" => return Ok(CliMode::Help),
        "--broker-internal" => return Ok(CliMode::BrokerInternal(args[2..].to_vec())),
        "status" | "backend" | "broker" | "doctor" => return parse_admin(&args),
        _ => {}
    }

    parse_proxy(&args)
}

fn parse_proxy(args: &[String]) -> Result<CliMode, CliError> {
    let mut daemon = false;
    let mut init_timeout = timeouts::default_init_timeout();
    let mut index = 1;

    while index < args.len() {
        match args[index].as_str() {
            "--daemon" => {
                daemon = true;
                index += 1;
            }
            "--init-timeout" => {
                let secs = parse_secs(args.get(index + 1), "--init-timeout")?;
                init_timeout = Duration::from_secs(secs);
                index += 2;
            }
            flag if flag.starts_with('-') => {
                return Err(CliError::new(format!("Unknown option: {}", flag)));
            }
            _ => break,
        }
    }

    if index >= args.len() {
        return Err(CliError::new("Missing <command> argument"));
    }

    Ok(CliMode::Proxy(ProxyArgs {
        daemon,
        init_timeout,
        cmd: args[index].clone(),
        args: args[index + 1..].to_vec(),
    }))
}

fn parse_admin(args: &[String]) -> Result<CliMode, CliError> {
    let (action, mut index) = match args[1].as_str() {
        "status" => (AdminAction::Status, 2),
        "doctor" => (AdminAction::Doctor, 2),
        "backend" => {
            let sub = args
                .get(2)
                .ok_or_else(|| CliError::new("backend requires a subcommand"))?;
            let action = match sub.as_str() {
                "status" => AdminAction::BackendStatus,
                "ping" => AdminAction::BackendPing,
                "refresh" => AdminAction::BackendRefresh,
                "restart" => AdminAction::BackendRestart,
                "stop" => AdminAction::BackendStop,
                _ => return Err(CliError::new(format!("Unknown backend command: {}", sub))),
            };
            (action, 3)
        }
        "broker" => {
            let sub = args
                .get(2)
                .ok_or_else(|| CliError::new("broker requires a subcommand"))?;
            let action = match sub.as_str() {
                "list" => AdminAction::BrokerList,
                "status" => AdminAction::BrokerStatus,
                "stop" => AdminAction::BrokerStop,
                "restart" => AdminAction::BrokerRestart,
                _ => return Err(CliError::new(format!("Unknown broker command: {}", sub))),
            };
            (action, 3)
        }
        _ => unreachable!("parse_admin called for non-admin command"),
    };

    let mut output_json = false;
    let mut start = false;
    let mut force = false;
    let mut init_timeout = timeouts::default_init_timeout();
    let mut separator = None;

    while index < args.len() {
        match args[index].as_str() {
            "--" => {
                separator = Some(index);
                break;
            }
            "--json" => output_json = true,
            "--start" => start = true,
            "--force" => force = true,
            "--init-timeout" => {
                let secs = parse_secs(args.get(index + 1), "--init-timeout")?;
                init_timeout = Duration::from_secs(secs);
                index += 1;
            }
            flag if flag.starts_with('-') => {
                return Err(CliError::new(format!("Unknown option: {}", flag)));
            }
            other => {
                return Err(CliError::new(format!(
                    "Unexpected argument before --: {}",
                    other
                )));
            }
        }
        index += 1;
    }

    if action == AdminAction::BackendStop && start {
        return Err(CliError::new("backend stop does not accept --start"));
    }
    if force && action != AdminAction::BackendRestart {
        return Err(CliError::new("--force is only valid for backend restart"));
    }

    let target = match separator {
        Some(pos) => {
            let cmd = args
                .get(pos + 1)
                .ok_or_else(|| CliError::new("Missing <command> after --"))?;
            Some(CommandTarget {
                cmd: cmd.clone(),
                args: args[pos + 2..].to_vec(),
            })
        }
        None => None,
    };

    if action.requires_target() && target.is_none() {
        return Err(CliError::new("Missing -- <command> [args...]"));
    }
    if !action.requires_target() && target.is_some() {
        return Err(CliError::new(
            "broker list does not accept a command target",
        ));
    }

    Ok(CliMode::Admin(AdminArgs {
        action,
        output_json,
        start,
        force,
        init_timeout,
        target,
    }))
}

fn parse_secs(value: Option<&String>, flag: &str) -> Result<u64, CliError> {
    let raw = value.ok_or_else(|| CliError::new(format!("{} requires a value", flag)))?;
    let secs = raw
        .parse::<u64>()
        .map_err(|_| CliError::new(format!("{} requires a positive integer", flag)))?;
    if secs == 0 {
        return Err(CliError::new(format!("{} must be greater than 0", flag)));
    }
    Ok(secs)
}
