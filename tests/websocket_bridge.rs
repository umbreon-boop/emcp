use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_tungstenite::tungstenite::Message;
use emcp::{bridge::Bridge, ws};
use futures_util::StreamExt;
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_requests_between_multiple_clients_and_attributes_events() {
    let bridge = Bridge::new(tokio::runtime::Handle::current());
    let address = ws::spawn("127.0.0.1:0".parse().unwrap(), bridge.clone()).unwrap();

    let alice_requests = Arc::new(AtomicUsize::new(0));
    let bob_requests = Arc::new(AtomicUsize::new(0));
    let alice = spawn_client(address, "alice", alice_requests.clone());
    let bob = spawn_client(address, "bob", bob_requests.clone());

    wait_for_clients(&bridge, &["alice", "bob"]).await;

    let clients = bridge.clients();
    assert_eq!(clients.len(), 2);
    let alice_client = clients
        .iter()
        .find(|client| client.username == "alice")
        .expect("alice is missing");
    let bob_client = clients
        .iter()
        .find(|client| client.username == "bob")
        .expect("bob is missing");

    let error = bridge
        .execute(
            None,
            "run",
            json!({ "code": "return 1" }),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(
        error.contains("multiple clients"),
        "unexpected ambiguous-client error: {error}"
    );

    let (target, result) = bridge
        .execute(
            Some("ALICE"),
            "run",
            json!({ "code": "return 1" }),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(target.username, "alice");
    assert_eq!(result["client"], "alice");
    assert_eq!(alice_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        bob_requests.load(Ordering::SeqCst),
        0,
        "a request addressed to alice reached bob"
    );

    let alice_events = bridge.take_events(alice_client.id);
    assert_eq!(alice_events.len(), 1);
    assert_eq!(alice_events[0]["kind"], "warn");
    assert_eq!(alice_events[0]["text"], "alice warns");
    assert_eq!(alice_events[0]["client"], "alice");
    assert!(
        bridge.take_events(bob_client.id).is_empty(),
        "alice's event leaked into bob's queue"
    );

    let (target, result) = bridge
        .execute(
            Some("bob"),
            "run",
            json!({ "code": "return 2" }),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(target.username, "bob");
    assert_eq!(result["client"], "bob");
    assert_eq!(bob_requests.load(Ordering::SeqCst), 1);
    assert_eq!(alice_requests.load(Ordering::SeqCst), 1);

    let error = bridge
        .execute(
            Some("nobody"),
            "run",
            json!({ "code": "return 1" }),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(
        error.contains("no connected client matches"),
        "unexpected unknown-client error: {error}"
    );

    let _ = bridge
        .execute(Some("alice"), "stop", json!({}), Duration::from_secs(2))
        .await;
    let _ = bridge
        .execute(Some("bob"), "stop", json!({}), Duration::from_secs(2))
        .await;
    alice.join().unwrap();
    bob.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waits_for_an_executor_to_identify_itself_or_times_out() {
    let bridge = Bridge::new(tokio::runtime::Handle::current());
    let address = ws::spawn("127.0.0.1:0".parse().unwrap(), bridge.clone()).unwrap();

    assert!(
        bridge
            .wait_for_client(Duration::from_millis(50))
            .await
            .is_none(),
        "an empty bridge reported a client"
    );

    let requests = Arc::new(AtomicUsize::new(0));
    let carol = spawn_client(address, "carol", requests);
    let client = bridge
        .wait_for_client(Duration::from_secs(5))
        .await
        .expect("carol did not identify herself");

    assert_eq!(client.username, "carol");

    let _ = bridge
        .execute(Some("carol"), "stop", json!({}), Duration::from_secs(2))
        .await;
    carol.join().unwrap();
}

fn spawn_client(
    address: SocketAddr,
    username: &'static str,
    requests: Arc<AtomicUsize>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        smol::block_on(async move {
            let stream = async_net::TcpStream::connect(address).await.unwrap();
            let (mut socket, _) =
                async_tungstenite::client_async(format!("ws://{address}/"), stream)
                    .await
                    .unwrap();

            socket
                .send(Message::Text(
                    json!({
                        "type": "hello",
                        "username": username,
                        "executor": "test",
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            while let Some(frame) = socket.next().await {
                let Message::Text(text) = frame.unwrap() else {
                    continue;
                };
                let message: Value = serde_json::from_str(&text).unwrap();
                if message.get("type").and_then(Value::as_str) != Some("request") {
                    continue;
                }

                requests.fetch_add(1, Ordering::SeqCst);
                let id = message.get("id").and_then(Value::as_str).unwrap();
                let stop = message.get("method").and_then(Value::as_str) == Some("stop");

                socket
                    .send(Message::Text(
                        json!({
                            "type": "event",
                            "event": "warn",
                            "text": format!("{username} warns"),
                            "traceback": "stack traceback:",
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
                socket
                    .send(Message::Text(
                        json!({
                            "type": "response",
                            "id": id,
                            "ok": true,
                            "result": { "client": username },
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();

                if stop {
                    let _ = socket.close(None).await;
                    break;
                }
            }
        });
    })
}

async fn wait_for_clients(bridge: &Bridge, expected: &[&str]) {
    for _ in 0..300 {
        let clients = bridge.clients();
        if clients.len() == expected.len()
            && expected.iter().all(|expected| {
                clients
                    .iter()
                    .any(|client| client.username.eq_ignore_ascii_case(expected))
            })
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "clients did not identify themselves: {:?}",
        bridge.clients()
    );
}
