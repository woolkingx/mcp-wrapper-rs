//! Backend manager owner: child process lifecycle and process-group cleanup.

use process_wrap::tokio::{CommandWrap, ProcessGroup};
use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::{event_plane, mcp_manager};

pub type ChildPgids = Arc<std::sync::Mutex<Vec<u32>>>;

pub(crate) fn retain_utf8_tail(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }

    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
}

pub enum BackendEvent {
    Notification(serde_json::Value, BackendEventAck),
    PeerRequest(serde_json::Value),
    UnmatchedResponse(serde_json::Value),
    ProcessExit,
}

pub struct BackendEventAck(Option<oneshot::Sender<()>>);

impl BackendEventAck {
    fn none() -> Self {
        Self(None)
    }

    fn new(sender: oneshot::Sender<()>) -> Self {
        Self(Some(sender))
    }
}

impl Drop for BackendEventAck {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

struct PendingState {
    pending: HashMap<serde_json::Value, oneshot::Sender<serde_json::Value>>,
    discarded: HashSet<serde_json::Value>,
    closed: bool,
}

impl PendingState {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            discarded: HashSet::new(),
            closed: false,
        }
    }

    fn mark_discarding(&mut self, id: serde_json::Value) {
        self.discarded.insert(id);
    }
}

fn should_order_before_response(msg: &serde_json::Value) -> bool {
    let Some(method) = msg.get("method").and_then(|m| m.as_str()) else {
        return false;
    };
    event_plane::notification_program(event_plane::Dir::ServerToClient, method)
        == Some(event_plane::EventProgramId::ProgressRequest)
}

pub struct Backend {
    child_stdin: Arc<Mutex<BufWriter<tokio::process::ChildStdin>>>,
    pending: Arc<Mutex<PendingState>>,
    _notification_tx: mpsc::UnboundedSender<BackendEvent>,
    pgid: u32,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    stderr_buf: Arc<Mutex<String>>,
    server_info: Arc<Mutex<Option<Value>>>,
}

