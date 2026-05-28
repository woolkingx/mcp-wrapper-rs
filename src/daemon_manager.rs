//! Daemon mode: coordination logic and stdio↔UDS relay.
//!
//! When `--daemon` is used, the wrapper connects to a shared broker process
//! over a Unix domain socket instead of spawning its own backend. If no broker
//! is running, one is spawned automatically with flock-based coordination.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::UNIX_EPOCH;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use crate::logging::cmd_hash;

/// Check if PID in file is still alive. Returns Some(pid) if alive, None if stale/missing.
fn read_alive_pid(pid_path: &std::path::Path) -> Option<u32> {
    let content = std::fs::read_to_string(pid_path).ok()?;
    let pid: u32 = content.trim().parse().ok()?;
    // kill(pid, 0) checks existence without sending a signal
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 { Some(pid) } else { None }
}

/// Paths used for daemon coordination.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub hash: String,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub pid: PathBuf,
    pub meta: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BrokerStatus {
    pub hash: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub socket_exists: bool,
    pub lock_exists: bool,
    pub meta: Option<serde_json::Value>,
    pub meta_matches_current_exe: bool,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub pid_path: PathBuf,
    pub meta_path: PathBuf,
}

/// Compute socket and lock paths for a given command + args.
///
/// Layout: `$XDG_RUNTIME_DIR/mcp-wrapper/{hash}.sock` and `.lock`.
/// Falls back to `/tmp/mcp-wrapper/` if XDG_RUNTIME_DIR is unset.
pub fn daemon_paths(cmd: &str, args: &[String]) -> DaemonPaths {
    let hash = cmd_hash(cmd, args);
    let base = daemon_base_dir();
    DaemonPaths {
        hash: hash.clone(),
        socket: base.join(format!("{}.sock", hash)),
        lock: base.join(format!("{}.lock", hash)),
        pid: base.join(format!("{}.pid", hash)),
        meta: base.join(format!("{}.meta.json", hash)),
    }
}

fn daemon_base_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("mcp-wrapper")
    } else {
        PathBuf::from("/tmp/mcp-wrapper")
    }
}

fn paths_for_hash(base: &Path, hash: &str) -> DaemonPaths {
    DaemonPaths {
        hash: hash.to_string(),
        socket: base.join(format!("{}.sock", hash)),
        lock: base.join(format!("{}.lock", hash)),
        pid: base.join(format!("{}.pid", hash)),
        meta: base.join(format!("{}.meta.json", hash)),
    }
}

fn current_broker_meta() -> Result<serde_json::Value, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {}", e))?;
    let metadata = std::fs::metadata(&exe).map_err(|e| format!("exe metadata: {}", e))?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    Ok(serde_json::json!({
        "wrapperVersion": env!("CARGO_PKG_VERSION"),
        "exe": exe.to_string_lossy(),
        "exeLen": metadata.len(),
        "exeModifiedMs": modified_ms,
    }))
}

pub fn write_broker_meta(paths: &DaemonPaths) -> Result<(), String> {
    let meta = current_broker_meta()?;
    let body = serde_json::to_string(&meta).map_err(|e| format!("serialize broker meta: {}", e))?;
    std::fs::write(&paths.meta, body).map_err(|e| format!("write broker meta: {}", e))
}

fn broker_meta_matches(paths: &DaemonPaths) -> bool {
    let Ok(current) = current_broker_meta() else {
        return false;
    };
    let Ok(body) = std::fs::read_to_string(&paths.meta) else {
        return false;
    };
    let Ok(existing) = serde_json::from_str::<serde_json::Value>(&body) else {
        return false;
    };
    existing == current
}

pub fn broker_status(paths: &DaemonPaths) -> BrokerStatus {
    let meta = std::fs::read_to_string(&paths.meta)
        .ok()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok());
    let pid = read_alive_pid(&paths.pid);
    let socket_exists = paths.socket.exists();
    BrokerStatus {
        hash: paths.hash.clone(),
        running: pid.is_some() && socket_exists,
        pid,
        socket_exists,
        lock_exists: paths.lock.exists(),
        meta,
        meta_matches_current_exe: broker_meta_matches(paths),
        socket: paths.socket.clone(),
        lock: paths.lock.clone(),
        pid_path: paths.pid.clone(),
        meta_path: paths.meta.clone(),
    }
}

