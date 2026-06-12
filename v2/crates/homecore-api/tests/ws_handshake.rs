//! End-to-end WebSocket handshake + reply tests (ADR-161, HC-WS-01/02).
//!
//! These bind a real `TcpListener`, serve the full router, and connect
//! with a real WS client (`tokio-tungstenite`). They exercise the wire
//! path the in-crate unit tests cannot.
//!
//! - `wrong_token_is_rejected` — FAILS on the pre-fix `ws.rs` that only
//!   checked `token.trim().is_empty()` and accepted any non-empty token
//!   (HC-WS-01: WS auth bypass).
//! - `result_reply_is_received` — FAILS on the pre-fix `ws.rs` that moved
//!   the socket into a recv-only task and discarded every reply with
//!   `debug!("ws emit: {msg}")` (HC-WS-02: reply theater).

use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use homecore::HomeCore;
use homecore_api::{router, LongLivedTokenStore, SharedState};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// Spawn the API on an ephemeral port with a real (non-dev) token store
/// containing exactly one valid token. Returns the bound address.
async fn spawn_server_with_token(valid_token: &str) -> SocketAddr {
    let hc = HomeCore::new();
    let tokens = LongLivedTokenStore::empty();
    tokens.register(valid_token).await;
    let state = SharedState::with_tokens(hc, "Test", "test-version", tokens);
    let app = router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Read text frames until one parses as JSON; returns the parsed value.
async fn next_json<S>(ws: &mut S) -> serde_json::Value
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(raw))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    return v;
                }
            }
            Some(Ok(_)) => continue,
            other => panic!("expected text frame, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    // HC-WS-01: a provisioned store with one good token must reject a
    // DIFFERENT (non-empty) token over the WS handshake. The old code
    // sent `auth_ok` for any non-empty token — this asserts the fix.
    let addr = spawn_server_with_token("good_token_abc").await;
    let url = format!("ws://{addr}/api/websocket");
    let (mut ws, _resp) = connect_async(&url).await.unwrap();

    // Server → auth_required
    let req = next_json(&mut ws).await;
    assert_eq!(req["type"], "auth_required");

    // Client → auth with the WRONG token
    ws.send(Message::Text(
        serde_json::json!({"type":"auth","access_token":"wrong_token_xyz"}).to_string(),
    ))
    .await
    .unwrap();

    // Server → auth_invalid (NOT auth_ok)
    let resp = next_json(&mut ws).await;
    assert_eq!(
        resp["type"], "auth_invalid",
        "wrong token must be rejected with auth_invalid, got: {resp}"
    );
    assert_ne!(resp["type"], "auth_ok", "wrong token must NOT receive auth_ok");
}

#[tokio::test]
async fn correct_token_is_accepted() {
    let addr = spawn_server_with_token("good_token_abc").await;
    let url = format!("ws://{addr}/api/websocket");
    let (mut ws, _resp) = connect_async(&url).await.unwrap();

    let req = next_json(&mut ws).await;
    assert_eq!(req["type"], "auth_required");

    ws.send(Message::Text(
        serde_json::json!({"type":"auth","access_token":"good_token_abc"}).to_string(),
    ))
    .await
    .unwrap();

    let resp = next_json(&mut ws).await;
    assert_eq!(resp["type"], "auth_ok", "correct token should be accepted, got: {resp}");
}

#[tokio::test]
async fn result_reply_is_received() {
    // HC-WS-02: after a successful auth, a `get_states` command must
    // produce a `result` reply RECEIVED over the socket. The old code
    // discarded all replies in the rx-draining task, so this hangs/
    // fails on the pre-fix source.
    let addr = spawn_server_with_token("good_token_abc").await;
    let url = format!("ws://{addr}/api/websocket");
    let (mut ws, _resp) = connect_async(&url).await.unwrap();

    let req = next_json(&mut ws).await;
    assert_eq!(req["type"], "auth_required");

    ws.send(Message::Text(
        serde_json::json!({"type":"auth","access_token":"good_token_abc"}).to_string(),
    ))
    .await
    .unwrap();
    let auth = next_json(&mut ws).await;
    assert_eq!(auth["type"], "auth_ok");

    // Send a command and assert we RECEIVE a result reply.
    ws.send(Message::Text(
        serde_json::json!({"id": 1, "type": "get_states"}).to_string(),
    ))
    .await
    .unwrap();

    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), next_json(&mut ws))
        .await
        .expect("did not receive a reply within 5s — reply theater (HC-WS-02)");
    assert_eq!(reply["type"], "result", "expected a result reply, got: {reply}");
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["success"], true);
}

#[tokio::test]
async fn ping_pong_reply_is_received() {
    // The `ping` command must produce a `pong` reply on the wire — also
    // exercises the writer task that HC-WS-02 introduced.
    let addr = spawn_server_with_token("good_token_abc").await;
    let url = format!("ws://{addr}/api/websocket");
    let (mut ws, _resp) = connect_async(&url).await.unwrap();

    let _ = next_json(&mut ws).await; // auth_required
    ws.send(Message::Text(
        serde_json::json!({"type":"auth","access_token":"good_token_abc"}).to_string(),
    ))
    .await
    .unwrap();
    let _ = next_json(&mut ws).await; // auth_ok

    ws.send(Message::Text(
        serde_json::json!({"id": 7, "type": "ping"}).to_string(),
    ))
    .await
    .unwrap();

    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), next_json(&mut ws))
        .await
        .expect("did not receive pong within 5s");
    assert_eq!(reply["type"], "pong");
    assert_eq!(reply["id"], 7);
}
