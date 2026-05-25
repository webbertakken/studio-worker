//! Contract tests for the auto-register HTTP surface
//! (`POST /workers/register-request` + `GET /workers/register-requests/:id`)
//! against a wiremock fake studio.  Pins the wire-format the worker
//! sends and consumes; the studio Worker side has matching contract
//! tests of its own.

use std::collections::BTreeMap;

use studio_worker::http::ApiClient;
use studio_worker::types::*;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn caps() -> WorkerCapabilities {
    let mut models_per_kind: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
    models_per_kind.insert(TaskKind::Image, vec!["synthetic".into()]);
    WorkerCapabilities {
        machine_name: "alice-rig".into(),
        username: "alice".into(),
        agent_version: env!("CARGO_PKG_VERSION").into(),
        engine: "synthetic".into(),
        vram_total_gb: 24.0,
        vram_threshold_gb: 12.0,
        auto_enabled: true,
        auto_start: true,
        supported_models: vec!["synthetic".into()],
        task_kinds: vec![TaskKind::Image],
        supported_models_per_kind: models_per_kind,
    }
}

fn payload() -> AutoRegisterRequest {
    AutoRegisterRequest {
        install_id: "11111111-2222-3333-4444-555555555555".into(),
        registration_secret_hash: "0123456789abcdef".repeat(4),
        label: Some("alice's gaming rig".into()),
        capabilities: caps(),
        user_agent: format!("studio-worker/{}", env!("CARGO_PKG_VERSION")),
    }
}

/// Helper: spin up an ApiClient, do work, drop it — all inside one
/// blocking task so reqwest's internal tokio runtime never gets
/// dropped from an async context.  Reuses the pattern from
/// `tests/http_contract.rs`.
async fn with_client<T, F>(uri: String, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&ApiClient) -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let api = ApiClient::new(uri).unwrap();
        f(&api)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn register_request_round_trip() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register-request"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "requestId": "rr-abc-123",
            "status": "pending",
        })))
        .mount(&server)
        .await;

    let payload = payload();
    let response = with_client(server.uri(), move |api| {
        api.register_request(&payload).unwrap()
    })
    .await;

    assert_eq!(response.request_id, "rr-abc-123");
    assert_eq!(response.status, "pending");
}

#[tokio::test]
async fn poll_returns_pending_then_approved() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-abc-123"))
        .and(header("authorization", "Bearer my-secret-bearer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "pending",
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-abc-123"))
        .and(header("authorization", "Bearer my-secret-bearer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "approved",
            "workerId": "w-real-42",
            "authToken": "tok-issued-by-studio",
        })))
        .mount(&server)
        .await;

    let (first, second) = with_client(server.uri(), |api| {
        let first = api
            .poll_register_status("rr-abc-123", "my-secret-bearer")
            .unwrap();
        let second = api
            .poll_register_status("rr-abc-123", "my-secret-bearer")
            .unwrap();
        (first, second)
    })
    .await;

    assert!(matches!(first, Some(RegisterStatus::Pending)));
    match second {
        Some(RegisterStatus::Approved {
            worker_id,
            auth_token,
        }) => {
            assert_eq!(worker_id, "w-real-42");
            assert_eq!(auth_token, "tok-issued-by-studio");
        }
        other => panic!("expected Approved, got {other:?}"),
    }
}

#[tokio::test]
async fn poll_carries_rejection_reason() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-bad"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "rejected",
            "reason": "unknown contributor",
        })))
        .mount(&server)
        .await;

    let outcome = with_client(server.uri(), |api| {
        api.poll_register_status("rr-bad", "secret").unwrap()
    })
    .await;

    match outcome {
        Some(RegisterStatus::Rejected { reason }) => assert_eq!(reason, "unknown contributor"),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn poll_returns_none_on_404_so_orchestrator_can_recreate() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-stale"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let outcome = with_client(server.uri(), |api| {
        api.poll_register_status("rr-stale", "secret").unwrap()
    })
    .await;
    assert!(outcome.is_none(), "404 must surface as Ok(None)");
}

#[tokio::test]
async fn register_request_surfaces_rate_limit_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register-request"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
        .mount(&server)
        .await;

    let payload = payload();
    let msg = with_client(server.uri(), move |api| {
        let err = api.register_request(&payload).unwrap_err();
        format!("{err:#}")
    })
    .await;
    assert!(
        msg.contains("429"),
        "expected the 429 to appear in the error message, got: {msg}"
    );
}