pub fn list_brokers() -> Result<Vec<BrokerStatus>, String> {
    let base = daemon_base_dir();
    if !base.exists() {
        return Ok(Vec::new());
    }

    let mut hashes = BTreeSet::new();
    let entries = std::fs::read_dir(&base).map_err(|e| format!("read daemon dir: {}", e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read daemon entry: {}", e))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(hash) = name.strip_suffix(".sock") {
            hashes.insert(hash.to_string());
        } else if let Some(hash) = name.strip_suffix(".lock") {
            hashes.insert(hash.to_string());
        } else if let Some(hash) = name.strip_suffix(".pid") {
            hashes.insert(hash.to_string());
        } else if let Some(hash) = name.strip_suffix(".meta.json") {
            hashes.insert(hash.to_string());
        }
    }

    Ok(hashes
        .into_iter()
        .map(|hash| broker_status(&paths_for_hash(&base, &hash)))
        .collect())
}

pub async fn connect_existing_broker(paths: &DaemonPaths) -> Result<UnixStream, String> {
    let status = broker_status(paths);
    if !status.running {
        return Err("broker is not running; pass --start to start it".to_string());
    }
    if !status.meta_matches_current_exe {
        return Err("broker metadata mismatch; restart the broker".to_string());
    }
    UnixStream::connect(&paths.socket)
        .await
        .map_err(|e| format!("connect broker: {}", e))
}

pub async fn stop_broker(paths: &DaemonPaths) -> Result<BrokerStatus, String> {
    if let Some(pid) = read_alive_pid(&paths.pid) {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if read_alive_pid(&paths.pid).is_none() {
                break;
            }
        }
        if read_alive_pid(&paths.pid).is_some() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    let _ = std::fs::remove_file(&paths.pid);
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.meta);
    Ok(broker_status(paths))
}

pub async fn restart_broker(
    paths: &DaemonPaths,
    cmd: &str,
    args: &[String],
    init_timeout: Duration,
) -> Result<UnixStream, String> {
    let _ = stop_broker(paths).await?;
    connect_or_start_broker(paths, cmd, args, init_timeout).await
}

async fn remove_stale_broker(paths: &DaemonPaths, reason: &str) {
    if let Some(old_pid) = read_alive_pid(&paths.pid) {
        warn!(pid = old_pid, reason, "daemon: killing stale broker");
        unsafe {
            libc::kill(old_pid as libc::pid_t, libc::SIGTERM);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = std::fs::remove_file(&paths.pid);
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.meta);
}

/// Connect to an existing broker, or spawn one if none is running.
///
/// Uses flock for coordination: only one wrapper spawns the broker.
/// Others block on the lock then connect.
pub async fn connect_or_start_broker(
    paths: &DaemonPaths,
    cmd: &str,
    args: &[String],
    init_timeout: Duration,
) -> Result<UnixStream, String> {
    // Ensure directory exists
    std::fs::create_dir_all(paths.socket.parent().unwrap())
        .map_err(|e| format!("create daemon dir: {}", e))?;

    if paths.socket.exists() && !broker_meta_matches(paths) {
        remove_stale_broker(paths, "metadata mismatch").await;
    }

    // Attempt 1: try connecting directly
    if let Ok(stream) = UnixStream::connect(&paths.socket).await {
        info!("daemon: connected to existing broker");
        return Ok(stream);
    }

    // Open lock file
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&paths.lock)
        .map_err(|e| format!("open lock file: {}", e))?;

    // Try non-blocking lock to decide who spawns the broker
    let mut lock = fd_lock::RwLock::new(lock_file);
    let i_am_spawner = lock.try_write().is_ok();

    if i_am_spawner {
        // We hold the lock. Race check: try connecting again.
        if paths.socket.exists() && !broker_meta_matches(paths) {
            remove_stale_broker(paths, "metadata mismatch after lock").await;
        } else if let Ok(stream) = UnixStream::connect(&paths.socket).await {
            info!("daemon: broker appeared during lock acquisition");
            return Ok(stream);
        }

        // Check for stale PID file — if process is dead, clean up socket + pid
        if let Some(old_pid) = read_alive_pid(&paths.pid) {
            // Process alive but socket connect failed — stale socket or broker stuck
            warn!(
                pid = old_pid,
                "daemon: broker PID alive but socket unreachable"
            );
            remove_stale_broker(paths, "socket unreachable").await;
        }
        // Clean up stale files before spawning
        let _ = std::fs::remove_file(&paths.pid);
        let _ = std::fs::remove_file(&paths.socket);
        let _ = std::fs::remove_file(&paths.meta);

        // We are the spawner. Launch broker as a detached subprocess.
        info!("daemon: spawning broker");
        spawn_broker_process(cmd, args, init_timeout)?;

        // Wait for broker to become ready (socket appears and accepts)
        let deadline = tokio::time::Instant::now() + init_timeout + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if tokio::time::Instant::now() > deadline {
                return Err("broker did not become ready in time".to_string());
            }
            if let Ok(stream) = UnixStream::connect(&paths.socket).await {
                if !broker_meta_matches(paths) {
                    remove_stale_broker(paths, "metadata mismatch after spawn").await;
                    return Err("broker metadata mismatch after spawn".to_string());
                }
                info!("daemon: connected to new broker");
                return Ok(stream);
            }
        }
    } else {
        // Another process is spawning the broker. Wait for lock (blocking).
        debug!("daemon: waiting for another process to spawn broker");
        let _guard = lock
            .write()
            .map_err(|e| format!("flock blocking wait: {}", e))?;

        // Broker should be ready now. Connect with retries.
        for _ in 0..20 {
            if let Ok(stream) = UnixStream::connect(&paths.socket).await {
                if !broker_meta_matches(paths) {
                    remove_stale_broker(paths, "metadata mismatch after lock wait").await;
                    return Err("broker metadata mismatch after lock wait".to_string());
                }
                info!("daemon: connected after lock wait");
                return Ok(stream);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err("broker not reachable after lock wait".to_string())
    }
}

/// Spawn the broker as a detached child process via `--broker-internal`.
fn spawn_broker_process(cmd: &str, args: &[String], init_timeout: Duration) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {}", e))?;
    let mut broker_args = vec![
        "--broker-internal".to_string(),
        "--init-timeout".to_string(),
        init_timeout.as_secs().max(1).to_string(),
        "--no-idle-timeout".to_string(),
        cmd.to_string(),
    ];
    broker_args.extend(args.iter().cloned());
    let mut command = std::process::Command::new(exe);
    command.args(&broker_args);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());

    // Double-fork: spawn child, child immediately spawns grandchild and exits.
    // This detaches the broker from the wrapper's process tree.
    unsafe {
        command.pre_exec(|| {
            // New session so broker isn't killed when wrapper's terminal closes
            libc::setsid();
            Ok(())
        });
    }

    use std::os::unix::process::CommandExt;
    let child = command
        .spawn()
        .map_err(|e| format!("spawn broker: {}", e))?;
    // Don't wait — let the broker run independently.
    // The child handle is dropped, but the process continues because it's in a new session.
    std::mem::forget(child);

    Ok(())
}

