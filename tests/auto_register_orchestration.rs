//! Phase 2 of plans/auto-register-with-approval.md \u2014 the
//! orchestration tick that drives the worker through Pristine \u2192
//! Pending \u2192 Approved (writing `worker_id` + `auth_token` to
//! `config.toml`) or Rejected.
//!
//! The tick is pure-ish: it takes a config snapshot + observers,
//! does at most one HTTP round-trip per call, persists state to
//! disk via `config::save`, and returns a `RegistrationState`.
//! The outer loop in `runtime::run` calls it on a 30s interval
//! until terminal.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use studio_worker::auto_register::{self, RegistrationState};
use studio_worker::config::{self, Config};
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn pristine_cfg(api: &str) -> Config {
    Config {
        api_base_url: api.into(),
        worker_id: None,
        auth_token: None,
        engine: "synthetic".into(),
        auto_enabled: true,
        auto_update_enabled: false,
        ..Config::default()
    }
}

fn write_cfg(dir: &tempfile::TempDir, cfg: &Config) -> PathBuf {
    let path = dir.path().join("config.toml");
    config::save(cfg, &path).unwrap();
    path
}

#[tokio::test]
async fn first_tick_creates_request_and_persists_install_state() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register-request"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "requestId": "rr-001",
            "status": "pending",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let path = write_cfg(&dir, &pristine_cfg(&server.uri()));
    let shared = Arc::new(Mutex::new(pristine_cfg(&server.uri())));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));

    let state = auto_register::tick(&shared, &path, &observers).await;

    assert!(
        matches!(state, RegistrationState::Pending { ref request_id, .. } if request_id == "rr-001"),
        "expected Pending with rr-001, got {state:?}"
    );
    let on_disk = config::load(Some(path.to_str().unwrap())).unwrap().0;
    assert_eq!(on_disk.registration_request_id.as_deref(), Some("rr-001"));
    assert!(on_disk.install_id.is_some(), "install_id must be populated");
    assert!(
        on_disk.registration_secret.is_some(),
        "registration_secret must be populated"
    );
    assert!(on_disk.worker_id.is_none(), "worker_id stays empty");
}

#[tokio::test]
async fn pending_tick_returns_pending_when_studio_still_pending() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "pending",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let mut cfg = pristine_cfg(&server.uri());
    cfg.install_id = Some("install-abc".into());
    cfg.registration_request_id = Some("rr-001".into());
    cfg.registration_secret = Some("secret-xyz".into());
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));

    let state = auto_register::tick(&shared, &path, &observers).await;
    assert!(
        matches!(state, RegistrationState::Pending { ref request_id, .. } if request_id == "rr-001")
    );
}

#[tokio::test]
async fn approval_persists_worker_id_and_clears_request_state() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-002"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "approved",
            "workerId": "w-real-9",
            "authToken": "tok-shiny",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let mut cfg = pristine_cfg(&server.uri());
    cfg.install_id = Some("install-abc".into());
    cfg.registration_request_id = Some("rr-002".into());
    cfg.registration_secret = Some("secret-xyz".into());
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));

    let state = auto_register::tick(&shared, &path, &observers).await;
    assert!(matches!(state, RegistrationState::Approved));

    let on_disk = config::load(Some(path.to_str().unwrap())).unwrap().0;
    assert_eq!(on_disk.worker_id.as_deref(), Some("w-real-9"));
    assert_eq!(on_disk.auth_token.as_deref(), Some("tok-shiny"));
    assert!(
        on_disk.registration_request_id.is_none(),
        "request_id cleared on approval"
    );
    assert!(
        on_disk.registration_secret.is_none(),
        "secret cleared on approval"
    );
    // Shared snapshot also updated so the runtime loops see the new
    // worker_id without a reload.
    let snap = shared.lock().clone();
    assert_eq!(snap.worker_id.as_deref(), Some("w-real-9"));
    assert_eq!(snap.auth_token.as_deref(), Some("tok-shiny"));
}

#[tokio::test]
async fn rejection_surfaces_reason_and_clears_request_state() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-003"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "rejected",
            "reason": "stranger",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let mut cfg = pristine_cfg(&server.uri());
    cfg.install_id = Some("install-abc".into());
    cfg.registration_request_id = Some("rr-003".into());
    cfg.registration_secret = Some("secret-xyz".into());
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));

    let state = auto_register::tick(&shared, &path, &observers).await;
    match state {
        RegistrationState::Rejected { reason } => assert_eq!(reason, "stranger"),
        other => panic!("expected Rejected, got {other:?}"),
    }

    let on_disk = config::load(Some(path.to_str().unwrap())).unwrap().0;
    assert!(on_disk.registration_request_id.is_none());
    assert!(on_disk.registration_secret.is_none());
    assert!(
        on_disk.worker_id.is_none(),
        "worker_id stays empty on rejection"
    );
}

#[tokio::test]
async fn stale_404_drops_request_id_so_next_tick_starts_fresh() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/register-requests/rr-old"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register-request"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "requestId": "rr-fresh",
            "status": "pending",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let mut cfg = pristine_cfg(&server.uri());
    cfg.install_id = Some("install-abc".into());
    cfg.registration_request_id = Some("rr-old".into());
    cfg.registration_secret = Some("secret-xyz".into());
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));

    // 404 tick clears the stale id.
    let _ = auto_register::tick(&shared, &path, &observers).await;
    let mid = config::load(Some(path.to_str().unwrap())).unwrap().0;
    assert!(mid.registration_request_id.is_none());

    // Next tick re-submits and gets a fresh id back.
    let state = auto_register::tick(&shared, &path, &observers).await;
    assert!(
        matches!(state, RegistrationState::Pending { ref request_id, .. } if request_id == "rr-fresh")
    );
}

#[tokio::test]
async fn already_registered_short_circuits_without_http() {
    // No mocks => any HTTP call would fail loud.
    let server = MockServer::start().await;
    let dir = tempdir().unwrap();
    let mut cfg = pristine_cfg(&server.uri());
    cfg.worker_id = Some("w-old".into());
    cfg.auth_token = Some("tok-old".into());
    let path = write_cfg(&dir, &cfg);
    let shared = Arc::new(Mutex::new(cfg));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));

    let state = auto_register::tick(&shared, &path, &observers).await;
    assert!(matches!(state, RegistrationState::Approved));
}

#[tokio::test]
async fn install_id_is_stable_across_ticks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register-request"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "requestId": "rr-stable-1",
            "status": "pending",
        })))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let path = write_cfg(&dir, &pristine_cfg(&server.uri()));
    let shared = Arc::new(Mutex::new(pristine_cfg(&server.uri())));
    let observers = Arc::new(Mutex::new(RegistrationState::Pristine));

    let _ = auto_register::tick(&shared, &path, &observers).await;
    let first = shared.lock().install_id.clone();
    assert!(first.is_some());

    // Pretend we crashed before recording the request id but kept
    // the install id.  Next tick should reuse the install id.
    shared.lock().registration_request_id = None;
    shared.lock().registration_secret = None;
    let _ = auto_register::tick(&shared, &path, &observers).await;
    let second = shared.lock().install_id.clone();
    assert_eq!(first, second, "install_id must persist across ticks");
}