impl Backend {
    pub fn spawn(
        cmd: &str,
        args: &[String],
        notification_tx: mpsc::UnboundedSender<BackendEvent>,
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

        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdin"))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdout"))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stderr"))?;

        let pgid = child.id().unwrap_or(0);
        child_pgids.lock().unwrap().push(pgid);

        let pending: Arc<Mutex<PendingState>> = Arc::new(Mutex::new(PendingState::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let server_info = Arc::new(Mutex::new(None));

        let pending_clone = pending.clone();
        let alive_clone = alive.clone();
        let notif_tx = notification_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut progress_acks = VecDeque::new();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let msg: serde_json::Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::debug!(err = %e, line = %line, "stdout: invalid JSON, skipping");
                                continue;
                            }
                        };
                        let event = if msg.get("method").is_some() && msg.get("id").is_some() {
                            BackendEvent::PeerRequest(msg)
                        } else if msg.get("method").is_some() {
                            let ack = if should_order_before_response(&msg) {
                                let (ack_tx, ack_rx) = oneshot::channel();
                                progress_acks.push_back(ack_rx);
                                BackendEventAck::new(ack_tx)
                            } else {
                                BackendEventAck::none()
                            };
                            BackendEvent::Notification(msg, ack)
                        } else if msg.get("id").is_some() {
                            if let Some(id) = msg.get("id").cloned() {
                                let mut pending_guard = pending_clone.lock().await;
                                if let Some(tx) = pending_guard.pending.remove(&id) {
                                    drop(pending_guard);
                                    let response_acks = std::mem::take(&mut progress_acks);
                                    tokio::spawn(async move {
                                        for ack in response_acks {
                                            let _ = ack.await;
                                        }
                                        let _ = tx.send(msg);
                                    });
                                    continue;
                                }
                                if pending_guard.discarded.remove(&id) {
                                    drop(pending_guard);
                                    progress_acks.clear();
                                    tracing::debug!(id = %id, "backend: discarded late response");
                                    continue;
                                }
                            }
                            BackendEvent::UnmatchedResponse(msg)
                        } else {
                            BackendEvent::Notification(msg, BackendEventAck::none())
                        };
                        let _ = notif_tx.send(event);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(err = %e, "stdout: read error");
                        break;
                    }
                }
            }
            alive_clone.store(false, Ordering::Release);
            let pending_senders = {
                let mut pending = pending_clone.lock().await;
                pending.closed = true;
                std::mem::take(&mut pending.pending)
            };
            drop(pending_senders);
            let _ = notif_tx.send(BackendEvent::ProcessExit);
        });

        let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let stderr_buf_clone = stderr_buf.clone();
        tokio::spawn(async move {
            let mut chunk = vec![0u8; 1024];
            let mut stderr = stderr;
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) => break,
                    Err(e) => {
                        tracing::warn!(err = %e, "stderr: read error");
                        break;
                    }
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&chunk[..n]);
                        for line in text.lines() {
                            tracing::debug!(line, "backend stderr");
                        }
                        let mut guard = stderr_buf_clone.lock().await;
                        guard.push_str(&text);
                        retain_utf8_tail(&mut guard, 4096);
                    }
                }
            }
        });

        drop(child);

        Ok(Self {
            child_stdin: Arc::new(Mutex::new(BufWriter::new(stdin))),
            pending,
            _notification_tx: notification_tx,
            pgid,
            next_id: AtomicU64::new(1),
            alive,
            stderr_buf,
            server_info,
        })
    }

    pub async fn send_request(
        &self,
        msg: serde_json::Value,
    ) -> Result<serde_json::Value, std::io::Error> {
        let id = msg.get("id").cloned().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "request missing id")
        })?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.closed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "backend closed before request",
                ));
            }
            pending.pending.insert(id.clone(), tx);
        }

        if let Err(e) = self.write_message(&msg).await {
            self.pending.lock().await.pending.remove(&id);
            return Err(e);
        }

        let response = rx.await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "backend closed before response",
            )
        })?;
        Ok(response)
    }

    pub async fn send_request_with_timeout(
        &self,
        msg: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, std::io::Error> {
        let id = msg.get("id").cloned().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "request missing id")
        })?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.closed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "backend closed before request",
                ));
            }
            pending.pending.insert(id.clone(), tx);
        }

        if let Err(e) = self.write_message(&msg).await {
            self.pending.lock().await.pending.remove(&id);
            return Err(e);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "backend closed before response",
            )),
            Err(_) => {
                {
                    let mut pending = self.pending.lock().await;
                    pending.pending.remove(&id);
                    pending.mark_discarding(id.clone());
                }
                let cancel = crate::mcp_interface::build_notification(
                    "notifications/cancelled",
                    Some(serde_json::json!({
                        "requestId": id,
                        "reason": "request timed out"
                    })),
                );
                if let Err(e) = self.write_message(&cancel).await {
                    tracing::warn!(err = %e, "backend: failed to send timeout cancellation");
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "request timed out",
                ))
            }
        }
    }

    pub async fn cancel_pending_request(&self, id: &Value, reason: &str) -> bool {
        {
            let mut pending = self.pending.lock().await;
            if pending.pending.remove(id).is_none() {
                return false;
            }
            pending.mark_discarding(id.clone());
        }
        let cancel = crate::mcp_interface::build_notification(
            "notifications/cancelled",
            Some(serde_json::json!({
                "requestId": id,
                "reason": reason
            })),
        );
        if let Err(e) = self.write_message(&cancel).await {
            tracing::warn!(err = %e, "backend: failed to send cancellation");
        }
        true
    }

    pub async fn discard_pending_response(&self, id: &Value) -> bool {
        let mut pending = self.pending.lock().await;
        if pending.pending.remove(id).is_some() {
            pending.mark_discarding(id.clone());
            true
        } else {
            false
        }
    }

    pub async fn is_discarding_response(&self, id: &Value) -> bool {
        self.pending.lock().await.discarded.contains(id)
    }

    #[cfg(test)]
    pub async fn discarded_response_count(&self) -> usize {
        self.pending.lock().await.discarded.len()
    }

    #[cfg(test)]
    pub async fn mark_discarding_for_test(&self, id: Value) {
        self.pending.lock().await.mark_discarding(id);
    }

    #[cfg(test)]
    pub async fn pending_response_count(&self) -> usize {
        self.pending.lock().await.pending.len()
    }

    pub async fn send_notification(&self, msg: &serde_json::Value) -> Result<(), std::io::Error> {
        self.send_message(msg).await
    }

    pub async fn send_message(&self, msg: &serde_json::Value) -> Result<(), std::io::Error> {
        self.write_message(msg).await
    }

    async fn write_message(&self, msg: &serde_json::Value) -> Result<(), std::io::Error> {
        let mut line = serde_json::to_string(msg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let mut stdin = self.child_stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub async fn kill(&self) {
        let pid = -(self.pgid as libc::pid_t);
        tracing::debug!(pgid = self.pgid, "sending SIGTERM to process group");
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }

        for _ in 0..50 {
            if !self.is_alive() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        tracing::debug!(pgid = self.pgid, "SIGTERM timeout, sending SIGKILL");
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    pub fn next_request_id(&self) -> serde_json::Value {
        serde_json::Value::Number(self.next_id.fetch_add(1, Ordering::Relaxed).into())
    }

    pub async fn stderr_snapshot(&self) -> String {
        self.stderr_buf.lock().await.clone()
    }

    pub(crate) async fn set_server_info(&self, server_info: Value) {
        *self.server_info.lock().await = Some(server_info);
    }

    pub(crate) async fn server_info_snapshot(&self) -> Option<Value> {
        self.server_info.lock().await.clone()
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum BackendState {
    Cold,
    Starting { generation: u64 },
    Ready { generation: u64 },
    Degraded { generation: u64, last_error: String },
    Stopped,
}

impl BackendState {
    fn label(&self) -> &'static str {
        match self {
            BackendState::Cold => "cold",
            BackendState::Starting { .. } => "starting",
            BackendState::Ready { .. } => "ready",
            BackendState::Degraded { .. } => "degraded",
            BackendState::Stopped => "stopped",
        }
    }
}

pub struct BackendSnapshot {
    pub state: String,
    pub generation: u64,
    pub alive: bool,
    pub last_spawn_error: Option<String>,
    pub last_started_at_ms: Option<u64>,
    pub restart_count: u64,
    pub consecutive_spawn_failures: u64,
    pub cooldown_until_ms: Option<u64>,
}

impl BackendSnapshot {
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "state": self.state,
            "generation": self.generation,
            "alive": self.alive,
            "lastSpawnError": self.last_spawn_error,
            "lastStartedAtMs": self.last_started_at_ms,
            "restartCount": self.restart_count,
            "consecutiveSpawnFailures": self.consecutive_spawn_failures,
            "cooldownUntilMs": self.cooldown_until_ms,
        })
    }
}

