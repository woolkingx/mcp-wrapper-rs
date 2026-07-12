//! Request lifetime index for id translation and progress-token validation.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::backend_manager::Backend;

#[derive(Clone, Default)]
pub struct RequestLifetime {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    by_client: HashMap<ClientRequestKey, ActiveRequest>,
    client_by_backend: HashMap<Value, ClientRequestKey>,
    client_by_progress_token: HashMap<Value, ClientRequestKey>,
    peer_by_id: HashMap<PeerRequestKey, PeerRequest>,
    peer_by_progress_token: HashMap<PeerProgressKey, PeerRequestKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ClientRequestKey {
    session_id: Option<u64>,
    raw_id: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PeerRequestKey {
    session_id: Option<u64>,
    raw_id: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PeerProgressKey {
    session_id: Option<u64>,
    token: Value,
}

#[derive(Clone)]
struct ActiveRequest {
    backend_id: Option<Value>,
    progress_token: Option<Value>,
}

#[derive(Clone)]
struct PeerRequest {
    progress_token: Option<Value>,
}

pub struct TranslatedNotification {
    pub session_id: Option<u64>,
    pub message: Value,
}

impl RequestLifetime {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, client_id: Value, backend_id: Value, request: &Value) {
        self.insert_with_session(None, client_id, backend_id, request)
            .await;
    }

    pub async fn insert_with_session(
        &self,
        session_id: Option<u64>,
        client_id: Value,
        backend_id: Value,
        request: &Value,
    ) {
        if self
            .admit_with_session(session_id, client_id.clone(), request)
            .await
        {
            self.bind_backend(session_id, &client_id, backend_id).await;
        }
    }

    pub async fn admit_with_session(
        &self,
        session_id: Option<u64>,
        client_id: Value,
        request: &Value,
    ) -> bool {
        let progress_token = progress_token_from_request(request);
        let client_key = ClientRequestKey {
            session_id,
            raw_id: client_id,
        };
        let mut guard = self.inner.lock().await;
        if guard.by_client.contains_key(&client_key) {
            return false;
        }
        guard.by_client.insert(
            client_key,
            ActiveRequest {
                backend_id: None,
                progress_token,
            },
        );
        true
    }

    pub async fn bind_backend(
        &self,
        session_id: Option<u64>,
        client_id: &Value,
        backend_id: Value,
    ) {
        let client_key = ClientRequestKey {
            session_id,
            raw_id: client_id.clone(),
        };
        let mut guard = self.inner.lock().await;
        if !guard.by_client.contains_key(&client_key) {
            return;
        }
        let previous_backend = guard
            .by_client
            .get(&client_key)
            .and_then(|active| active.backend_id.clone());
        if let Some(previous_backend) = previous_backend {
            guard.client_by_backend.remove(&previous_backend);
        }
        guard
            .client_by_backend
            .insert(backend_id.clone(), client_key.clone());
        if let Some(active) = guard.by_client.get_mut(&client_key) {
            active.backend_id = Some(backend_id);
        }
        let progress_token = guard
            .by_client
            .get(&client_key)
            .and_then(|active| active.progress_token.clone());
        if let Some(token) = progress_token {
            guard
                .client_by_progress_token
                .insert(token, client_key.clone());
        }
    }

    pub async fn remove_client(&self, client_id: &Value) {
        self.remove_client_for_owner(None, client_id).await;
    }

    pub async fn remove_client_for_session(&self, session_id: u64, client_id: &Value) {
        self.remove_client_for_owner(Some(session_id), client_id)
            .await;
    }

    async fn remove_client_for_owner(&self, session_id: Option<u64>, client_id: &Value) {
        let client_key = ClientRequestKey {
            session_id,
            raw_id: client_id.clone(),
        };
        let mut guard = self.inner.lock().await;
        if let Some(active) = guard.by_client.remove(&client_key) {
            if let Some(backend_id) = active.backend_id {
                guard.client_by_backend.remove(&backend_id);
            }
            if let Some(token) = active.progress_token {
                if guard.client_by_progress_token.get(&token) == Some(&client_key) {
                    guard.client_by_progress_token.remove(&token);
                }
            }
        }
    }

    pub async fn timeout_client(&self, client_id: &Value) {
        self.remove_client(client_id).await;
    }

    pub async fn timeout_client_for_session(&self, session_id: u64, client_id: &Value) {
        self.remove_client_for_session(session_id, client_id).await;
    }

    pub async fn insert_peer_request(&self, session_id: Option<u64>, request: &Value) {
        let Some(peer_id) = request.get("id").cloned() else {
            return;
        };
        let progress_token = progress_token_from_request(request);
        let peer_key = PeerRequestKey {
            session_id,
            raw_id: peer_id,
        };
        let mut guard = self.inner.lock().await;
        if let Some(previous) = guard.peer_by_id.remove(&peer_key) {
            if let Some(token) = previous.progress_token {
                guard
                    .peer_by_progress_token
                    .remove(&PeerProgressKey { session_id, token });
            }
        }
        if let Some(token) = progress_token.clone() {
            guard
                .peer_by_progress_token
                .insert(PeerProgressKey { session_id, token }, peer_key.clone());
        }
        guard
            .peer_by_id
            .insert(peer_key, PeerRequest { progress_token });
    }

    pub async fn take_peer_response(&self, raw: &Value) -> bool {
        self.take_peer_response_for_owner(None, raw).await
    }

    pub async fn take_peer_response_for_session(&self, session_id: u64, raw: &Value) -> bool {
        self.take_peer_response_for_owner(Some(session_id), raw)
            .await
    }

    async fn take_peer_response_for_owner(&self, session_id: Option<u64>, raw: &Value) -> bool {
        let Some(peer_id) = raw.get("id").cloned() else {
            return false;
        };
        let peer_key = PeerRequestKey {
            session_id,
            raw_id: peer_id,
        };
        let mut guard = self.inner.lock().await;
        let Some(peer) = guard.peer_by_id.remove(&peer_key) else {
            return false;
        };
        if let Some(token) = peer.progress_token {
            guard
                .peer_by_progress_token
                .remove(&PeerProgressKey { session_id, token });
        }
        true
    }

    pub async fn cancel_client_request(&self, raw: &Value, backend: &Backend) -> bool {
        self.cancel_client_request_for_owner(None, raw, backend)
            .await
    }

    pub async fn cancel_client_request_for_session(
        &self,
        session_id: u64,
        raw: &Value,
        backend: &Backend,
    ) -> bool {
        self.cancel_client_request_for_owner(Some(session_id), raw, backend)
            .await
    }

    async fn cancel_client_request_for_owner(
        &self,
        session_id: Option<u64>,
        raw: &Value,
        backend: &Backend,
    ) -> bool {
        let Some(client_id) = notification_request_id(raw) else {
            return false;
        };
        let client_key = ClientRequestKey {
            session_id,
            raw_id: client_id,
        };
        let active = {
            let mut guard = self.inner.lock().await;
            let Some(active) = guard.by_client.remove(&client_key) else {
                return false;
            };
            if let Some(backend_id) = &active.backend_id {
                guard.client_by_backend.remove(backend_id);
            }
            if let Some(token) = active.progress_token.clone() {
                if guard.client_by_progress_token.get(&token) == Some(&client_key) {
                    guard.client_by_progress_token.remove(&token);
                }
            }
            active
        };
        let reason = raw
            .get("params")
            .and_then(|params| params.get("reason"))
            .and_then(|reason| reason.as_str())
            .unwrap_or("request cancelled");
        let Some(backend_id) = active.backend_id else {
            return true;
        };
        backend.cancel_pending_request(&backend_id, reason).await
    }

    pub async fn translate_backend_cancel(
        &self,
        raw: &Value,
        backend: &Backend,
    ) -> Option<TranslatedNotification> {
        let backend_id = notification_request_id(raw)?;
        let (client_id, session_id) = {
            let mut guard = self.inner.lock().await;
            let client_key = guard.client_by_backend.remove(&backend_id)?;
            if let Some(active) = guard.by_client.remove(&client_key) {
                if let Some(token) = active.progress_token {
                    if guard.client_by_progress_token.get(&token) == Some(&client_key) {
                        guard.client_by_progress_token.remove(&token);
                    }
                }
            }
            (client_key.raw_id, client_key.session_id)
        };
        backend.discard_pending_response(&backend_id).await;
        let translated = translate_cancel_message(raw, client_id)?;
        Some(TranslatedNotification {
            session_id,
            message: translated,
        })
    }

    pub async fn backend_progress_is_active(&self, raw: &Value) -> bool {
        let Some(token) = notification_progress_token(raw) else {
            return false;
        };
        self.inner
            .lock()
            .await
            .client_by_progress_token
            .contains_key(&token)
    }

    pub async fn client_progress_is_active(&self, raw: &Value) -> bool {
        self.client_progress_is_active_for_owner(None, raw).await
    }

    pub async fn client_progress_is_active_for_session(
        &self,
        session_id: u64,
        raw: &Value,
    ) -> bool {
        self.client_progress_is_active_for_owner(Some(session_id), raw)
            .await
    }

    async fn client_progress_is_active_for_owner(
        &self,
        session_id: Option<u64>,
        raw: &Value,
    ) -> bool {
        let Some(token) = notification_progress_token(raw) else {
            return false;
        };
        let guard = self.inner.lock().await;
        let progress_key = PeerProgressKey { session_id, token };
        let Some(peer_key) = guard.peer_by_progress_token.get(&progress_key) else {
            return false;
        };
        guard.peer_by_id.contains_key(peer_key)
    }

    pub async fn client_cancel_peer_request(&self, raw: &Value) -> bool {
        self.client_cancel_peer_request_for_owner(None, raw).await
    }

    pub async fn client_cancel_peer_request_for_session(
        &self,
        session_id: u64,
        raw: &Value,
    ) -> bool {
        self.client_cancel_peer_request_for_owner(Some(session_id), raw)
            .await
    }

    async fn client_cancel_peer_request_for_owner(
        &self,
        session_id: Option<u64>,
        raw: &Value,
    ) -> bool {
        let Some(peer_id) = notification_request_id(raw) else {
            return false;
        };
        let peer_key = PeerRequestKey {
            session_id,
            raw_id: peer_id,
        };
        let mut guard = self.inner.lock().await;
        let Some(peer) = guard.peer_by_id.remove(&peer_key) else {
            return false;
        };
        if let Some(token) = peer.progress_token {
            guard
                .peer_by_progress_token
                .remove(&PeerProgressKey { session_id, token });
        }
        true
    }

    pub async fn clear_peer_requests(&self) {
        let mut guard = self.inner.lock().await;
        guard.peer_by_id.clear();
        guard.peer_by_progress_token.clear();
    }

    pub async fn session_for_backend_progress(&self, raw: &Value) -> Option<u64> {
        let token = notification_progress_token(raw)?;
        let guard = self.inner.lock().await;
        let client_key = guard.client_by_progress_token.get(&token)?;
        guard.by_client.get(client_key)?;
        client_key.session_id
    }

    pub async fn active_count(&self) -> usize {
        self.inner.lock().await.by_client.len()
    }

    pub async fn active_count_for_session(&self, session_id: u64) -> usize {
        self.inner
            .lock()
            .await
            .by_client
            .keys()
            .filter(|key| key.session_id == Some(session_id))
            .count()
    }

    pub async fn unique_bound_session(&self) -> Option<u64> {
        let guard = self.inner.lock().await;
        let mut selected = None;
        for (key, row) in &guard.by_client {
            if row.backend_id.is_none() {
                continue;
            }
            let Some(session_id) = key.session_id else {
                continue;
            };
            match selected {
                None => selected = Some(session_id),
                Some(current) if current == session_id => {}
                Some(_) => return None,
            }
        }
        selected
    }

    pub async fn contains_client_for_session(&self, session_id: u64, client_id: &Value) -> bool {
        self.inner
            .lock()
            .await
            .by_client
            .contains_key(&ClientRequestKey {
                session_id: Some(session_id),
                raw_id: client_id.clone(),
            })
    }

    #[cfg(test)]
    pub async fn client_index_counts(&self) -> (usize, usize, usize) {
        let guard = self.inner.lock().await;
        (
            guard.by_client.len(),
            guard.client_by_backend.len(),
            guard.client_by_progress_token.len(),
        )
    }

    #[cfg(test)]
    pub async fn peer_index_counts(&self) -> (usize, usize) {
        let guard = self.inner.lock().await;
        (guard.peer_by_id.len(), guard.peer_by_progress_token.len())
    }
}

pub fn notification_request_id(raw: &Value) -> Option<Value> {
    raw.get("params")
        .and_then(|params| params.get("requestId"))
        .cloned()
}

pub(crate) fn translate_cancel_message(raw: &Value, client_id: Value) -> Option<Value> {
    let mut translated = raw.clone();
    let mut params = raw.get("params")?.as_object()?.clone();
    params.insert("requestId".to_string(), client_id);
    translated["params"] = Value::Object(params);
    Some(translated)
}

pub fn notification_progress_token(raw: &Value) -> Option<Value> {
    raw.get("params")
        .and_then(|params| params.get("progressToken"))
        .cloned()
}

fn progress_token_from_request(raw: &Value) -> Option<Value> {
    raw.get("params")
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get("progressToken"))
        .cloned()
}
