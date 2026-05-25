//! Integration tests for the surviving HTTP contract.
//!
//! After the WS migration the only worker-side HTTP routes are
//!  - `POST /workers/register` (bootstrap token)
//!  - `POST /workers/:id/jobs/:jobId/complete` (multipart image / audio
//!    / video bytes; only modality that doesn't fit cleanly into WS
//!    frames).
//!
//! Everything else (heartbeat, accept/reject, completeJson, fail, log
//! batches) is now WS frame traffic and is covered in
//! `ws_client_contract.rs` + the orchestrator unit tests on the API
//! side.
use studio_worker::http::ApiClient;
use studio_worker::types::*;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn caps() -> WorkerCapabilities {
    WorkerCapabilities {
        machine_name: "test-machine".into(),
        username: "tester".into(),
        agent_version: "0.0.0-test".into(),
        engine: "synthetic".into(),
        vram_total_gb: 0.0,
        vram_threshold_gb: 64.0,
        auto_enabled: true,
        auto_start: false,
        supported_models: vec!["synthetic".into()],
        task_kinds: vec![TaskKind::Image],
        supported_models_per_kind: [(TaskKind::Image, vec!["synthetic".into()])]
            .into_iter()
            .collect(),
    }
}

/// Run a blocking closure outside the current tokio runtime.
///
/// `reqwest::blocking::Client` spins up its own internal runtime, which
/// panics on drop if it's called from within an enclosing tokio context.
/// Putting the call on a real OS thread sidesteps that.
fn detached<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::spawn(f)
        .join()
        .expect("worker thread panicked")
}

#[tokio::test]
async fn register_returns_worker_id_and_auth_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register"))
        .and(header("authorization", "Bearer boot-token"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({ "workerId": "w-1", "authToken": "tok-xyz" })),
        )
        .mount(&server)
        .await;

    let uri = server.uri();
    let response = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.register("boot-token", caps(), None).unwrap()
    });
    assert_eq!(response.worker_id, "w-1");
    assert_eq!(response.auth_token, "tok-xyz");
}

#[tokio::test]
async fn complete_posts_multipart_image() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/jobs/job-42/complete"))
        .and(header("authorization", "Bearer tok-xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete(
            "w-1",
            "tok-xyz",
            "job-42",
            "webp",
            "a prompt",
            vec![0xff, 0xd8, 0xff],
        )
        .unwrap();
    });
}

#[tokio::test]
async fn complete_handles_audio_wav() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/jobs/job-42/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete(
            "w-1",
            "tok-xyz",
            "job-42",
            "wav",
            "a tts prompt",
            vec![0x52, 0x49, 0x46, 0x46],
        )
        .unwrap();
    });
}