const BACKEND_COOLDOWN_MS: u64 = 30_000;

#[derive(Clone)]
pub struct BackendSpec {
    pub cmd: String,
    pub args: Vec<String>,
    pub init_timeout: Duration,
    pub child_pgids: ChildPgids,
    pub notif_tx: mpsc::UnboundedSender<BackendEvent>,
}

pub struct BackendSlot {
    backend: Arc<Mutex<Option<Arc<Backend>>>>,
    spec: BackendSpec,
    generation: AtomicU64,
    restart_count: AtomicU64,
    state: Mutex<BackendState>,
    last_spawn_error: Mutex<Option<String>>,
    last_started_at_ms: AtomicU64,
    consecutive_spawn_failures: AtomicU64,
    cooldown_until_ms: AtomicU64,
}

impl BackendSlot {
    pub fn new_empty(spec: BackendSpec) -> Self {
        Self {
            backend: Arc::new(Mutex::new(None)),
            spec,
            generation: AtomicU64::new(0),
            restart_count: AtomicU64::new(0),
            state: Mutex::new(BackendState::Cold),
            last_spawn_error: Mutex::new(None),
            last_started_at_ms: AtomicU64::new(0),
            consecutive_spawn_failures: AtomicU64::new(0),
            cooldown_until_ms: AtomicU64::new(0),
        }
    }

    pub fn from_backend(spec: BackendSpec, backend: Backend) -> Self {
        Self {
            backend: Arc::new(Mutex::new(Some(Arc::new(backend)))),
            spec,
            generation: AtomicU64::new(1),
            restart_count: AtomicU64::new(0),
            state: Mutex::new(BackendState::Ready { generation: 1 }),
            last_spawn_error: Mutex::new(None),
            last_started_at_ms: AtomicU64::new(now_millis()),
            consecutive_spawn_failures: AtomicU64::new(0),
            cooldown_until_ms: AtomicU64::new(0),
        }
    }

