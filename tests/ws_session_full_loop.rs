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
//! 5. close cleanly with 1000 → the worker session loop sees
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

        // Both offers carry a synthetic ModelSource so the no-fallback
        // gate is satisfied (every offer needs `task` + `modelSource`).
        let synthetic_source = json!({
            "engine": "synthetic",
            "files": [],
            "cliDefaults": {
                "cfgScale": 1.0,
                "steps": 8,
                "width": 1024,
                "height": 1024
            }
        });

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
                    "task": {
                        "kind": "llm",
                        "messages": [{"role": "user", "content": "hi"}],
                        "maxTokens": 4,
                        "temperature": 0.5
                    },
                    "modelSource": synthetic_source
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
                    "task": {
                        "kind": "audio_stt",
                        "inputUrl": "https://example/audio.wav",
                        "language": null
                    },
                    "modelSource": synthetic_source
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

/// Minimal studio server: accept the upgrade, read `hello`, reply
/// `welcome`, then close.  Enough to drive the worker through the
/// capability-advertising handshake without any job offers.
async fn spawn_handshake_only_ws() -> (SocketAddr, tokio::task::JoinHandle<Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, echo_subprotocol).await?;
        let hello = ws
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("hello missing"))??
            .into_text()
            .map_err(|_| anyhow::anyhow!("hello not text"))?;
        let hello_json: serde_json::Value = serde_json::from_str(&hello)?;
        assert_eq!(hello_json["type"], "hello");
        ws.send(Message::Text(
            serde_json::to_string(
                &json!({"type":"welcome","workerId":"w-test","serverTime":"now"}),
            )?
            .into(),
        ))
        .await?;
        ws.close(None).await?;
        Ok(())
    });
    (addr, handle)
}

/// A studio that completes the handshake then goes SILENT: it never acks heartbeats and never
/// closes the socket. This is the half-open / dead-peer case that used to hang the worker forever
/// (its reader blocks on `source.next()` and nothing tears the session down). After accepting the
/// one connection it drops the listener so the worker's reconnect attempt fails fast (refused).
async fn spawn_silent_after_welcome_ws() -> (SocketAddr, tokio::task::JoinHandle<Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        // Refuse any further connection so the worker's reconnect fails immediately instead of
        // hanging on a second silent upgrade.
        drop(listener);
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, echo_subprotocol).await?;
        let _hello = ws
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("hello missing"))??;
        ws.send(Message::Text(
            serde_json::to_string(
                &json!({"type":"welcome","workerId":"w-test","serverTime":"now"}),
            )?
            .into(),
        ))
        .await?;
        // Go silent: hold the connection open, never ack, never close.
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(ws);
        Ok(())
    });
    (addr, handle)
}

