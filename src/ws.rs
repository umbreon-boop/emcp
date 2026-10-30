use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_tungstenite::tungstenite::Message;
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::bridge::{Bridge, new_session_channel};

struct ConnectionState {
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<Result<Value, String>>>>,
    closed: AtomicBool,
}

pub fn spawn(addr: SocketAddr, bridge: Bridge) -> anyhow::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind(addr)?;
    let local_addr = listener.local_addr()?;

    std::thread::Builder::new()
        .name("emcp-websocket".to_string())
        .spawn(move || {
            smol::block_on(async move {
                let listener = match async_net::TcpListener::try_from(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        tracing::error!(%error, "failed to initialize websocket listener");
                        return;
                    }
                };

                loop {
                    match listener.accept().await {
                        Ok((stream, peer)) => {
                            tracing::info!(%peer, "websocket client accepted");
                            let bridge = bridge.clone();
                            smol::spawn(handle_connection(stream, peer, bridge)).detach();
                        }
                        Err(error) => {
                            tracing::error!(%error, "websocket accept failed");
                            smol::Timer::after(Duration::from_millis(250)).await;
                        }
                    }
                }
            });
        })?;

    Ok(local_addr)
}

async fn handle_connection(stream: async_net::TcpStream, peer: SocketAddr, bridge: Bridge) {
    let ws = match async_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(error) => {
            tracing::warn!(%peer, %error, "websocket handshake failed");
            return;
        }
    };

    let (call_tx, call_rx) = new_session_channel();
    let session_id = bridge.register_session(call_tx.clone());

    let state = Arc::new(ConnectionState {
        pending: Mutex::new(HashMap::new()),
        closed: AtomicBool::new(false),
    });

    let (mut sink, mut stream) = ws.split();

    let writer_state = state.clone();
    let writer = smol::spawn(async move {
        while let Ok(call) = call_rx.recv().await {
            let id = call.id.clone();

            {
                let mut pending = writer_state.pending.lock().expect("pending lock poisoned");
                if writer_state.closed.load(Ordering::SeqCst) {
                    let _ = call
                        .reply
                        .send(Err("executor disconnected before sending".to_string()));
                    break;
                }
                pending.insert(id.clone(), call.reply);
            }

            let request = json!({
                "type": "request",
                "id": id,
                "method": call.method,
                "params": call.params,
            });
            let text = match serde_json::to_string(&request) {
                Ok(text) => text,
                Err(error) => {
                    if let Some(reply) = writer_state
                        .pending
                        .lock()
                        .expect("pending lock poisoned")
                        .remove(&id)
                    {
                        let _ = reply.send(Err(format!("failed to encode request: {error}")));
                    }
                    continue;
                }
            };

            if let Err(error) = sink.send(Message::Text(text.into())).await {
                if let Some(reply) = writer_state
                    .pending
                    .lock()
                    .expect("pending lock poisoned")
                    .remove(&id)
                {
                    let _ = reply.send(Err(format!("websocket send failed: {error}")));
                }
                writer_state.closed.store(true, Ordering::SeqCst);
                break;
            }
        }
    });

    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Text(text)) => handle_text(&text, &bridge, &state, session_id),
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_)) => {}
            Err(error) => {
                tracing::warn!(%peer, %error, "websocket read failed");
                break;
            }
        }
    }

    state.closed.store(true, Ordering::SeqCst);
    bridge.unregister_session(session_id);
    drop(call_tx);

    let pending = {
        let mut pending = state.pending.lock().expect("pending lock poisoned");
        pending.drain().map(|(_, reply)| reply).collect::<Vec<_>>()
    };
    for reply in pending {
        let _ = reply.send(Err("executor disconnected before answering".to_string()));
    }

    writer.detach();
    tracing::info!(%peer, "websocket client disconnected");
}

fn handle_text(text: &str, bridge: &Bridge, state: &ConnectionState, session_id: u64) {
    let message: Value = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            bridge.push_event(
                session_id,
                "protocol_error",
                format!("invalid JSON from executor: {error}"),
                None,
            );
            return;
        }
    };

    match message.get("type").and_then(Value::as_str) {
        Some("hello") => {
            let username = message
                .get("username")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|username| !username.is_empty());
            let executor = message
                .get("executor")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match username {
                Some(username) => bridge.identify_session(session_id, username, executor),
                None => tracing::warn!(
                    client = session_id,
                    "executor hello is missing a non-empty username"
                ),
            }
        }
        Some("event") => {
            let kind = message
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or("event");
            let text = message.get("text").and_then(Value::as_str).unwrap_or("");
            let traceback = message
                .get("traceback")
                .and_then(Value::as_str)
                .map(str::to_owned);
            bridge.push_event(session_id, kind, text, traceback);
        }
        Some("response") => {
            let id = message.get("id").and_then(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| value.as_u64().map(|id| id.to_string()))
            });
            let Some(id) = id else {
                bridge.push_event(
                    session_id,
                    "protocol_error",
                    "executor response is missing an id",
                    None,
                );
                return;
            };

            let reply = state
                .pending
                .lock()
                .expect("pending lock poisoned")
                .remove(&id);
            let Some(reply) = reply else {
                tracing::debug!(%id, "received response for unknown request");
                return;
            };

            let result = if message.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            } else {
                Err(message
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("executor returned an unspecified error")
                    .to_string())
            };
            let _ = reply.send(result);
        }
        Some(other) => {
            tracing::debug!(kind = other, "ignoring unknown websocket message");
        }
        None => {
            bridge.push_event(
                session_id,
                "protocol_error",
                "executor message is missing a type",
                None,
            );
        }
    }
}
