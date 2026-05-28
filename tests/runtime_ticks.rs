//! Per-tick coverage of the auto-updater loop, plus a smoke test that
//! `runtime::run` returns cleanly when aborted.
//!
//! After the WS migration the heartbeat / claim / log-shipper ticks
//! have been removed; their replacement lives in `ws::session` and is
//! covered by `ws_client_contract.rs` + the orchestrator unit tests on
//! the API side.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use studio_worker::config::Config;
use studio_worker::runtime::{auto_update_tick, AutoUpdateDecision};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn registered_cfg(api: &str) -> Config {
    Config {
        api_base_url: api.into(),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        auto_update_enabled: false,
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------
// auto_update_tick
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_update_tick_reports_up_to_date() {
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&feed)
        .await;
    let mut cfg = registered_cfg("http://api.invalid");
    cfg.auto_update_enabled = true;
    cfg.auto_update_feed = format!("{}/releases", feed.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let decision = auto_update_tick(&cfg, false, &logs).await;
    assert_eq!(decision, AutoUpdateDecision::UpToDate);
}

#[tokio::test]
async fn auto_update_tick_reports_check_error_on_bad_feed() {
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&feed)
        .await;
    let mut cfg = registered_cfg("http://api.invalid");
    cfg.auto_update_enabled = true;
    cfg.auto_update_feed = format!("{}/releases", feed.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let decision = auto_update_tick(&cfg, false, &logs).await;
    assert!(matches!(decision, AutoUpdateDecision::CheckError(_)));
}

#[tokio::test]
async fn auto_update_tick_reports_update_error_when_apply_fails() {
    // The feed advertises a higher version but the assets are missing,
    // which makes `update::apply` fail.
    let feed = MockServer::start().await;
    let body = serde_json::json!([{
        "tag_name": "v9999.0.0",
        "prerelease": false,
        "draft": false,
        "assets": [],
    }]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&feed)
        .await;
    let mut cfg = registered_cfg("http://api.invalid");
    cfg.auto_update_enabled = true;
    cfg.auto_update_feed = format!("{}/releases", feed.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let decision = auto_update_tick(&cfg, false, &logs).await;
    assert!(matches!(decision, AutoUpdateDecision::UpdateError(_)));
    let entries = logs.lock();
    assert!(entries
        .iter()
        .any(|e| e.level == "error" && e.message.contains("update failed")));
}

#[tokio::test]
async fn auto_update_tick_skips_when_busy() {
    let mut cfg = registered_cfg("http://api.invalid");
    cfg.auto_update_enabled = true;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let decision = auto_update_tick(&cfg, true, &logs).await;
    assert_eq!(decision, AutoUpdateDecision::SkippedBusy);
    let entries = logs.lock();
    assert!(entries.iter().any(|e| e.message.contains("worker is busy")));
}

#[tokio::test]
async fn auto_update_tick_returns_disabled_when_turned_off() {
    let mut cfg = registered_cfg("http://api.invalid");
    cfg.auto_update_enabled = false;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let decision = auto_update_tick(&cfg, false, &logs).await;
    assert_eq!(decision, AutoUpdateDecision::Disabled);
}

// ---------------------------------------------------------------------------
// run() — drive it with a synthetic-only config + immediate shutdown.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_returns_when_aborted() {
    // run() spawns the WS session + auto-update loop + a ctrl_c watcher.
    // We point at a wiremock studio API that always 401s the upgrade
    // (no real WS server in test), then abort the top-level future so
    // the test exits.
    let api = MockServer::start().await;
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/graphics/api/workers/w-test/connect"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&feed)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        format!(
            r#"api_base_url = "{}"
worker_id = "w-test"
auth_token = "tok-test"
vram_threshold_gb = 16.0
auto_start = true
auto_update_enabled = true
auto_update_interval_secs = 60
auto_update_feed = "{}/releases"
auto_update_prerelease = false
ws_reconnect_attempts = 1
models_root = "/tmp/studio-worker-test-models"
"#,
            api.uri(),
            feed.uri()
        ),
    )
    .unwrap();

    let path_str = cfg_path.to_string_lossy().to_string();
    let run_handle = tokio::spawn(async move {
        let _ = studio_worker::runtime::run(Some(&path_str)).await;
    });

    // Let the loops spin up briefly then abort the future so the test exits.
    tokio::time::sleep(Duration::from_millis(150)).await;
    run_handle.abort();
    let _ = run_handle.await;
}