/// Minimal studio that acks every heartbeat and forwards each
/// heartbeat's advertised `vramThresholdGb` to the test through an
/// unbounded channel, so a test can observe what the worker is
/// advertising tick by tick.
async fn spawn_heartbeat_collecting_ws() -> (
    SocketAddr,
    tokio::sync::mpsc::UnboundedReceiver<f64>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, echo_subprotocol).await?;

        let hello = ws
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("hello missing"))??
            .into_text()
            .map_err(|_| anyhow::anyhow!("hello not text"))?;
        let hello_json: serde_json::Value = serde_json::from_str(&hello)?;
        assert_eq!(hello_json["type"], "hello");
        ws.send(Message::Text(
            serde_json::to_string(
                &json!({"type":"welcome","workerId":"w-test","serverTime":"now"}),
            )?
            .into(),
        ))
        .await?;

        while let Some(item) = ws.next().await {
            let msg = match item {
                Ok(m) => m,
                Err(_) => break,
            };
            match msg {
                Message::Text(t) => {
                    let frame: serde_json::Value = serde_json::from_str(&t)?;
                    if frame["type"] == "heartbeat" {
                        let threshold = frame["capabilities"]["vramThresholdGb"]
                            .as_f64()
                            .unwrap_or(-1.0);
                        if tx.send(threshold).is_err() {
                            break;
                        }
                        ws.send(Message::Text(
                            serde_json::to_string(&json!({"type":"heartbeatAck"}))?.into(),
                        ))
                        .await?;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        Ok(())
    });
    (addr, rx, handle)
}

/// Poll `pred` every 5ms until it returns true or `timeout` elapses.
/// Used to wait on an operator log line landing in `recent_logs`
/// without racing the heartbeat pump's tick cadence.
async fn wait_until<F: Fn() -> bool>(timeout: Duration, pred: F) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    pred()
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
        auto_update_enabled: false,
        ws_reconnect_attempts: Some(1),
        ..Config::default()
    };
    let shared = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::<LogEntry>::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));

    let session_handle = tokio::spawn({
        let shared = shared.clone();
        let stop = stop.clone();
        let logs = logs.clone();
        let busy = busy.clone();
        let paused = paused.clone();
        async move {
            spawn_ws_session(
                shared,
                stop,
                logs,
                busy,
                paused,
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

#[tokio::test]
async fn ws_session_logs_a_breadcrumb_when_json_result_is_sent() {
    // The binary-output path already logs "binary upload ok" on a
    // successful multipart upload.  The JSON (LLM / STT) path must be
    // symmetric: a successfully sent `completeJson` frame has to leave
    // an explicit completion breadcrumb so operators (and the studio's
    // shipped logs) can tell a JSON job actually delivered its result,
    // not just that it dispatched.
    let (ws_addr, server_handle) = spawn_studio_ws().await;

    let cfg = Config {
        api_base_url: format!("http://{ws_addr}"),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        auto_update_enabled: false,
        ws_reconnect_attempts: Some(1),
        ..Config::default()
    };
    let shared = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::<LogEntry>::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    // `recent_logs` is *not* drained by the log shipper, so the
    // completion breadcrumb stays inspectable after the run.
    let observers = WorkerObservers::default();

    let session_handle = tokio::spawn({
        let shared = shared.clone();
        let stop = stop.clone();
        let logs = logs.clone();
        let busy = busy.clone();
        let paused = paused.clone();
        let observers = observers.clone();
        async move {
            spawn_ws_session(
                shared,
                stop,
                logs,
                busy,
                paused,
                observers,
                SessionSchedule::fast_for_tests(),
            )
            .await
        }
    });

    tokio::time::timeout(TIMEOUT, server_handle)
        .await
        .expect("server timed out")
        .expect("server task panicked")
        .expect("server returned err");

    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = tokio::time::timeout(Duration::from_secs(5), session_handle)
        .await
        .expect("session loop timed out");

    let json_completion = observers
        .recent_logs
        .lock()
        .iter()
        .filter(|e| e.message.contains("json result sent"))
        .map(|e| (e.level.clone(), e.job_id.clone()))
        .collect::<Vec<_>>();
    // Both the LLM and the STT offer travel the JSON path, so both
    // must leave an info-level breadcrumb tagged with their job id.
    assert_eq!(
        json_completion.len(),
        2,
        "expected a completion breadcrumb per JSON job, got {json_completion:?}"
    );
    assert!(
        json_completion.iter().all(|(level, _)| level == "info"),
        "json completion breadcrumbs must be info-level: {json_completion:?}"
    );
    assert!(
        json_completion
            .iter()
            .any(|(_, job_id)| job_id.as_deref() == Some("job-llm")),
        "missing breadcrumb for the LLM job: {json_completion:?}"
    );
    assert!(
        json_completion
            .iter()
            .any(|(_, job_id)| job_id.as_deref() == Some("job-stt")),
        "missing breadcrumb for the STT job: {json_completion:?}"
    );
}

#[tokio::test]
async fn ws_session_heartbeats_pick_up_live_config_changes() {
    // Regression: the capability snapshot used to be built once per
    // session, so an operator saving a new VRAM threshold from the UI
    // Config tab (or `set-threshold` on a shared config) was never
    // advertised to the studio until the next reconnect — and the
    // studio's pickWorkerForJob kept filtering on the stale budget.
    let (ws_addr, mut thresholds, _server) = spawn_heartbeat_collecting_ws().await;

    let cfg = Config {
        api_base_url: format!("http://{ws_addr}"),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        vram_threshold_gb: 12.0,
        auto_update_enabled: false,
        ws_reconnect_attempts: Some(1),
        ..Config::default()
    };
    let shared = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::<LogEntry>::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));

    let session_handle = tokio::spawn({
        let shared = shared.clone();
        let stop = stop.clone();
        let logs = logs.clone();
        let busy = busy.clone();
        let paused = paused.clone();
        async move {
            spawn_ws_session(
                shared,
                stop,
                logs,
                busy,
                paused,
                WorkerObservers::default(),
                SessionSchedule::fast_for_tests(),
            )
            .await
        }
    });

    // The first heartbeat advertises the boot-time threshold.
    let first = tokio::time::timeout(TIMEOUT, thresholds.recv())
        .await
        .expect("timed out waiting for the first heartbeat")
        .expect("server channel closed before the first heartbeat");
    assert_eq!(first, 12.0);

    // Operator saves a new threshold mid-session.
    shared.lock().vram_threshold_gb = 20.0;

    // A subsequent heartbeat must advertise the new value without a
    // reconnect.
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let v = thresholds
                .recv()
                .await
                .expect("server channel closed before the updated heartbeat");
            if v == 20.0 {
                break;
            }
        }
    })
    .await
    .expect("no heartbeat carried the updated vram threshold within the timeout");

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = tokio::time::timeout(Duration::from_secs(5), session_handle)
        .await
        .expect("session loop timed out");
}

