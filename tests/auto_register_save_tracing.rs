//! Regression coverage for silent `config::save` failures inside the
//! auto-register poll loop.
//!
//! The create side (`ensure_install_state` + `create_request`) persists
//! twice on a first-ever tick — the freshly-seeded install_id + secret,
//! then the request_id the studio hands back — and `poll_existing`
//! persists on three studio responses: 404 (stale id dropped), Approved
//! (`worker_id` + `auth_token` written), and Rejected (request state
//! cleared).  Every one of these used to swallow a failed
//! `config::save` with `let _ = ...`, so a read-only or full disk
//! produced *zero* operator-visible breadcrumb.
//!
//! The Approved case is the worst: the in-memory snapshot flips to
//! `Approved` and the current session runs fine, but the credentials
//! never reach disk, so the next restart re-registers from scratch and
//! the operator must approve all over again — with nothing in the logs
//! to explain why.  That path must surface at `ERROR` (a Sentry event
//! via `sentry-tracing`); the cleanup paths surface at `WARN`.
//!
//! Each test forces `config::save` to fail by pointing the config path
//! at a child of a regular file, so `create_dir_all(parent)` fails
//! deterministically without any platform-specific permission games.

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

/// Build a config path whose parent is a regular file, so any
/// `config::save` to it fails at `create_dir_all(parent)`.
fn unwritable_config_path(dir: &tempfile::TempDir) -> PathBuf {
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    blocker.join("config.toml")
}

#[tokio::test]
async fn create_request_save_failures_are_logged_as_warn() {
    // A first-ever tick (Pristine: no install_id, no request_id)
    // persists twice: the freshly-seeded install_id + secret
    // (ensure_install_state) and the request_id the studio returns
    // (create_request).  Both used to swallow a failed config::save, so
    // an operator on a read-only / full disk saw nothing — yet the bug
    // is corrosive: the worker re-seeds a new secret every tick and
    // never converges to a stable registration request.  Both saves
    // must leave a WARN breadcrumb naming what couldn't be persisted.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register-request"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "requestId": "rr-seed",
            "status": "pending",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let bad_path = unwritable_config_path(&dir);
    let cfg = pristine_cfg(&server.uri());

    let (logs, state) = capture_tick(cfg, bad_path);

    // The POST still succeeds, so the in-memory state advances to
    // Pending; the failure is silent disk loss, not a crash.
    assert!(
        matches!(state, RegistrationState::Pending { ref request_id, .. } if request_id == "rr-seed"),
        "expected Pending with rr-seed, got {state:?}"
    );
    assert!(logs.contains("WARN"), "expected WARN events, got: {logs}");
    assert!(
        logs.contains("studio_worker::auto_register"),
        "expected the auto_register target, got: {logs}"
    );
    assert!(
        logs.contains("failed to persist install state"),
        "expected the install-state save-failure warn, got: {logs}"
    );
    assert!(
        logs.contains("failed to persist request_id"),
        "expected the request_id save-failure warn, got: {logs}"
    );
}

fn cfg_with_pending_request(api: &str, request_id: &str) -> Config {
    let mut cfg = pristine_cfg(api);
    cfg.install_id = Some("install-abc".into());
    cfg.registration_request_id = Some(request_id.into());
    cfg.registration_secret = Some("secret-xyz".into());
    cfg
}

/// Drive a single `tick` to completion on a dedicated capture thread
/// (so the post-`spawn_blocking` continuation — where the save +
/// tracing event run — is captured) and return the formatted logs.
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
async fn approval_save_failure_is_logged_as_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-approve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "approved",
            "workerId": "w-real-9",
            "authToken": "tok-shiny",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let bad_path = unwritable_config_path(&dir);
    let cfg = cfg_with_pending_request(&server.uri(), "rr-approve");

    let (logs, state) = capture_tick(cfg, bad_path);

    // The in-memory session still flips to Approved (the bug is *silent*
    // disk loss, not a crash), so the failure is invisible without logs.
    assert!(
        matches!(state, RegistrationState::Approved),
        "expected Approved in memory, got {state:?}"
    );
    assert!(logs.contains("ERROR"), "expected ERROR event, got: {logs}");
    assert!(
        logs.contains("studio_worker::auto_register"),
        "expected the auto_register target, got: {logs}"
    );
    assert!(
        logs.contains("persist approved credentials"),
        "expected the approved-credentials message, got: {logs}"
    );
    // Never leak the auth token, even on the failure path.
    assert!(
        !logs.contains("tok-shiny"),
        "auth_token leaked into logs: {logs}"
    );
}

#[tokio::test]
async fn rejection_save_failure_is_logged_as_warn() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-reject"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "rejected",
            "reason": "stranger",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let bad_path = unwritable_config_path(&dir);
    let cfg = cfg_with_pending_request(&server.uri(), "rr-reject");

    let (logs, state) = capture_tick(cfg, bad_path);

    assert!(
        matches!(state, RegistrationState::Rejected { .. }),
        "expected Rejected, got {state:?}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("studio_worker::auto_register"),
        "expected the auto_register target, got: {logs}"
    );
    assert!(
        logs.contains("rejection"),
        "expected the rejection-cleanup message, got: {logs}"
    );
}

#[tokio::test]
async fn stale_404_save_failure_is_logged_as_warn() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-stale"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let bad_path = unwritable_config_path(&dir);
    let cfg = cfg_with_pending_request(&server.uri(), "rr-stale");

    let (logs, state) = capture_tick(cfg, bad_path);

    assert!(
        matches!(state, RegistrationState::Pristine),
        "expected Pristine after 404, got {state:?}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("studio_worker::auto_register"),
        "expected the auto_register target, got: {logs}"
    );
    assert!(
        logs.contains("404"),
        "expected the stale-404 cleanup message, got: {logs}"
    );
}
