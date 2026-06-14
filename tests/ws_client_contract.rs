#![allow(clippy::result_large_err)]
//! Contract tests for `studio_worker::ws::client`.
//!
//! Spin up a `tokio-tungstenite` server bound to an ephemeral port,
//! drive the production `WsClient` against it, and assert the
//! protocol-level behaviour (URL coercion, auth header, sub-protocol
//! header, error mapping, message round-trip).
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use studio_worker::types::WorkerCapabilities;
use studio_worker::ws::client::{connect, WsClientError};
use studio_worker::ws::types::{HelloFrame, WorkerInbound, WorkerOutbound};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
use tokio_tungstenite::tungstenite::Message;

/// Helper that echoes the worker's sub-protocol in the upgrade
/// response, matching what the production DO does.
fn echo_subprotocol(_req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
    resp.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("studio-worker-v1"),
    );
    Ok(resp)
}

const TIMEOUT: Duration = Duration::from_secs(5);

fn capabilities() -> WorkerCapabilities {
    WorkerCapabilities {
        machine_name: "rig".into(),
        username: "webber".into(),
        agent_version: "0.2.0".into(),
        engine: "synthetic".into(),
        vram_total_gb: 24.0,
        vram_threshold_gb: 12.0,
        auto_enabled: true,
        auto_start: false,
        supported_models: vec!["synthetic".into()],
        task_kinds: vec![],
        supported_models_per_kind: Default::default(),
    }
}

/// Spawn a one-shot WS server that lets each test customise the
/// upgrade-time behaviour (status code, subprotocol echo) and the
/// post-upgrade message loop.
async fn spawn_server<F, G, Fut>(upgrade: F, handle: G) -> (SocketAddr, tokio::task::JoinHandle<()>)
where
    F: Fn(&Request, Response) -> Result<Response, ErrorResponse> + Send + Sync + 'static,
    G: Fn(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let result =
                tokio_tungstenite::accept_hdr_async(stream, |req: &Request, resp: Response| {
                    upgrade(req, resp)
                })
                .await;
            if let Ok(ws) = result {
                handle(ws).await
            }
        }
    });
    (addr, handle)
}

/// Asserts that we coerce `http://` to `ws://` and pass through the
/// `Authorization` + sub-protocol headers cleanly.
#[tokio::test]
async fn connect_sends_bearer_and_sub_protocol() {
    let (addr, server) = spawn_server(
        |req, resp| {
            assert_eq!(
                req.headers().get("authorization").unwrap(),
                "Bearer the-token"
            );
            assert_eq!(
                req.headers()
                    .get("sec-websocket-protocol")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "studio-worker-v1"
            );
            echo_subprotocol(req, resp)
        },
        |mut ws| async move {
            // Close cleanly so the client side returns.
            let _ = ws.close(None).await;
        },
    )
    .await;

    let base = format!("http://{addr}/graphics/api");
    let client = tokio::time::timeout(TIMEOUT, connect(&base, "w-1", "the-token"))
        .await
        .expect("connect timed out")
        .expect("connect succeeded");
    drop(client);
    let _ = server.await;
}

