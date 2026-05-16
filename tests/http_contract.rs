//! Integration tests for the API contract.
//!
//! Spins up a wiremock server that pretends to be the studio API and
//! exercises every endpoint the worker hits.  No GPU, no real studio,
//! no network — fully reproducible on GitHub Actions.

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
async fn heartbeat_sends_caps_with_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/heartbeat"))
        .and(header("authorization", "Bearer tok-xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.heartbeat("w-1", "tok-xyz", caps(), None).unwrap();
    });
}

#[tokio::test]
async fn claim_returns_none_on_204() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/claim"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let uri = server.uri();
    let result = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.claim("w-1", "tok-xyz").unwrap()
    });
    assert!(result.is_none());
}

#[tokio::test]
async fn claim_returns_job_on_200() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobId": "job-42",
            "gameId": "game-of-elements",
            "assetName": "game-of-elements/creatures/t2-golem-stone",
            "model": "synthetic",
            "vramGbEstimate": 12.0,
            "prompt": "a stone golem in cinematic light",
            "ext": "webp"
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let job = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.claim("w-1", "tok-xyz")
            .unwrap()
            .expect("expected a job")
    });
    assert_eq!(job.job_id, "job-42");
    assert_eq!(job.model, "synthetic");
    assert_eq!(job.ext, "webp");
    assert!((job.vram_gb_estimate - 12.0).abs() < 0.001);
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
async fn fail_posts_error_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/jobs/job-42/fail"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.fail("w-1", "tok-xyz", "job-42", "boom", true).unwrap();
    });
}

#[tokio::test]
async fn ship_logs_posts_batch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        let batch = LogBatch {
            entries: vec![LogEntry {
                ts: "2026-05-16T00:00:00.000Z".into(),
                level: "info".into(),
                category: "generate".into(),
                message: "ok".into(),
                job_id: Some("job-42".into()),
            }],
        };
        api.ship_logs("w-1", "tok-xyz", batch).unwrap();
    });
}
