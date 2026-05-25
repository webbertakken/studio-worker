#![allow(clippy::result_large_err)]
//! End-to-end "full loop" test for the WS session.
//!
//! Boots a tokio-tungstenite server that mimics the DO's protocol and
//! drives the real `spawn_ws_session` through the worker-side
//! lifecycle for JSON-result offers (LLM and STT, both deterministic
//! synthetic engines).
//!
//! 1. accept upgrade with the studio sub-protocol
//! 2. wait for `hello`, reply with `welcome`
//! 3. send an LLM `offer`, expect `accept` + `completeJson`
//! 4. send an STT `offer`, expect `accept` + `completeJson`
//! 5. close cleanly with 1000 \u2192 the worker session loop sees
//!    `Disconnected`, hits its 1-attempt reconnect cap, and exits.
//!
//! The multipart `complete` HTTP path is covered separately by
//! `tests/http_contract.rs`; mixing it in here would need a single
//! server bound to both protocols on one port and would obscure the
//! WS contract these tests focus on.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::json;
use studio_worker::config::{self, Config};
use studio_worker::runtime::WorkerObservers;
use studio_worker::types::LogEntry;
use studio_worker::ws::session::{spawn_ws_session, SessionSchedule};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const TIMEOUT: Duration = Duration::from_secs(10);

fn echo_subprotocol(_req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
    resp.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("studio-worker-v1"),
    );
    Ok(resp)
}

async fn spawn_studio_ws() -> (SocketAddr, tokio::task::JoinHandle<Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, echo_subprotocol).await?;

        // 1. hello.
        let hello = ws
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("hello missing"))??
            .into_text()
            .map_err(|_| anyhow::anyhow!("hello not text"))?;
        let hello_json: serde_json::Value = serde_json::from_str(&hello)?;
        assert_eq!(hello_json["type"], "hello");

        // 2. welcome.
        ws.send(Message::Text(
            serde_json::to_string(
                &json!({"type":"welcome","workerId":"w-test","serverTime":"now"}),
            )?
            .into(),
        ))
        .await?;

        // 3. LLM offer.
        ws.send(Message::Text(
            serde_json::to_string(&json!({
                "type": "offer",
                "claim": {
                    "jobId": "job-llm",
                    "gameId": "g",
                    "assetName": "g/dialogue/scribe",
                    "model": "synthetic",
                    "vramGbEstimate": 1.0,
                    "prompt": "",
                    "ext": "json",
                    "task": {
                        "kind": "llm",
                        "messages": [{"role": "user", "content": "hi"}],
                        "max_tokens": 4,
                        "temperature": 0.5
                    }
                }
            }))?
            .into(),
        ))
        .await?;
        let frames = collect_frames(&mut ws, &["accept", "completeJson"]).await?;
        assert_eq!(frames["accept"]["jobId"], "job-llm");
        assert_eq!(frames["completeJson"]["jobId"], "job-llm");

        // 4. STT offer.
        ws.send(Message::Text(
            serde_json::to_string(&json!({
                "type": "offer",
                "claim": {
                    "jobId": "job-stt",
                    "gameId": "g",
                    "assetName": "g/dialogue/transcript",
                    "model": "synthetic",
                    "vramGbEstimate": 1.0,
                    "prompt": "",
                    "ext": "json",
                    "task": {
                        "kind": "audio_stt",
                        "input_url": "https://example/audio.wav",
                        "language": null
                    }
                }
            }))?
            .into(),
        ))
        .await?;
        let frames = collect_frames(&mut ws, &["accept", "completeJson"]).await?;
        assert_eq!(frames["accept"]["jobId"], "job-stt");
        assert_eq!(frames["completeJson"]["jobId"], "job-stt");

        // 5. clean close.
        ws.close(None).await?;
        Ok(())
    });
    (addr, handle)
}

async fn collect_frames(
    ws: &mut WebSocketStream<TcpStream>,
    expected: &[&str],
) -> Result<HashMap<String, serde_json::Value>> {
    let mut bucket: HashMap<String, serde_json::Value> = HashMap::new();
    while bucket.len() < expected.len() {
        let item = tokio::time::timeout(TIMEOUT, ws.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("stream ended early"))??;
        if let Message::Text(t) = item {
            let frame: serde_json::Value = serde_json::from_str(&t)?;
            if let Some(kind) = frame["type"].as_str() {
                if expected.contains(&kind) {
                    bucket.insert(kind.to_string(), frame);
                }
            }
        }
    }
    Ok(bucket)
}

#[tokio::test]
async fn ws_session_walks_through_two_json_offers_and_then_disconnects() {
    let (ws_addr, server_handle) = spawn_studio_ws().await;

    let cfg = Config {
        api_base_url: format!("http://{ws_addr}"),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        engine: "synthetic".into(),
        auto_enabled: true,
        auto_update_enabled: false,
        ws_reconnect_attempts: Some(1),
        ..Config::default()
    };
    let shared = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::<LogEntry>::new()));
    let busy = Arc::new(AtomicBool::new(false));

    let session_handle = tokio::spawn({
        let shared = shared.clone();
        let stop = stop.clone();
        let logs = logs.clone();
        let busy = busy.clone();
        async move {
            spawn_ws_session(
                shared,
                stop,
                logs,
                busy,
                WorkerObservers::default(),
                SessionSchedule::fast_for_tests(),
            )
            .await
        }
    });

    // The fake server walks through the protocol then closes.
    tokio::time::timeout(TIMEOUT, server_handle)
        .await
        .expect("server timed out")
        .expect("server task panicked")
        .expect("server returned err");

    // Give the session a beat to observe the close, then signal stop so
    // the reconnect loop doesn't try a second connection to the closed
    // listener.
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = tokio::time::timeout(Duration::from_secs(5), session_handle)
        .await
        .expect("session loop timed out");

    // Log assertions are unreliable here because spawn_log_shipper_pump
    // drains the buffer to send `logBatch` frames over the WS as the
    // session runs.  The server-side assertions in `collect_frames`
    // already verify the full protocol round-trip end-to-end: if those
    // returned Ok then every frame in the script reached the worker and
    // every expected reply (accept + completeJson per offer) flowed back.
    let _ = logs;
}