    pub fn handle(&self) -> Arc<Mutex<Option<Arc<Backend>>>> {
        self.backend.clone()
    }

    /// Return the current backend only if it exists and is alive.
    /// Owner query: the slot owns the alive decision and the lock; callers must
    /// not reach into `handle()` to make this decision themselves.
    pub async fn live(&self) -> Option<Arc<Backend>> {
        let guard = self.backend.lock().await;
        guard.as_ref().filter(|be| be.is_alive()).cloned()
    }

    /// Lifecycle invariant: non-force restart is rejected while the
    /// caller-owned active-call count is non-zero. The counter is owned by the
    /// caller (runtime/broker); the rule is owned here.
    pub fn restart_blocked_by_active_calls(active: usize, force: bool) -> bool {
        active > 0 && !force
    }

    /// Lifecycle invariant: stop is rejected while the caller-owned active-call
    /// count is non-zero.
    pub fn stop_blocked_by_active_calls(active: usize) -> bool {
        active > 0
    }

    /// Ensure a live initialized backend exists.
    ///
    /// Returns `true` when this call replaced a previously known backend and
    /// `false` when an existing live backend was reused or the slot was cold.
    pub async fn ensure(&self) -> Result<bool, String> {
        let previous_had_backend;
        {
            let guard = self.backend.lock().await;
            previous_had_backend = guard.is_some();
            if let Some(be) = guard.as_ref() {
                if be.is_alive() {
                    tracing::debug!("backend: reusing");
                    return Ok(false);
                }
                tracing::warn!("backend: died, will respawn");
            }
        }

        if let Some(msg) = self.cooldown_error() {
            return Err(msg);
        }

        let next_generation = self.generation.load(Ordering::Relaxed) + 1;
        *self.state.lock().await = BackendState::Starting {
            generation: next_generation,
        };

        let backoff = [1, 2, 4];
        let mut last_error = None;
        for (attempt, delay) in backoff.iter().enumerate() {
            tracing::info!(attempt = attempt + 1, "backend: spawning");
            match self.spawn_initialized().await {
                Ok(new_be) => {
                    tracing::info!("backend: ready");
                    let mut guard = self.backend.lock().await;
                    if let Some(old) = guard.take() {
                        old.kill().await;
                    }
                    *guard = Some(Arc::new(new_be));
                    self.generation.store(next_generation, Ordering::Release);
                    if previous_had_backend {
                        self.restart_count.fetch_add(1, Ordering::Relaxed);
                    }
                    self.clear_circuit();
                    *self.last_spawn_error.lock().await = None;
                    self.last_started_at_ms
                        .store(now_millis(), Ordering::Release);
                    *self.state.lock().await = BackendState::Ready {
                        generation: next_generation,
                    };
                    return Ok(previous_had_backend);
                }
                Err(e) => {
                    tracing::warn!(attempt = attempt + 1, err = %e, "backend: spawn failed");
                    self.record_spawn_failure();
                    *self.last_spawn_error.lock().await = Some(e.clone());
                    last_error = Some(e);
                    if attempt < backoff.len() - 1 {
                        tokio::time::sleep(Duration::from_secs(*delay)).await;
                    }
                }
            }
        }

        let mut guard = self.backend.lock().await;
        if let Some(dead) = guard.take() {
            dead.kill().await;
        }
        let msg = last_error.unwrap_or_else(|| "backend spawn failed after 3 attempts".to_string());
        self.enter_cooldown();
        *self.state.lock().await = BackendState::Degraded {
            generation: self.generation.load(Ordering::Acquire),
            last_error: msg.clone(),
        };
        Err(format!("backend spawn failed after 3 attempts: {}", msg))
    }