/// Successful hello + welcome round-trip over the live socket.
#[tokio::test]
async fn round_trips_a_hello_then_a_welcome() {
    let (addr, server) = spawn_server(echo_subprotocol, |mut ws| async move {
        // Wait for the client's hello.
        let frame = ws
            .next()
            .await
            .expect("frame")
            .expect("ok")
            .into_text()
            .unwrap();
        assert!(frame.contains("\"hello\""));
        // Reply with a welcome.
        let welcome =
            serde_json::to_string(&json!({"type":"welcome","workerId":"w-1","serverTime":"now"}))
                .unwrap();
        ws.send(Message::Text(welcome.into())).await.unwrap();
        let _ = ws.close(None).await;
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let mut client = connect(&base, "w-1", "t").await.unwrap();
    client
        .send(&WorkerInbound::Hello(HelloFrame {
            auth_token: "t".into(),
            capabilities: capabilities(),
        }))
        .await
        .unwrap();
    let received = tokio::time::timeout(TIMEOUT, client.recv())
        .await
        .expect("recv timed out")
        .expect("recv ok")
        .expect("got a frame");
    match received {
        WorkerOutbound::Welcome { worker_id, .. } => assert_eq!(worker_id, "w-1"),
        other => panic!("expected welcome, got {other:?}"),
    }
    server.await.unwrap();
}

/// 401 on the upgrade surfaces as `AuthFailed`.
#[tokio::test]
async fn maps_401_upgrade_to_auth_failed_error() {
    let (addr, server) = spawn_server(
        |_req, _resp| {
            let mut response = tokio_tungstenite::tungstenite::http::Response::new(Some(
                "unauthorized".to_owned(),
            ));
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            Err(response)
        },
        |_ws| async move {},
    )
    .await;

    let base = format!("http://{addr}/graphics/api");
    let err = connect(&base, "w-1", "t").await.unwrap_err();
    assert!(
        matches!(err, WsClientError::AuthFailed { .. }),
        "got {err:?}"
    );
    let _ = server.await;
}

/// Server-side close with code 4001 → typed `AuthFailed` from `recv()`.
#[tokio::test]
async fn surfaces_4001_close_as_typed_auth_failed() {
    let (addr, server) = spawn_server(echo_subprotocol, |mut ws| async move {
        let _ = ws
            .close(Some(CloseFrame {
                code: CloseCode::Library(4001),
                reason: "invalid auth token".into(),
            }))
            .await;
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let mut client = connect(&base, "w-1", "t").await.unwrap();
    // The server may or may not have closed by the time we read; loop
    // until we observe the close-driven error.
    let mut error = None;
    for _ in 0..5 {
        match tokio::time::timeout(TIMEOUT, client.recv()).await {
            Ok(Ok(None)) => break, // graceful EOF without typed error
            Ok(Ok(Some(_))) => continue,
            Ok(Err(e)) => {
                error = Some(e);
                break;
            }
            Err(_) => panic!("recv timed out"),
        }
    }
    let err = error.expect("expected a typed close-driven error");
    assert!(
        matches!(err, WsClientError::AuthFailed { .. }),
        "got {err:?}"
    );
    server.await.unwrap();
}

/// recv() rejects a binary frame from the server.
#[tokio::test]
async fn rejects_binary_frames() {
    let (addr, server) = spawn_server(echo_subprotocol, |mut ws| async move {
        let _ = ws.send(Message::Binary(vec![1, 2, 3].into())).await;
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let mut client = connect(&base, "w-1", "t").await.unwrap();
    let err = client.recv().await.unwrap_err();
    assert!(matches!(err, WsClientError::Protocol(_)), "got {err:?}");
    let _ = server.await;
}

/// recv() returns Ok(None) when the server's stream ends without
/// emitting a close frame (rare, but represents "socket dropped").
#[tokio::test]
async fn returns_none_when_stream_ends_silently() {
    let (addr, server) = spawn_server(echo_subprotocol, |ws| async move {
        drop(ws); // Drop with no Close frame.
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let mut client = connect(&base, "w-1", "t").await.unwrap();
    // The first read may surface a transport error; if so accept that
    // as equivalent to a silent end-of-stream.
    let got_end = match tokio::time::timeout(TIMEOUT, client.recv()).await {
        Ok(Ok(None)) => true,
        Ok(Ok(Some(_))) => panic!("unexpected frame"),
        Ok(Err(WsClientError::ConnectionClosed | WsClientError::Transport(_))) => true,
        Ok(Err(other)) => panic!("unexpected error: {other:?}"),
        Err(_) => panic!("timed out"),
    };
    assert!(got_end);
    // A second recv() after closed must return Ok(None) cleanly.
    let next = client.recv().await.unwrap();
    assert!(next.is_none());
    let _ = server.await;
}

/// close() sends a close frame the server can observe + is idempotent.
#[tokio::test]
async fn close_is_graceful_and_idempotent() {
    let (close_tx, mut close_rx) = tokio::sync::mpsc::unbounded_channel::<u16>();
    let (addr, server) = spawn_server(echo_subprotocol, move |mut ws| {
        let tx = close_tx.clone();
        async move {
            while let Some(item) = ws.next().await {
                match item {
                    Ok(Message::Close(Some(frame))) => {
                        let _ = tx.send(frame.code.into());
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let mut client = connect(&base, "w-1", "t").await.unwrap();
    client.close(1000, "bye").await.unwrap();
    // Second close is a no-op.
    client.close(1011, "again").await.unwrap();
    let code = tokio::time::timeout(Duration::from_secs(1), close_rx.recv())
        .await
        .expect("close frame")
        .expect("channel ok");
    assert_eq!(code, 1000);
    let _ = server.await;
}

/// Debug impl on WsClient shouldn't crash.
#[tokio::test]
async fn debug_impl_renders() {
    let (addr, server) = spawn_server(echo_subprotocol, |mut ws| async move {
        let _ = ws.close(None).await;
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let client = connect(&base, "w-1", "t").await.unwrap();
    let rendered = format!("{client:?}");
    assert!(rendered.contains("WsClient"));
    let _ = server.await;
}

/// Server closes with a non-auth code → returns a generic
/// `ConnectionClosed` so the runtime knows to reconnect.
#[tokio::test]
async fn surfaces_unknown_close_as_connection_closed() {
    let (addr, server) = spawn_server(echo_subprotocol, |mut ws| async move {
        let _ = ws
            .close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "bye".into(),
            }))
            .await;
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let mut client = connect(&base, "w-1", "t").await.unwrap();
    let mut got_eof = false;
    for _ in 0..5 {
        match tokio::time::timeout(TIMEOUT, client.recv()).await {
            Ok(Ok(None)) => {
                got_eof = true;
                break;
            }
            Ok(Ok(Some(_))) => continue,
            Ok(Err(WsClientError::ConnectionClosed)) => {
                got_eof = true;
                break;
            }
            Ok(Err(other)) => panic!("unexpected error: {other:?}"),
            Err(_) => panic!("recv timed out"),
        }
    }
    assert!(got_eof);
    let _ = server.await;
}

// ---------------------------------------------------------------------------
// Split-half contract tests.
//
// Production (`ws::session`) never drives the combined `WsClient`: it calls
// `client.split()` once and then pushes every worker frame through the
// cheap-to-clone `WsSender` (shared by the heartbeat / log-shipper /
// engine-dispatch tasks) while a dedicated reader task drains the
// single-owner `WsReceiver`.  Those split halves are a separate code path
// from the `WsClient::{send,recv,close}` convenience methods the rest of
// this file exercises, so the protocol contract is pinned on them directly
// here — otherwise the methods that actually ship frames in production have
// no live-socket coverage at all.
// ---------------------------------------------------------------------------

/// hello (via `WsSender`) → welcome (via `WsReceiver`) over a live socket —
/// the handshake every session performs after `split()`.
#[tokio::test]
async fn split_round_trips_a_hello_then_a_welcome() {
    let (addr, server) = spawn_server(echo_subprotocol, |mut ws| async move {
        let frame = ws
            .next()
            .await
            .expect("frame")
            .expect("ok")
            .into_text()
            .unwrap();
        assert!(frame.contains("\"hello\""));
        let welcome = serde_json::to_string(
            &json!({"type":"welcome","workerId":"w-split","serverTime":"now"}),
        )
        .unwrap();
        ws.send(Message::Text(welcome.into())).await.unwrap();
        let _ = ws.close(None).await;
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let client = connect(&base, "w-split", "t").await.unwrap();
    let (sender, mut receiver) = client.split();
    sender
        .send(&WorkerInbound::Hello(HelloFrame {
            auth_token: "t".into(),
            capabilities: capabilities(),
        }))
        .await
        .unwrap();
    let received = tokio::time::timeout(TIMEOUT, receiver.recv())
        .await
        .expect("recv timed out")
        .expect("recv ok")
        .expect("got a frame");
    match received {
        WorkerOutbound::Welcome { worker_id, .. } => assert_eq!(worker_id, "w-split"),
        other => panic!("expected welcome, got {other:?}"),
    }
    server.await.unwrap();
}

/// A 4001 server close surfaces through `WsReceiver::recv` as the typed
/// `AuthFailed` — the session keys its "auth rejected, stop reconnecting"
/// branch on exactly this.
#[tokio::test]
async fn split_receiver_surfaces_4001_as_auth_failed() {
    let (addr, server) = spawn_server(echo_subprotocol, |mut ws| async move {
        let _ = ws
            .close(Some(CloseFrame {
                code: CloseCode::Library(4001),
                reason: "invalid auth token".into(),
            }))
            .await;
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let client = connect(&base, "w-split", "t").await.unwrap();
    let (_sender, mut receiver) = client.split();
    let mut error = None;
    for _ in 0..5 {
        match tokio::time::timeout(TIMEOUT, receiver.recv()).await {
            Ok(Ok(None)) => break,
            Ok(Ok(Some(_))) => continue,
            Ok(Err(e)) => {
                error = Some(e);
                break;
            }
            Err(_) => panic!("recv timed out"),
        }
    }
    let err = error.expect("expected a typed close-driven error");
    assert!(
        matches!(err, WsClientError::AuthFailed { .. }),
        "got {err:?}"
    );
    server.await.unwrap();
}

/// A normal server close drives `WsReceiver::recv` to a generic
/// `ConnectionClosed`, after which the receiver is latched: every
/// subsequent `recv()` returns `Ok(None)` without touching the socket.
#[tokio::test]
async fn split_receiver_latches_closed_after_a_normal_close() {
    let (addr, server) = spawn_server(echo_subprotocol, |mut ws| async move {
        let _ = ws
            .close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "bye".into(),
            }))
            .await;
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let client = connect(&base, "w-split", "t").await.unwrap();
    let (_sender, mut receiver) = client.split();
    let mut observed_close = false;
    for _ in 0..5 {
        match tokio::time::timeout(TIMEOUT, receiver.recv()).await {
            Ok(Ok(None)) => {
                observed_close = true;
                break;
            }
            Ok(Ok(Some(_))) => continue,
            Ok(Err(WsClientError::ConnectionClosed)) => {
                observed_close = true;
                break;
            }
            Ok(Err(other)) => panic!("unexpected error: {other:?}"),
            Err(_) => panic!("recv timed out"),
        }
    }
    assert!(observed_close, "expected the receiver to observe the close");
    // A latched receiver returns Ok(None) forever without re-reading.
    assert!(receiver.recv().await.unwrap().is_none());
    let _ = server.await;
}

/// The server drops the socket without a close frame (a yanked cable /
/// crashed DO): `WsReceiver::recv` surfaces it as end-of-stream
/// (`Ok(None)`) or a transport error, then latches to `Ok(None)`.  This
/// is the disconnect the session's reader task turns into a reconnect.
#[tokio::test]
async fn split_receiver_returns_none_on_a_silent_eof() {
    let (addr, server) = spawn_server(echo_subprotocol, |ws| async move {
        drop(ws); // Drop with no Close frame.
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let client = connect(&base, "w-split", "t").await.unwrap();
    let (_sender, mut receiver) = client.split();
    let got_end = match tokio::time::timeout(TIMEOUT, receiver.recv()).await {
        Ok(Ok(None)) => true,
        Ok(Ok(Some(_))) => panic!("unexpected frame"),
        Ok(Err(WsClientError::ConnectionClosed | WsClientError::Transport(_))) => true,
        Ok(Err(other)) => panic!("unexpected error: {other:?}"),
        Err(_) => panic!("recv timed out"),
    };
    assert!(got_end);
    // A second recv() after the stream ended must return Ok(None) cleanly.
    assert!(receiver.recv().await.unwrap().is_none());
    let _ = server.await;
}

/// `WsSender::close` emits a close frame the peer observes — the graceful
/// drain path the session runs on shutdown.
#[tokio::test]
async fn split_sender_close_is_observed_by_the_server() {
    let (close_tx, mut close_rx) = tokio::sync::mpsc::unbounded_channel::<u16>();
    let (addr, server) = spawn_server(echo_subprotocol, move |mut ws| {
        let tx = close_tx.clone();
        async move {
            while let Some(item) = ws.next().await {
                match item {
                    Ok(Message::Close(Some(frame))) => {
                        let _ = tx.send(frame.code.into());
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let client = connect(&base, "w-split", "t").await.unwrap();
    let (sender, _receiver) = client.split();
    sender.close(1000, "bye").await.unwrap();
    let code = tokio::time::timeout(Duration::from_secs(1), close_rx.recv())
        .await
        .expect("close frame")
        .expect("channel ok");
    assert_eq!(code, 1000);
    let _ = server.await;
}

/// A `WsSender::send` that fails surfaces a typed error rather than
/// vanishing.  The session pushes accepts / fails / completeJson /
/// heartbeats through the shared sender with `let _ = sender.send(...)`,
/// discarding the result — so this mapped error (and the warn
/// breadcrumb it drives via `log_send_error`) is the only signal an
/// operator gets that a frame never made it onto the wire.  Closing the
/// sender first puts the sink into its closing handshake, so the next
/// send deterministically errors without racing the socket teardown.
#[tokio::test]
async fn split_sender_send_after_close_surfaces_a_typed_error() {
    let (addr, server) = spawn_server(echo_subprotocol, |mut ws| async move {
        // Hold the connection open and drain until the client goes away.
        while ws.next().await.is_some() {}
    })
    .await;

    let base = format!("http://{addr}/graphics/api");
    let client = connect(&base, "w-split", "t").await.unwrap();
    let (sender, _receiver) = client.split();
    sender.close(1000, "bye").await.unwrap();
    let err = sender
        .send(&WorkerInbound::ReadyForMore)
        .await
        .expect_err("send after close must fail");
    assert!(
        matches!(
            err,
            WsClientError::Transport(_) | WsClientError::ConnectionClosed
        ),
        "a post-close send must map to a transport-class error, got {err:?}"
    );
    let _ = server.await;
}
