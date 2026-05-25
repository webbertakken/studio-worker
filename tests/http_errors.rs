//! HTTP error-path coverage for `ApiClient`.  Every endpoint has a
//! happy-path test in `http_contract.rs`; this file pushes the failure
//! branches.

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
async fn register_surfaces_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let uri = server.uri();
    let err = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.register("tok", caps(), None).unwrap_err()
    });
    assert!(err.to_string().contains("register failed"));
}

#[tokio::test]
async fn heartbeat_surfaces_4xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/heartbeat"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;
    let uri = server.uri();
    let err = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.heartbeat("w", "t", caps(), None).unwrap_err()
    });
    assert!(err.to_string().contains("heartbeat failed"));
}

#[tokio::test]
async fn claim_surfaces_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/claim"))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .mount(&server)
        .await;
    let uri = server.uri();
    let err = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.claim("w", "t").unwrap_err()
    });
    assert!(err.to_string().contains("claim failed"));
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
async fn complete_json_round_trips_to_completion_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete-json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete_json("w", "t", "j", "the prompt", &serde_json::json!({"k": "v"}))
            .unwrap();
    });
}

#[tokio::test]
async fn complete_json_surfaces_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/complete-json"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
        .mount(&server)
        .await;
    let uri = server.uri();
    let err = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete_json("w", "t", "j", "p", &serde_json::json!({}))
            .unwrap_err()
    });
    assert!(err.to_string().contains("complete-json failed"));
}

#[tokio::test]
async fn fail_surfaces_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/jobs/j/fail"))
        .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable"))
        .mount(&server)
        .await;
    let uri = server.uri();
    let err = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.fail("w", "t", "j", "err", true).unwrap_err()
    });
    assert!(err.to_string().contains("fail failed"));
}

#[tokio::test]
async fn ship_logs_surfaces_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/logs"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
        .mount(&server)
        .await;
    let uri = server.uri();
    let err = detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.ship_logs(
            "w",
            "t",
            LogBatch {
                entries: vec![LogEntry {
                    ts: "ts".into(),
                    level: "info".into(),
                    category: "x".into(),
                    message: "m".into(),
                    job_id: None,
                }],
            },
        )
        .unwrap_err()
    });
    assert!(err.to_string().contains("log ship failed"));
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

// ---------------------------------------------------------------------------
// Tracing emission — proves every HTTP call leaves an operator-visible
// breadcrumb (debug on success, warn on failure) with the endpoint,
// status and elapsed time.  Without this the operations team only sees
// the API-shipped log line, which doesn't include the URL the worker
// actually hit.
//
// The shared `test_support::capture` helper (re-exported above as
// `captured_logs_for`) installs one process-global subscriber +
// thread-local sink.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn successful_call_emits_debug_tracing_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({ "workerId": "w", "authToken": "t" })),
        )
        .mount(&server)
        .await;

    let uri = server.uri();
    let logs = captured_logs_for(move || {
        let api = ApiClient::new(uri).unwrap();
        api.register("boot", caps(), None).unwrap();
    });

    assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
    assert!(
        logs.contains("/graphics/api/workers/register"),
        "expected endpoint in log, got: {logs}"
    );
    assert!(
        logs.contains("op=\"register\""),
        "expected op field: {logs}"
    );
    assert!(logs.contains("status=201"), "expected status field: {logs}");
    assert!(logs.contains("elapsed_ms"), "expected elapsed_ms: {logs}");
}

#[tokio::test]
async fn failing_call_emits_warn_tracing_event_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w/heartbeat"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream-unavailable"))
        .mount(&server)
        .await;

    let uri = server.uri();
    let logs = captured_logs_for(move || {
        let api = ApiClient::new(uri).unwrap();
        let _ = api.heartbeat("w", "t", caps(), None);
    });

    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"heartbeat\""),
        "expected op field: {logs}"
    );
    assert!(logs.contains("status=503"), "expected status field: {logs}");
    assert!(
        logs.contains("upstream-unavailable"),
        "expected response body in log, got: {logs}"
    );
}