    pub async fn restart(&self, force: bool) -> Result<(), String> {
        if !force {
            if let Some(msg) = self.cooldown_error() {
                return Err(format!("backend restart rejected during cooldown: {}", msg));
            }
        } else {
            self.cooldown_until_ms.store(0, Ordering::Release);
        }

        let next_generation = self.generation.load(Ordering::Relaxed) + 1;
        *self.state.lock().await = BackendState::Starting {
            generation: next_generation,
        };

        {
            let mut guard = self.backend.lock().await;
            if let Some(old) = guard.take() {
                old.kill().await;
            }
        }

        match self.spawn_initialized().await {
            Ok(new_be) => {
                let mut guard = self.backend.lock().await;
                *guard = Some(Arc::new(new_be));
                self.generation.store(next_generation, Ordering::Release);
                self.restart_count.fetch_add(1, Ordering::Relaxed);
                self.clear_circuit();
                *self.last_spawn_error.lock().await = None;
                self.last_started_at_ms
                    .store(now_millis(), Ordering::Release);
                *self.state.lock().await = BackendState::Ready {
                    generation: next_generation,
                };
                Ok(())
            }
            Err(e) => {
                self.record_spawn_failure();
                self.enter_cooldown();
                *self.last_spawn_error.lock().await = Some(e.clone());
                *self.state.lock().await = BackendState::Degraded {
                    generation: self.generation.load(Ordering::Acquire),
                    last_error: e.clone(),
                };
                Err(e)
            }
        }
    }

    pub async fn stop(&self) {
        {
            let mut guard = self.backend.lock().await;
            if let Some(old) = guard.take() {
                old.kill().await;
            }
        }
        *self.state.lock().await = BackendState::Stopped;
    }

    pub async fn snapshot(&self) -> BackendSnapshot {
        let alive = {
            let guard = self.backend.lock().await;
            guard.as_ref().map(|be| be.is_alive()).unwrap_or(false)
        };
        let state = self.state.lock().await.clone();
        let _observed_generation = match &state {
            BackendState::Starting { generation }
            | BackendState::Ready { generation }
            | BackendState::Degraded { generation, .. } => Some(*generation),
            BackendState::Cold | BackendState::Stopped => None,
        };
        let _observed_error = match &state {
            BackendState::Degraded { last_error, .. } => Some(last_error.clone()),
            _ => None,
        };
        let last_started_at_ms = match self.last_started_at_ms.load(Ordering::Acquire) {
            0 => None,
            n => Some(n),
        };
        let cooldown_until_ms = match self.cooldown_until_ms.load(Ordering::Acquire) {
            0 => None,
            n => Some(n),
        };

        BackendSnapshot {
            state: state.label().to_string(),
            generation: self.generation.load(Ordering::Acquire),
            alive,
            last_spawn_error: self.last_spawn_error.lock().await.clone(),
            last_started_at_ms,
            restart_count: self.restart_count.load(Ordering::Acquire),
            consecutive_spawn_failures: self.consecutive_spawn_failures.load(Ordering::Acquire),
            cooldown_until_ms,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    async fn spawn_initialized(&self) -> Result<Backend, String> {
        let backend = Backend::spawn(
            &self.spec.cmd,
            &self.spec.args,
            self.spec.notif_tx.clone(),
            &self.spec.child_pgids,
        )
        .map_err(|e| format!("spawn: {}", e))?;

        mcp_manager::initialize_backend(&backend, self.spec.init_timeout).await?;
        Ok(backend)
    }

    fn record_spawn_failure(&self) {
        self.consecutive_spawn_failures
            .fetch_add(1, Ordering::AcqRel);
    }

    fn enter_cooldown(&self) {
        self.cooldown_until_ms
            .store(now_millis() + BACKEND_COOLDOWN_MS, Ordering::Release);
    }

    fn clear_circuit(&self) {
        self.consecutive_spawn_failures.store(0, Ordering::Release);
        self.cooldown_until_ms.store(0, Ordering::Release);
    }

    fn cooldown_error(&self) -> Option<String> {
        let now = now_millis();
        let until = self.cooldown_until_ms.load(Ordering::Acquire);
        if until > now {
            Some(format!(
                "backend temporarily degraded; retry after {}ms",
                until - now
            ))
        } else {
            None
        }
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn new_child_pgids() -> ChildPgids {
    Arc::new(std::sync::Mutex::new(Vec::new()))
}

pub fn kill_all_pgids(child_pgids: &ChildPgids) {
    let pgids = child_pgids.lock().unwrap().clone();
    for pgid in &pgids {
        tracing::info!(pgid = pgid, "killing child process group");
        unsafe {
            libc::kill(-(*pgid as libc::pid_t), libc::SIGKILL);
        }
    }
}
