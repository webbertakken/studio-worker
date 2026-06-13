//! `runtime::ensure_registered` — the startup gate that blocks the
//! worker until the studio approves it.
//!
//! `tests/auto_register_orchestration.rs` covers a single
//! `auto_register::tick` in isolation.  This file covers the *loop*
//! layered on top of it: how the gate translates a tick's
//! `RegistrationState` into a terminal `Result`, short-circuits an
//! already-registered worker, and aborts cleanly on shutdown instead
//! of hanging an operator who hits Ctrl-C during a long approval wait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use studio_worker::auto_register::RegistrationState;
use studio_worker::config::{self, Config};
use studio_worker::runtime;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A config that has a pending registration request in flight (so the
/// gate's tick takes the poll-status path) but is not yet approved.
fn polling_cfg(api: &str) -> Config {
    Config {
        api_base_url: api.into(),
        worker_id: None,
        auth_token: None,
        auto_update_enabled: false,
        install_id: Some("install-abc".into()),
        registration_request_id: Some("rr-gate".into()),
        registration_secret: Some("secret-xyz".into()),
        ..Config::default()
    }
}

fn write_cfg(dir: &tempfile::TempDir, cfg: &Config) -> std::path::PathBuf {
    let path = dir.path().join("config.toml");
    config::save(cfg, &path).unwrap();
    path
}

#[tokio::test]
async fn returns_ok_immediately_when_already_registered() {
    // An approved worker (worker_id + auth_token present) must pass the
    // gate without contacting the studio at all — the api_base_url is
    // deliberately unroutable so any HTTP attempt would fail the test.
    let dir = tempdir().unwrap();
    let mut cfg = polling_cfg("http://127.0.0.1:1/unreachable");
    cfg.worker_id = Some("w-known".into());
    cfg.auth_token = Some("tok-known".into());
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));
    let stop = Arc::new(AtomicBool::new(false));

    runtime::ensure_registered(&shared, &path, &observers, &stop)
        .await
        .expect("an already-registered worker must pass the gate");
}

#[tokio::test]
async fn aborts_with_an_error_when_stop_is_set_before_registration() {
    // Operator hit Ctrl-C before approval ever arrived: the gate must
    // return promptly with a shutdown error rather than loop forever.
    let dir = tempdir().unwrap();
    let cfg = polling_cfg("http://127.0.0.1:1/unreachable");
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));
    let stop = Arc::new(AtomicBool::new(true));

    let err = runtime::ensure_registered(&shared, &path, &observers, &stop)
        .await
        .expect_err("a pre-set stop flag must abort the gate");
    assert!(
        err.to_string().contains("shutdown before registration"),
        "unexpected abort error: {err}"
    );
}

#[tokio::test]
async fn surfaces_operator_rejection_with_reset_guidance() {
    // The studio operator rejected this worker.  The gate must fail
    // with the rejection reason *and* tell the operator how to recover
    // (`register --reset`) — a bare "rejected" leaves them stuck.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-gate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "rejected",
            "reason": "unknown host",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cfg = polling_cfg(&server.uri());
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));
    let stop = Arc::new(AtomicBool::new(false));

    let err = runtime::ensure_registered(&shared, &path, &observers, &stop)
        .await
        .expect_err("a studio rejection must fail the gate");
    let msg = err.to_string();
    assert!(
        msg.contains("registration rejected"),
        "must name the rejection: {msg}"
    );
    assert!(
        msg.contains("unknown host"),
        "must carry the operator's reason: {msg}"
    );
    assert!(
        msg.contains("register --reset"),
        "must tell the operator how to recover: {msg}"
    );
}

#[tokio::test]
async fn passes_the_gate_when_the_studio_approves() {
    // Happy path: the poll returns approved with credentials; the gate
    // returns Ok and the worker can proceed to open its WS session.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-gate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "approved",
            "workerId": "w-approved",
            "authToken": "tok-approved",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cfg = polling_cfg(&server.uri());
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));
    let stop = Arc::new(AtomicBool::new(false));

    runtime::ensure_registered(&shared, &path, &observers, &stop)
        .await
        .expect("approval must pass the gate");

    // The approval is persisted to the shared snapshot so the WS
    // session that follows sees the new credentials without a reload.
    let snap = shared.lock().clone();
    assert_eq!(snap.worker_id.as_deref(), Some("w-approved"));
    assert_eq!(snap.auth_token.as_deref(), Some("tok-approved"));
    // The gate observed a non-shutdown stop flag throughout.
    assert!(!stop.load(Ordering::SeqCst));
}