#[tokio::test]
async fn ws_session_recovers_from_a_silent_half_open_connection() {
    // Regression: the worker used to hang forever when the studio went silent without closing the
    // socket (post-job WS drops + half-open peers). The read-idle-timeout must detect the silence,
    // tear the session down, and drive a reconnect — proven here by the session loop actually
    // returning (it hits its 1-attempt cap on the refused reconnect) instead of blocking.
    let (ws_addr, _server) = spawn_silent_after_welcome_ws().await;

    let cfg = Config {
        api_base_url: format!("http://{ws_addr}"),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        auto_update_enabled: false,
        ws_reconnect_attempts: Some(1),
        ..Config::default()
    };
    let shared = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::<LogEntry>::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let observers = WorkerObservers::default();

    // Tiny read-idle-timeout so the silent connection is detected fast.
    let schedule = SessionSchedule {
        read_idle_timeout: Duration::from_millis(300),
        ..SessionSchedule::fast_for_tests()
    };

    let session_handle = tokio::spawn({
        let shared = shared.clone();
        let stop = stop.clone();
        let logs = logs.clone();
        let busy = busy.clone();
        let paused = paused.clone();
        let observers = observers.clone();
        async move { spawn_ws_session(shared, stop, logs, busy, paused, observers, schedule).await }
    });

    // Before the fix this times out: the worker blocks on the dead-but-open socket and the session
    // loop never returns. After the fix the reader idle-times-out, reconnects (refused), hits the
    // 1-attempt cap, and the loop returns Err within the window.
    let outcome = tokio::time::timeout(Duration::from_secs(8), session_handle).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        outcome.is_ok(),
        "session hung on a silent connection instead of detecting the read-idle-timeout"
    );

    assert!(
        observers
            .recent_logs
            .lock()
            .iter()
            .any(|e| e.message.contains("reconnect attempt")),
        "worker must log a reconnect attempt after the idle timeout"
    );
}

