//! HTTP error-path coverage for the surviving `ApiClient` surface.
//!
//! After the WS migration only `register` and `complete` (multipart)
//! remain on the HTTP side; this file pushes the failure branches
//! on both routes plus the tracing emission contract.

use studio_worker::http::ApiClient;
use studio_worker::test_support::capture as captured_logs_for;
use studio_worker::types::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn caps() -> WorkerCapabilities {
    WorkerCapabilities {
        machine_name: "t".into(),
        username: "u".into(),
        agent_version: "0".into(),
        engine: "synthetic".into(),
        vram_total_gb: 0.0,
        vram_threshold_gb: 0.0,
        auto_enabled: true,
        auto_start: false,
        supported_models: vec![],
        task_kinds: vec![],
        supported_models_per_kind: Default::default(),
    }
}

fn detached<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::spawn(f)
        .join()
        .expect("worker thread panicked")
}

#[tokio::test]
async fn complete_surfaces_4xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(409).set_body_string("conflict"))
        .mount(&server)
        .await;
    let uri = server.uri();
    let err = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete("w", "t", "j", "webp", "p", vec![1, 2, 3])
            .unwrap_err()
    });
    assert!(err.to_string().contains("complete failed"));
}

#[tokio::test]
async fn complete_with_retry_survives_transient_5xx() {
    // Two 5xx blips then success: the upload must complete instead of
    // failing the job back to the studio (which costs a full GPU
    // regeneration).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(500).set_body_string("blip"))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .expect(1)
        .mount(&server)
        .await;
    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete_with_retry(
            "w",
            "t",
            "j",
            "webp",
            "p",
            vec![1, 2, 3],
            2,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    });
}

#[tokio::test]
async fn complete_with_retry_gives_up_after_the_retry_budget() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(500).set_body_string("down"))
        .expect(3) // 1 initial + 2 retries
        .mount(&server)
        .await;
    let uri = server.uri();
    let err = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete_with_retry(
            "w",
            "t",
            "j",
            "webp",
            "p",
            vec![1, 2, 3],
            2,
            std::time::Duration::from_millis(1),
        )
        .unwrap_err()
    });
    assert!(err.to_string().contains("complete failed"), "got: {err}");
}

#[tokio::test]
async fn complete_with_retry_does_not_retry_4xx() {
    // A 4xx is a contract error (e.g. job no longer claimed by this
    // worker) — retrying would just hammer the studio with a request
    // it has already refused.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(409).set_body_string("conflict"))
        .expect(1)
        .mount(&server)
        .await;
    let uri = server.uri();
    let err = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete_with_retry(
            "w",
            "t",
            "j",
            "webp",
            "p",
            vec![1, 2, 3],
            2,
            std::time::Duration::from_millis(1),
        )
        .unwrap_err()
    });
    assert!(err.to_string().contains("complete failed"), "got: {err}");
}

#[tokio::test]
async fn complete_for_mp3_uses_audio_mime() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete("w", "t", "j", "mp3", "p", vec![1, 2, 3])
            .unwrap();
    });
}

#[tokio::test]
async fn complete_for_mp4_uses_video_mime() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete("w", "t", "j", "mp4", "p", vec![1, 2, 3])
            .unwrap();
    });
}

#[tokio::test]
async fn complete_for_unknown_ext_falls_back_to_octet_stream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete("w", "t", "j", "bin", "p", vec![1, 2, 3])
            .unwrap();
    });
}

#[tokio::test]
async fn complete_logs_upload_byte_size() {
    // The multipart `complete` route is the only path that ships the
    // (potentially large) binary result.  Operators debugging slow or
    // failed result delivery need to see how many bytes the worker
    // tried to upload, so `complete` emits the payload size before the
    // fallible request regardless of its outcome.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    let uri = server.uri();
    let logs = captured_logs_for(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete("w", "t", "j", "webp", "p", vec![1, 2, 3, 4, 5])
            .unwrap();
    });
    assert!(
        logs.contains("op=\"complete\""),
        "expected complete op field: {logs}"
    );
    assert!(
        logs.contains("bytes=5"),
        "expected upload byte-size field, got: {logs}"
    );
}

#[tokio::test]
async fn complete_logs_upload_byte_size_even_on_failure() {
    // The pre-upload breadcrumb fires before the request, so a failed
    // upload still leaves the attempted payload size in the logs.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let uri = server.uri();
    let logs = captured_logs_for(move || {
        let api = ApiClient::new(uri).unwrap();
        let _ = api.complete("w", "t", "j", "webp", "p", vec![1, 2, 3, 4, 5, 6, 7]);
    });
    assert!(
        logs.contains("bytes=7"),
        "expected attempted upload byte-size field on failure, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// Tracing emission — every HTTP call leaves an operator-visible
// breadcrumb (debug on success, warn on failure) with endpoint, status,
// elapsed time, and the op label.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn successful_call_emits_debug_tracing_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register-request"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "requestId": "rr-1",
            "status": "pending",
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let logs = captured_logs_for(move || {
        let api = ApiClient::new(uri).unwrap();
        api.register_request(&AutoRegisterRequest {
            install_id: "id".into(),
            registration_secret_hash: "h".into(),
            capabilities: caps(),
            user_agent: "studio-worker/test".into(),
        })
        .unwrap();
    });

    assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
    assert!(
        logs.contains("/graphics/api/workers/register-request"),
        "expected endpoint in log, got: {logs}"
    );
    assert!(
        logs.contains("op=\"register-request\""),
        "expected op field: {logs}"
    );
    assert!(logs.contains("status=200"), "expected status field: {logs}");
    assert!(logs.contains("elapsed_ms"), "expected elapsed_ms: {logs}");
}

#[tokio::test]
async fn failing_call_emits_warn_tracing_event_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register-request"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream-unavailable"))
        .mount(&server)
        .await;

    let uri = server.uri();
    let logs = captured_logs_for(move || {
        let api = ApiClient::new(uri).unwrap();
        let _ = api.register_request(&AutoRegisterRequest {
            install_id: "id".into(),
            registration_secret_hash: "h".into(),
            capabilities: caps(),
            user_agent: "studio-worker/test".into(),
        });
    });

    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"register-request\""),
        "expected op field: {logs}"
    );
    assert!(logs.contains("status=503"), "expected status field: {logs}");
    assert!(
        logs.contains("upstream-unavailable"),
        "expected response body in log, got: {logs}"
    );
}
