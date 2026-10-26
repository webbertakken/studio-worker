//! Structured-field coverage for the auto-register breadcrumbs.
//!
//! Every other module in the worker tags its tracing events with a
//! stable `op = "<operation>"` field and carries the underlying error
//! in a structured `error = <display>` field (not interpolated into the
//! message), so an operator can filter the logs by operation and a JSON
//! log aggregator can extract the error.  The registration state machine
//! is the worker's most operator-facing flow ("why is my worker stuck
//! unregistered?"), so its breadcrumbs must expose the same structure.
//!
//! These tests drive the three breadcrumb shapes the flow emits — a
//! WARN on a failed register-request, a WARN on a failed poll, and an
//! ERROR on a failed credential save after approval — and assert each
//! one carries the `op` + `error` fields.  They use the same
//! capture-on-a-dedicated-thread trick as `auto_register_save_tracing`
//! so the post-`spawn_blocking` continuation (where the event fires) is
//! recorded.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use studio_worker::auto_register::{self, RegistrationState};
use studio_worker::config::Config;
use studio_worker::test_support::capture;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn pristine_cfg(api: &str) -> Config {
    Config {
        api_base_url: api.into(),
        worker_id: None,
        auth_token: None,
        auto_update_enabled: false,
        ..Config::default()
    }
}

fn cfg_with_pending_request(api: &str, request_id: &str) -> Config {
    let mut cfg = pristine_cfg(api);
    cfg.install_id = Some("install-abc".into());
    cfg.registration_request_id = Some(request_id.into());
    cfg.registration_secret = Some("secret-xyz".into());
    cfg
}

/// A config path whose parent is a regular file, so any `config::save`
/// to it fails deterministically at `create_dir_all(parent)`.
fn unwritable_config_path(dir: &tempfile::TempDir) -> PathBuf {
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    blocker.join("config.toml")
}

/// Drive a single `tick` to completion on a dedicated capture thread
/// (so the post-`spawn_blocking` continuation — where the breadcrumb
/// fires — is recorded) and return the formatted logs + final state.
fn capture_tick(cfg: Config, config_path: PathBuf) -> (String, RegistrationState) {
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));
    let result_slot: Arc<Mutex<Option<RegistrationState>>> = Arc::new(Mutex::new(None));
    let slot = result_slot.clone();

    let logs = capture(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = rt.block_on(auto_register::tick(&shared, &config_path, &observers));
        *slot.lock() = Some(state);
    });

    let state = result_slot.lock().take().expect("tick produced a state");
    (logs, state)
}

#[tokio::test]
async fn register_request_http_failure_breadcrumb_carries_op_and_error() {
    // A studio 500 on the register-request POST must leave the worker
    // Pristine (retry next tick) *and* a WARN breadcrumb tagged with the
    // operation + a structured error field.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register-request"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cfg = pristine_cfg(&server.uri());
    let config_path = dir.path().join("config.toml");

    let (logs, state) = capture_tick(cfg, config_path);

    assert!(
        matches!(state, RegistrationState::Pristine),
        "a failed register-request must stay Pristine, got {state:?}"
    );
    assert!(logs.contains("WARN"), "expected WARN, got: {logs}");
    assert!(
        logs.contains("studio_worker::auto_register"),
        "expected the auto_register target, got: {logs}"
    );
    assert!(
        logs.contains("op=\"register-request\""),
        "the breadcrumb must be tagged with the register-request op: {logs}"
    );
    assert!(
        logs.contains("error="),
        "the error must travel as a structured field, not just the message: {logs}"
    );
}

#[tokio::test]
async fn poll_http_failure_breadcrumb_carries_op_and_error() {
    // A studio 500 on the poll must keep the worker Pending (the
    // request id is retained) and emit a WARN breadcrumb tagged with the
    // poll op + a structured error field.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-poll-500"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let cfg = cfg_with_pending_request(&server.uri(), "rr-poll-500");
    let config_path = dir.path().join("config.toml");

    let (logs, state) = capture_tick(cfg, config_path);

    assert!(
        matches!(state, RegistrationState::Pending { .. }),
        "a failed poll must stay Pending, got {state:?}"
    );
    assert!(logs.contains("WARN"), "expected WARN, got: {logs}");
    assert!(
        logs.contains("op=\"poll\""),
        "the breadcrumb must be tagged with the poll op: {logs}"
    );
    assert!(
        logs.contains("error="),
        "the error must travel as a structured field: {logs}"
    );
}

#[tokio::test]
async fn approved_save_failure_breadcrumb_carries_op_and_error() {
    // The worst silent-loss path: approval flips the in-memory state but
    // the credential save fails.  It surfaces at ERROR; that event must
    // also carry the op + structured error fields (and never the token).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/graphics/api/workers/register-requests/rr-approve-fields",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "approved",
            "workerId": "w-real-42",
            "authToken": "tok-secret",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let bad_path = unwritable_config_path(&dir);
    let cfg = cfg_with_pending_request(&server.uri(), "rr-approve-fields");

    let (logs, state) = capture_tick(cfg, bad_path);

    assert!(
        matches!(state, RegistrationState::Approved),
        "expected Approved in memory, got {state:?}"
    );
    assert!(logs.contains("ERROR"), "expected ERROR, got: {logs}");
    assert!(
        logs.contains("op=\"poll\""),
        "the credential-save failure must be tagged with the poll op: {logs}"
    );
    assert!(
        logs.contains("error="),
        "the error must travel as a structured field: {logs}"
    );
    assert!(
        !logs.contains("tok-secret"),
        "auth_token must never leak into logs: {logs}"
    );
}
