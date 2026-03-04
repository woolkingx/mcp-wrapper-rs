//! Backend subprocess lifecycle management.
//!
//! Spawns a child MCP server process, communicates via stdin/stdout JSON-RPC
//! lines, and routes responses to callers via oneshot channels.

use process_wrap::tokio::{CommandWrap, ProcessGroup};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

/// Registry of child process group IDs for cleanup on exit.
pub type ChildPgids = Arc<std::sync::Mutex<Vec<u32>>>;

/// Low-level handle to a child MCP server process.
/// Manages stdin writes, response routing, and process lifecycle.
pub struct Backend {
    child_stdin: Arc<Mutex<BufWriter<tokio::process::ChildStdin>>>,
    pending: Arc<Mutex<HashMap<serde_json::Value, oneshot::Sender<serde_json::Value>>>>,
    notification_tx: mpsc::UnboundedSender<serde_json::Value>,
    pgid: u32,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    stderr_buf: Arc<Mutex<String>>,
}

impl Backend {
    /// Spawn a child MCP server process and wire up I/O routing.
    ///
    /// - `cmd`/`args`: the subprocess command line
    /// - `notification_tx`: channel for JSON-RPC notifications (messages without matching pending waiter)
    /// - `child_pgids`: shared registry for cleanup on wrapper exit
    pub fn spawn(
        cmd: &str,
        args: &[String],
        notification_tx: mpsc::UnboundedSender<serde_json::Value>,
        child_pgids: &ChildPgids,
    ) -> std::io::Result<Self> {
        let mut command = Command::new(cmd);
        command.args(args);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut wrapped = CommandWrap::from(command);
        wrapped.wrap(ProcessGroup::leader());
        let mut child = wrapped.spawn()?;

        let stdin = child.stdin().take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdin"))?;
        let stdout = child.stdout().take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdout"))?;
        let stderr = child.stderr().take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stderr"))?;

        let pgid = child.id().unwrap_or(0);
        child_pgids.lock().unwrap().push(pgid);

        let pending: Arc<Mutex<HashMap<serde_json::Value, oneshot::Sender<serde_json::Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));

        // Stdout reader: parse JSON lines, route to pending waiters or notification channel
        let pending_clone = pending.clone();
        let alive_clone = alive.clone();
        let notif_tx = notification_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let msg: serde_json::Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(e) => {
                                debug!(err = %e, line = %line, "stdout: invalid JSON, skipping");
                                continue;
                            }
                        };
                        // Response: has `id` field and matches a pending waiter
                        if let Some(id) = msg.get("id").cloned() {
                            let sender = pending_clone.lock().await.remove(&id);
                            if let Some(tx) = sender {
                                let _ = tx.send(msg);
                                continue;
                            }
                        }
                        // Notification or unmatched response → forward
                        let _ = notif_tx.send(msg);
                    }
                    Ok(None) => break, // EOF
                    Err(e) => {
                        warn!(err = %e, "stdout: read error");
                        break;
                    }
                }
            }
            alive_clone.store(false, Ordering::Release);
        });

        // Stderr reader: keep last 4KB in ring buffer
        let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let stderr_buf_clone = stderr_buf.clone();
        tokio::spawn(async move {
            let mut chunk = vec![0u8; 1024];
            let mut stderr = stderr;
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) => break,
                    Err(e) => {
                        warn!(err = %e, "stderr: read error");
                        break;
                    }
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&chunk[..n]);
                        for line in text.lines() {
                            debug!(line, "backend stderr");
                        }
                        let mut guard = stderr_buf_clone.lock().await;
                        guard.push_str(&text);
                        if guard.len() > 4096 {
                            let trim_at = guard.len() - 4096;
                            *guard = guard[trim_at..].to_string();
                        }
                    }
                }
            }
        });

        // Let the child be detached — we kill via pgid, not child handle
        drop(child);

        Ok(Self {
            child_stdin: Arc::new(Mutex::new(BufWriter::new(stdin))),
            pending,
            notification_tx,
            pgid,
            next_id: AtomicU64::new(1),
            alive,
            stderr_buf,
        })
    }

    /// Send a JSON-RPC request and wait for the matching response.
    /// The message must contain an `id` field for response correlation.
    pub async fn send_request(
        &self,
        msg: serde_json::Value,
    ) -> Result<serde_json::Value, std::io::Error> {
        let id = msg.get("id").cloned().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "request missing id")
        })?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        if let Err(e) = self.write_message(&msg).await {
            // Remove pending entry on write failure
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        rx.await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "backend closed before response")
        })
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn send_notification(
        &self,
        msg: &serde_json::Value,
    ) -> Result<(), std::io::Error> {
        self.write_message(msg).await
    }

    /// Write a JSON message as a single line to child stdin.
    async fn write_message(&self, msg: &serde_json::Value) -> Result<(), std::io::Error> {
        let mut line = serde_json::to_string(msg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let mut stdin = self.child_stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Check if the child process stdout reader is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Graceful shutdown: SIGTERM, then SIGKILL after 5 seconds.
    pub async fn kill(&self) {
        let pid = -(self.pgid as libc::pid_t);
        debug!(pgid = self.pgid, "sending SIGTERM to process group");
        unsafe { libc::kill(pid, libc::SIGTERM); }

        // Wait up to 5s for stdout reader to notice child exit
        for _ in 0..50 {
            if !self.is_alive() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        debug!(pgid = self.pgid, "SIGTERM timeout, sending SIGKILL");
        unsafe { libc::kill(pid, libc::SIGKILL); }
    }

    /// Generate a monotonically increasing integer request ID.
    pub fn next_request_id(&self) -> serde_json::Value {
        serde_json::Value::Number(
            self.next_id.fetch_add(1, Ordering::Relaxed).into(),
        )
    }

    /// Process group ID of the child.
    pub fn pgid(&self) -> u32 {
        self.pgid
    }

    /// Access the last ~4KB of child stderr output for error reporting.
    pub async fn stderr_snapshot(&self) -> String {
        self.stderr_buf.lock().await.clone()
    }
}