#[tokio::test]
async fn ws_session_logs_advertised_capabilities_on_handshake() {
    let (ws_addr, server_handle) = spawn_handshake_only_ws().await;

    let cfg = Config {
        api_base_url: format!("http://{ws_addr}"),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        auto_update_enabled: false,
        ws_reconnect_attempts: Some(1),
        ..Config::default()
    };
    let shared = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::<LogEntry>::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    // The session pushes operator log lines through `recent_logs`,
    // which — unlike `logs` — is *not* drained by the log shipper, so
    // the capability summary stays inspectable after the run.
    let observers = WorkerObservers::default();

    let session_handle = tokio::spawn({
        let shared = shared.clone();
        let stop = stop.clone();
        let logs = logs.clone();
        let busy = busy.clone();
        let paused = paused.clone();
        let observers = observers.clone();
        async move {
            spawn_ws_session(
                shared,
                stop,
                logs,
                busy,
                paused,
                observers,
                SessionSchedule::fast_for_tests(),
            )
            .await
        }
    });

    tokio::time::timeout(TIMEOUT, server_handle)
        .await
        .expect("server timed out")
        .expect("server task panicked")
        .expect("server returned err");

    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = tokio::time::timeout(Duration::from_secs(5), session_handle)
        .await
        .expect("session loop timed out");

    // The handshake must have recorded what the worker advertised: the
    // engine, the served task kinds, and at least one model id.
    let summary = observers
        .recent_logs
        .lock()
        .iter()
        .find(|e| e.message.contains("advertising engine="))
        .map(|e| e.message.clone())
        .expect("capability summary must be logged on the handshake");
    assert!(summary.contains("kinds=["), "missing kinds: {summary}");
    assert!(summary.contains("image"), "missing image kind: {summary}");
    assert!(summary.contains("synthetic"), "missing model id: {summary}");
    assert!(
        summary.contains("auto_enabled=true"),
        "unpaused worker must advertise auto_enabled=true: {summary}"
    );
}

#[tokio::test]
async fn ws_session_ships_pause_and_resume_transitions_to_operator_logs() {
    // A Pause / Resume from the Status tab or tray menu only emits a
    // local `tracing` breadcrumb (stdout / Sentry); it never enters the
    // worker's shipped log stream.  So the studio's shipped-log view and
    // the UI's Logs tab used to show `auto_enabled=false` heartbeats
    // with no record of *why* the worker stopped claiming.  The
    // heartbeat pump must observe the runtime pause flag and ship the
    // transition, so a toggle from any source is visible to operators.
    let (ws_addr, mut thresholds, _server) = spawn_heartbeat_collecting_ws().await;

    let cfg = Config {
        api_base_url: format!("http://{ws_addr}"),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        auto_update_enabled: false,
        ws_reconnect_attempts: Some(1),
        ..Config::default()
    };
    let shared = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::<LogEntry>::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    // `recent_logs` is *not* drained by the log shipper, so the
    // transition breadcrumbs stay inspectable after they ship.
    let observers = WorkerObservers::default();

    let session_handle = tokio::spawn({
        let shared = shared.clone();
        let stop = stop.clone();
        let logs = logs.clone();
        let busy = busy.clone();
        let paused = paused.clone();
        let observers = observers.clone();
        async move {
            spawn_ws_session(
                shared,
                stop,
                logs,
                busy,
                paused,
                observers,
                SessionSchedule::fast_for_tests(),
            )
            .await
        }
    });

    // The session is welcomed + heartbeating once the first heartbeat
    // lands, so a pause flipped after this is guaranteed to be observed
    // by a running pump.
    tokio::time::timeout(TIMEOUT, thresholds.recv())
        .await
        .expect("timed out waiting for the first heartbeat")
        .expect("server channel closed before the first heartbeat");

    // Operator pauses: the pump must ship the transition.
    paused.store(true, std::sync::atomic::Ordering::SeqCst);
    let paused_logged = wait_until(TIMEOUT, || {
        observers
            .recent_logs
            .lock()
            .iter()
            .any(|e| e.level == "info" && e.message.contains("claiming paused by operator"))
    })
    .await;
    assert!(
        paused_logged,
        "a pause must reach the operator-facing log stream"
    );

    // Operator resumes: the next transition must ship too.
    paused.store(false, std::sync::atomic::Ordering::SeqCst);
    let resumed_logged = wait_until(TIMEOUT, || {
        observers
            .recent_logs
            .lock()
            .iter()
            .any(|e| e.level == "info" && e.message.contains("claiming resumed by operator"))
    })
    .await;
    assert!(
        resumed_logged,
        "a resume must reach the operator-facing log stream"
    );

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = tokio::time::timeout(Duration::from_secs(5), session_handle).await;
}
