//! Logging owner: command identity, log path, and tracing initialization.

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;

/// Resolve log directory: $XDG_RUNTIME_DIR/mcp-wrapper > $TMPDIR > /tmp
pub fn log_dir() -> String {
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR") {
        let dir = format!("{}/mcp-wrapper", xdg);
        if std::fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    if let Ok(tmp) = env::var("TMPDIR") {
        return tmp;
    }
    "/tmp".to_string()
}

/// Compute 8-char hex hash from cmd + args for unique log file naming.
pub fn cmd_hash(cmd: &str, args: &[String]) -> String {
    let mut h = DefaultHasher::new();
    cmd.hash(&mut h);
    args.hash(&mut h);
    format!("{:016x}", h.finish())[..8].to_string()
}

/// Initialize tracing to file if MCP_WRAPPER_DEBUG is set.
/// Returns WorkerGuard that must be kept alive for the duration of the program.
pub fn init_tracing(cmd: &str, args: &[String]) -> Option<WorkerGuard> {
    let level_str = env::var("MCP_WRAPPER_DEBUG").ok()?;

    let level = match level_str.to_lowercase().as_str() {
        "1" | "info" => "info",
        "2" | "warn" => "warn",
        "3" | "debug" => "debug",
        _ => "info",
    };

    let file_name = if let Ok(name) = env::var("MCP_SERVER_NAME") {
        format!("mcp-wrapper-{}.log", sanitize_name(&name))
    } else {
        let name = infer_mcp_name(cmd, args);
        let hash = cmd_hash(cmd, args);
        format!("mcp-wrapper-{}-{}.log", name, hash)
    };

    let appender = tracing_appender::rolling::never(log_dir(), &file_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(format!("mcp_wrapper_rs={}", level))
        .with_target(false)
        .with_thread_ids(false)
        .init();

    Some(guard)
}

pub fn infer_mcp_name(cmd: &str, args: &[String]) -> String {
    if let Ok(name) = env::var("MCP_SERVER_NAME") {
        return sanitize_name(&name);
    }
    if cmd == "npx" && !args.is_empty() {
        for arg in args {
            if !arg.starts_with('-') {
                let name = arg.split('/').last().unwrap_or(arg);
                let name = name.split('@').next().unwrap_or(name);
                return sanitize_name(name);
            }
        }
    }
    if (cmd == "python3" || cmd == "python") && !args.is_empty() {
        if let Some(script) = args.first() {
            if let Some(name) = Path::new(script).file_stem() {
                return sanitize_name(name.to_string_lossy().as_ref());
            }
        }
    }
    if cmd.ends_with(".sh") {
        if let Some(name) = Path::new(cmd).file_stem() {
            return sanitize_name(name.to_string_lossy().as_ref());
        }
    }
    sanitize_name(cmd)
}

pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
