//! MCP Wrapper - Universal lightweight proxy (raw JSON-RPC, no rmcp)
//!
//! Usage: mcp-wrapper-rs <command> [args...]
//!        mcp-wrapper-rs --init-timeout <secs> <command> [args...]
//!
//! Design:
//! - On startup, spawns temporary backend to cache tools/prompts/resources/server_info
//! - list/init requests served instantly from cache
//! - tools/call and other pass-through requests spawn a persistent backend on demand
//! - Raw JSON-RPC over stdin/stdout — no SDK dependency for protocol handling

mod admin_cli;
mod backend_manager;
mod broker_manager;
mod cli;
#[cfg(test)]
mod cli_tests;
mod daemon_manager;
#[cfg(test)]
mod daemon_manager_tests;
mod logging;
mod mcp_interface;
#[cfg(test)]
mod mcp_interface_tests;
mod mcp_manager;
mod runtime_manager;

use cli::CliMode;
use std::env;

// ── CLI ─────────────────────────────────────────────────────────────

fn print_usage(program: &str) {
    eprintln!("mcp-wrapper-rs - Universal lightweight MCP proxy");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  {} [options] <command> [args...]", program);
    eprintln!(
        "  {} status [--json] [--start] -- <command> [args...]",
        program
    );
    eprintln!(
        "  {} backend <status|ping|refresh|restart|stop> [options] -- <command> [args...]",
        program
    );
    eprintln!(
        "  {} broker <list|status|stop|restart> [options] [-- <command> [args...]]",
        program
    );
    eprintln!("  {} doctor [--json] -- <command> [args...]", program);
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --version, -V              Show version and exit");
    eprintln!("  --help, -h                 Show this help and exit");
    eprintln!(
        "  --init-timeout <secs>      Seconds to wait for subprocess init handshake (default: 30)"
    );
    eprintln!(
        "  --daemon                   Share a single backend via a broker process (N:1 architecture)"
    );
    eprintln!("  --json                     Print admin command readback as JSON");
    eprintln!(
        "  --start                    Start broker for admin backend/status commands if absent"
    );
    eprintln!("  --force                    Force backend restart");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  {} python3 server.py", program);
    eprintln!("  {} npx -y @anthropics/mcp-searxng", program);
    eprintln!("  {} --init-timeout 10 codex mcp-server", program);
    eprintln!("  {} --daemon python3 server.py", program);
    eprintln!(
        "  {} backend restart --start --json -- python3 server.py",
        program
    );
    eprintln!("  {} broker status --json -- python3 server.py", program);
    eprintln!();
    eprintln!("Environment Variables:");
    eprintln!(
        "  MCP_WRAPPER_DEBUG=N    Enable logging: 1/info, 2/warn, 3/debug (to $XDG_RUNTIME_DIR/mcp-wrapper/ or /tmp)"
    );
    eprintln!("  MCP_SERVER_NAME=xxx    Override inferred server name for logs");
}

fn print_version() {
    println!("mcp-wrapper-rs {}", env!("CARGO_PKG_VERSION"));
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let program = args
        .first()
        .cloned()
        .unwrap_or_else(|| "mcp-wrapper-rs".to_string());

    let mode = match cli::parse(args) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("Error: {}", err.message);
            eprintln!();
            print_usage(&program);
            std::process::exit(1);
        }
    };

    let CliMode::Proxy(proxy) = mode else {
        match mode {
            CliMode::Version => print_version(),
            CliMode::Help => print_usage(&program),
            CliMode::BrokerInternal(args) => broker_manager::start_broker_process(args),
            CliMode::Admin(admin) => {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create tokio runtime");
                let code = rt.block_on(admin_cli::run(admin));
                std::process::exit(code);
            }
            CliMode::Proxy(_) => unreachable!(),
        }
        return;
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    if proxy.daemon {
        rt.block_on(async {
            let _tracing_guard = logging::init_tracing(&proxy.cmd, &proxy.args);
            let paths = daemon_manager::daemon_paths(&proxy.cmd, &proxy.args);
            match daemon_manager::connect_or_start_broker(
                &paths,
                &proxy.cmd,
                &proxy.args,
                proxy.init_timeout,
            )
            .await
            {
                Ok(stream) => {
                    if let Err(e) = daemon_manager::run_relay(stream).await {
                        tracing::warn!(err = %e, "relay error");
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        });
    } else {
        rt.block_on(runtime_manager::run_normal_command(
            proxy.init_timeout,
            proxy.cmd,
            proxy.args,
        ));
    }
}
