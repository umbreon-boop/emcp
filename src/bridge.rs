use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_channel::{Receiver, Sender};
use rmcp::service::{Peer, RoleServer};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

const MAX_EVENTS: usize = 512;

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeEvent {
    pub seq: u64,
    pub kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traceback: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientSummary {
    /// Internal session key. Usernames are the public client identifier.
    #[serde(skip)]
    pub id: u64,
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    pub connected_at_unix_ms: u64,
}

pub struct Call {
    pub id: String,
    pub method: String,
    pub params: Value,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

struct Session {
    summary: ClientSummary,
    tx: Sender<Call>,
    events: VecDeque<RuntimeEvent>,
    execute_lock: Arc<tokio::sync::Mutex<()>>,
    identified: bool,
}

struct Inner {
    sessions: Mutex<HashMap<u64, Session>>,
    peer: Mutex<Option<Peer<RoleServer>>>,
    next_request: AtomicU64,
    next_event: AtomicU64,
    next_session: AtomicU64,
    clients_changed: watch::Sender<u64>,
    tokio: tokio::runtime::Handle,
}

#[derive(Clone)]
pub struct Bridge {
    inner: Arc<Inner>,
}

impl Bridge {
    pub fn new(tokio: tokio::runtime::Handle) -> Self {
        let (clients_changed, _) = watch::channel(0);
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                peer: Mutex::new(None),
                next_request: AtomicU64::new(1),
                next_event: AtomicU64::new(1),
                next_session: AtomicU64::new(1),
                clients_changed,
                tokio,
            }),
        }
    }

    pub fn set_peer(&self, peer: Peer<RoleServer>) {
        *self.inner.peer.lock().expect("peer lock poisoned") = Some(peer);
    }

    pub fn register_session(&self, tx: Sender<Call>) -> u64 {
        let id = self.inner.next_session.fetch_add(1, Ordering::Relaxed);
        let summary = ClientSummary {
            id,
            username: format!("client-{id}"),
            executor: None,
            connected_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0),
        };

        self.inner
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .insert(
                id,
                Session {
                    summary,
                    tx,
                    events: VecDeque::new(),
                    execute_lock: Arc::new(tokio::sync::Mutex::new(())),
                    identified: false,
                },
            );
        tracing::info!(client = id, "executor session registered");
        id
    }

    pub fn identify_session(&self, id: u64, username: &str, executor: &str) {
        let username = username.trim();
        if username.is_empty() {
            return;
        }

        let identified = {
            let mut sessions = self.inner.sessions.lock().expect("sessions lock poisoned");
            match sessions.get_mut(&id) {
                Some(session) => {
                    session.summary.username = username.to_string();
                    if !executor.trim().is_empty() {
                        session.summary.executor = Some(executor.trim().to_string());
                    }
                    session.identified = true;
                    tracing::info!(client = id, username, "executor client identified");
                    true
                }
                None => false,
            }
        };

        if identified {
            self.inner
                .clients_changed
                .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        }
    }

    pub fn unregister_session(&self, id: u64) {
        let removed = self
            .inner
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .remove(&id);
        if removed.is_some() {
            tracing::info!(client = id, "executor session unregistered");
        }
    }

    pub fn is_connected(&self) -> bool {
        !self
            .inner
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .is_empty()
    }

    pub fn clients(&self) -> Vec<ClientSummary> {
        let mut clients = self
            .inner
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .values()
            .map(|session| session.summary.clone())
            .collect::<Vec<_>>();
        clients.sort_by(|left, right| {
            left.username
                .to_lowercase()
                .cmp(&right.username.to_lowercase())
                .then(left.id.cmp(&right.id))
        });
        clients
    }

    /// Wait until an executor connects and sends its hello, or the timeout expires.
    pub async fn wait_for_client(&self, timeout: Duration) -> Option<ClientSummary> {
        let mut changes = self.inner.clients_changed.subscribe();
        let deadline = Instant::now() + timeout;

        loop {
            {
                let sessions = self.inner.sessions.lock().expect("sessions lock poisoned");
                if let Some(session) = sessions.values().find(|session| session.identified) {
                    return Some(session.summary.clone());
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            if tokio::time::timeout(remaining, changes.changed())
                .await
                .is_err()
            {
                return None;
            }
        }
    }

    pub fn resolve_client(&self, selector: Option<&str>) -> Result<ClientSummary, String> {
        let sessions = self.inner.sessions.lock().expect("sessions lock poisoned");
        if sessions.is_empty() {
            return Err(
                "executor is not connected; run runtime.lua inside the executor first".to_string(),
            );
        }

        let selector = selector
            .map(str::trim)
            .filter(|selector| !selector.is_empty());
        let Some(selector) = selector else {
            if sessions.len() == 1 {
                return Ok(sessions
                    .values()
                    .next()
                    .expect("sessions is not empty")
                    .summary
                    .clone());
            }
            return Err(format!(
                "multiple clients are connected; pass `client` with a username. Connected: {}",
                describe_clients(sessions.values().map(|session| &session.summary))
            ));
        };

        let mut matched = sessions
            .values()
            .filter(|session| session.summary.username.eq_ignore_ascii_case(selector))
            .map(|session| session.summary.clone());

        match (matched.next(), matched.next()) {
            (Some(client), None) => Ok(client),
            (None, _) => Err(format!(
                "no connected client matches `{selector}`. Connected: {}",
                describe_clients(sessions.values().map(|session| &session.summary))
            )),
            (Some(_), Some(_)) => Err(format!(
                "multiple connections use the username `{selector}`; close the stale executor first. Connected: {}",
                describe_clients(sessions.values().map(|session| &session.summary))
            )),
        }
    }

    pub async fn execute(
        &self,
        client: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<(ClientSummary, Value), String> {
        let target = self.resolve_client(client)?;
        let result = self.execute_for(&target, method, params, timeout).await?;
        Ok((target, result))
    }

    pub async fn execute_for(
        &self,
        client: &ClientSummary,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let (tx, execute_lock) = {
            let sessions = self.inner.sessions.lock().expect("sessions lock poisoned");
            let session = sessions
                .get(&client.id)
                .ok_or_else(|| format!("client `{}` is not connected", client.username))?;
            (session.tx.clone(), session.execute_lock.clone())
        };

        let _guard = execute_lock.lock().await;
        let id = self
            .inner
            .next_request
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let (reply, response) = oneshot::channel();

        tx.try_send(Call {
            id,
            method: method.to_string(),
            params,
            reply,
        })
        .map_err(|_| format!("client `{}` command queue is closed", client.username))?;

        match tokio::time::timeout(timeout, response).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!(
                "client `{}` disconnected before answering",
                client.username
            )),
            Err(_) => Err(format!(
                "client `{}` did not answer within {} ms",
                client.username,
                timeout.as_millis()
            )),
        }
    }

    pub fn push_event(
        &self,
        client_id: u64,
        kind: impl Into<String>,
        text: impl Into<String>,
        traceback: Option<String>,
    ) {
        let event = RuntimeEvent {
            seq: self.inner.next_event.fetch_add(1, Ordering::Relaxed),
            kind: kind.into(),
            text: text.into(),
            traceback,
        };

        let client = {
            let mut sessions = self.inner.sessions.lock().expect("sessions lock poisoned");
            match sessions.get_mut(&client_id) {
                Some(session) => {
                    if session.events.len() >= MAX_EVENTS {
                        session.events.pop_front();
                    }
                    session.events.push_back(event.clone());
                    Some(session.summary.clone())
                }
                None => None,
            }
        };

        let Some(client) = client else {
            tracing::debug!(
                client = client_id,
                "dropping event from a disconnected client"
            );
            return;
        };

        tracing::info!(
            client = client.id,
            username = %client.username,
            kind = %event.kind,
            text = %event.text,
            "runtime event"
        );
        self.notify_peer(&event, &client);
    }

    pub fn take_events(&self, client_id: u64) -> Vec<Value> {
        let mut sessions = self.inner.sessions.lock().expect("sessions lock poisoned");
        let Some(session) = sessions.get_mut(&client_id) else {
            return Vec::new();
        };
        let client = session.summary.clone();
        session
            .events
            .drain(..)
            .map(|event| event_value(event, &client))
            .collect()
    }

    pub fn take_all_events(&self) -> Vec<Value> {
        let mut sessions = self.inner.sessions.lock().expect("sessions lock poisoned");
        let mut values = Vec::new();
        for session in sessions.values_mut() {
            let client = session.summary.clone();
            values.extend(
                session
                    .events
                    .drain(..)
                    .map(|event| event_value(event, &client)),
            );
        }
        values.sort_by_key(|value| value.get("seq").and_then(Value::as_u64).unwrap_or(0));
        values
    }

    #[allow(deprecated)]
    fn notify_peer(&self, event: &RuntimeEvent, client: &ClientSummary) {
        let Some(peer) = self.inner.peer.lock().expect("peer lock poisoned").clone() else {
            return;
        };

        let level = match event.kind.as_str() {
            "warn" => rmcp::model::LoggingLevel::Warning,
            "dbg" => rmcp::model::LoggingLevel::Info,
            _ => rmcp::model::LoggingLevel::Info,
        };
        let params = rmcp::model::LoggingMessageNotificationParam::new(
            level,
            json!({
                "kind": event.kind,
                "text": event.text,
                "traceback": event.traceback,
                "client": client.username,
            }),
        )
        .with_logger("emcp");

        self.inner.tokio.spawn(async move {
            let _ = peer.notify_logging_message(params).await;
        });
    }
}

fn describe_clients<'a>(clients: impl Iterator<Item = &'a ClientSummary>) -> String {
    let mut described = clients
        .map(|client| client.username.clone())
        .collect::<Vec<_>>();
    described.sort();
    described.join(", ")
}

fn event_value(event: RuntimeEvent, client: &ClientSummary) -> Value {
    let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
    if let Value::Object(object) = &mut value {
        object.insert("client".to_string(), Value::String(client.username.clone()));
    }
    value
}

pub fn new_session_channel() -> (Sender<Call>, Receiver<Call>) {
    async_channel::unbounded()
}
