//! `runtime::ensure_registered` — the startup gate that blocks the
//! worker until the studio approves it.
//!
//! `tests/auto_register_orchestration.rs` covers a single
//! `auto_register::tick` in isolation.  This file covers the *loop*
//! layered on top of it: how the gate translates a tick's
//! `RegistrationState` into a terminal `RegistrationGate` /
//! `Result`, short-circuits an already-registered worker, and shuts
//! down *cleanly* (not as a fatal error) when an operator hits Ctrl-C
//! during a long approval wait — the pre-approval wait is the normal
//! state of a fresh worker, so stopping it then must not error.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use studio_worker::auto_register::RegistrationState;
use studio_worker::config::{self, Config};
use studio_worker::runtime::{self, RegistrationGate};
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

    let gate = runtime::ensure_registered(&shared, &path, &observers, &stop)
        .await
        .expect("an already-registered worker must pass the gate");
    assert_eq!(gate, RegistrationGate::Ready);
}

#[tokio::test]
async fn returns_stopped_when_stop_is_set_before_registration() {
    // Operator hit Ctrl-C before approval ever arrived: the gate must
    // return promptly with a *clean* Stopped outcome (not a fatal
    // error), so `run` exits 0 — `systemctl stop` mustn't see the unit
    // fail and an unset/unapproved worker mustn't ship a Sentry event
    // on every routine stop.
    let dir = tempdir().unwrap();
    let cfg = polling_cfg("http://127.0.0.1:1/unreachable");
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));
    let stop = Arc::new(AtomicBool::new(true));

    let gate = runtime::ensure_registered(&shared, &path, &observers, &stop)
        .await
        .expect("a pre-set stop flag is a clean shutdown, not an error");
    assert_eq!(gate, RegistrationGate::Stopped);
}

#[tokio::test]
async fn returns_stopped_when_stop_is_set_during_the_wait() {
    // Operator hit Ctrl-C *after* the first poll returned pending but
    // *before* approval arrived: the gate must observe the stop flag
    // inside its poll-wait sleep and return the same clean Stopped
    // outcome as the pre-loop check, never a fatal error.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-gate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "pending",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cfg = polling_cfg(&server.uri());
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));
    let stop = Arc::new(AtomicBool::new(false));

    // Flip the stop flag shortly after the gate enters its wait sleep.
    let stop_flipper = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop_flipper.store(true, Ordering::SeqCst);
    });

    let gate = runtime::ensure_registered(&shared, &path, &observers, &stop)
        .await
        .expect("a clean stop during the wait must not be a fatal error");
    assert_eq!(gate, RegistrationGate::Stopped);
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

    let gate = runtime::ensure_registered(&shared, &path, &observers, &stop)
        .await
        .expect("approval must pass the gate");
    assert_eq!(gate, RegistrationGate::Ready);

    // The approval is persisted to the shared snapshot so the WS
    // session that follows sees the new credentials without a reload.
    let snap = shared.lock().clone();
    assert_eq!(snap.worker_id.as_deref(), Some("w-approved"));
    assert_eq!(snap.auth_token.as_deref(), Some("tok-approved"));
    // The gate observed a non-shutdown stop flag throughout.
    assert!(!stop.load(Ordering::SeqCst));
}