/// Bidirectional relay: stdin↔UDS.
///
/// Reads JSON-RPC lines from stdin, forwards to broker over UDS.
/// Reads responses from broker UDS, writes to stdout.
/// Returns when either side closes.
pub async fn run_relay(stream: UnixStream) -> Result<(), String> {
    let (reader, writer) = stream.into_split();
    let mut uds_reader = BufReader::new(reader);
    let mut uds_writer = writer;

    let stdin = tokio::io::stdin();
    let mut stdin_reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();

    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    // Spawn stdin→UDS forwarder as a separate task.
    // When stdin hits EOF, we shutdown the UDS write side but keep
    // reading responses from the broker until UDS EOF.
    let stdin_task = tokio::spawn(async move {
        loop {
            let mut line = String::new();
            match stdin_reader.read_line(&mut line).await {
                Ok(0) | Err(_) => {
                    debug!("relay: stdin EOF");
                    // Shutdown write half so broker sees EOF for this session
                    let _ = uds_writer.shutdown().await;
                    return;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if uds_writer.write_all(line.as_bytes()).await.is_err()
                        || uds_writer.flush().await.is_err()
                    {
                        return;
                    }
                }
            }
        }
    });

    // Main loop: read broker responses → stdout, until UDS EOF or signal
    loop {
        let mut uds_line = String::new();
        tokio::select! {
            result = uds_reader.read_line(&mut uds_line) => {
                match result {
                    Ok(0) | Err(_) => {
                        debug!("relay: UDS EOF");
                        break;
                    }
                    Ok(_) => {
                        if stdout.write_all(uds_line.as_bytes()).await.is_err()
                            || stdout.flush().await.is_err()
                        {
                            break;
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("relay: SIGINT");
                break;
            }
            _ = async {
                #[cfg(unix)]
                {
                    match sigterm.as_mut() {
                        Some(s) => s.recv().await,
                        None => std::future::pending::<Option<()>>().await,
                    }
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<Option<()>>().await
                }
            } => {
                info!("relay: SIGTERM");
                break;
            }
        }
    }

    stdin_task.abort();
    Ok(())
}
