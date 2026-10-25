#![allow(clippy::result_large_err)]
//! A studio `llm` offer carrying `images` (the Glimpse pre-screen's shape), end to end through
//! the real WS session, GPU-free:
//!
//! 1. a model whose source carries a vision projector reads the pictures, fetched from the
//!    studio's job-input URLs (wiremock), and the answer comes back on `completeJson`;
//! 2. a model without a projector refuses the task with a permanent `fail`, so no answer is ever
//!    given about pictures the model did not see.
//!
//! The synthetic engine stands in for the vision model: it reads every picture and says what it
//! saw.
use std::collections::HashMap;
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
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TIMEOUT: Duration = Duration::from_secs(10);
const WEBP: &[u8] = include_bytes!("../fixtures/lossy-vp8.webp");

fn echo_subprotocol(_req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
    resp.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("studio-worker-v1"),
    );
    Ok(resp)
}

fn source(with_projector: bool) -> serde_json::Value {
    let mut files = vec![json!({
        "role": "model",
        "url": "https://models.invalid/m.gguf",
        "filename": "m.gguf"
    })];
    if with_projector {
        files.push(json!({
            "role": "text-encoder-vision",
            "url": "https://models.invalid/m-mmproj.gguf",
            "filename": "m-mmproj.gguf"
        }));
    }
    json!({
        "engine": "synthetic",
        "files": files,
        "cliDefaults": { "cfgScale": 1.0, "steps": 8, "width": 1024, "height": 1024 }
    })
}

fn screening_offer(job_id: &str, images: Vec<String>, with_projector: bool) -> Message {
    let frame = json!({
        "type": "offer",
        "claim": {
            "jobId": job_id,
            "gameId": "glimpse",
            "assetName": "glimpse/screen",
            "model": "vision-model",
            "vramGbEstimate": 1.0,
            "task": {
                "kind": "llm",
                "messages": [{"role": "user", "content": "Image 1 is the full picture."}],
                "jsonSchema": {"type": "object"},
                "images": images
            },
            "modelSource": source(with_projector)
        }
    });
    Message::Text(frame.to_string().into())
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
async fn a_screening_offer_is_read_by_a_vision_model_and_refused_by_any_other() {
    let studio_http = MockServer::start().await;
    for index in 0..2 {
        Mock::given(method("GET"))
            .and(path(format!("/glimpse/api/job-input/p-1/job-vl/{index}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/webp")
                    .set_body_bytes(WEBP),
            )
            .mount(&studio_http)
            .await;
    }
    let images: Vec<String> = (0..2)
        .map(|i| format!("{}/glimpse/api/job-input/p-1/job-vl/{i}", studio_http.uri()))
        .collect();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, echo_subprotocol).await?;
        let _hello = ws
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("no hello"))??;
        ws.send(Message::Text(
            json!({"type":"welcome","workerId":"w-test","serverTime":"now"})
                .to_string()
                .into(),
        ))
        .await?;

        ws.send(screening_offer("job-vl", images.clone(), true))
            .await?;
        let read = collect_frames(&mut ws, &["accept", "completeJson"]).await?;
        let answer = read["completeJson"]["result"]["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        ws.send(screening_offer("job-text", images, false)).await?;
        let refused = collect_frames(&mut ws, &["accept", "fail"]).await?;

        ws.close(None).await?;
        Ok::<_, anyhow::Error>((answer, refused["fail"].clone()))
    });

    let cfg = Config {
        api_base_url: format!("http://{addr}"),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        auto_update_enabled: false,
        ws_reconnect_attempts: Some(1),
        ..Config::default()
    };
    let stop = Arc::new(AtomicBool::new(false));
    let session = tokio::spawn({
        let stop = stop.clone();
        async move {
            spawn_ws_session(
                config::shared(cfg),
                stop,
                Arc::new(Mutex::new(Vec::<LogEntry>::new())),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                WorkerObservers::default(),
                SessionSchedule::fast_for_tests(),
            )
            .await
        }
    });

    let (answer, fail) = tokio::time::timeout(TIMEOUT, server)
        .await
        .expect("studio timed out")
        .expect("studio task panicked")
        .expect("studio script failed");
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = tokio::time::timeout(Duration::from_secs(5), session).await;

    assert!(answer.contains("(read 2 images: "), "{answer}");
    assert_eq!(fail["jobId"], "job-text");
    assert_eq!(fail["retryable"], false, "{fail}");
    assert!(
        fail["error"]
            .as_str()
            .unwrap_or_default()
            .contains("reads no images"),
        "{fail}"
    );
}
