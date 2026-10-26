//! Per-tick coverage of the auto-updater loop, plus a smoke test that
//! `runtime::run` returns cleanly when aborted.
//!
//! After the WS migration the heartbeat / claim / log-shipper ticks
//! have been removed; their replacement lives in `ws::session` and is
//! covered by `ws_client_contract.rs` + the orchestrator unit tests on
//! the API side.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use studio_worker::config::Config;
use studio_worker::job_gate::JobGate;
use studio_worker::runtime::{
    auto_update_tick, spawn_auto_updater, AutoUpdateDecision, LoopSchedule, UpdateDrain,
};
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
    let decision = auto_update_tick(
        &cfg,
        &JobGate::new(),
        &UpdateDrain::new(Arc::new(AtomicBool::new(false))),
        &logs,
    )
    .await;
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
    let decision = auto_update_tick(
        &cfg,
        &JobGate::new(),
        &UpdateDrain::new(Arc::new(AtomicBool::new(false))),
        &logs,
    )
    .await;
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
    let decision = auto_update_tick(
        &cfg,
        &JobGate::new(),
        &UpdateDrain::new(Arc::new(AtomicBool::new(false))),
        &logs,
    )
    .await;
    assert!(matches!(decision, AutoUpdateDecision::UpdateError(_)));
    let entries = logs.lock();
    assert!(entries
        .iter()
        .any(|e| e.level == "error" && e.message.contains("update failed")));
}

/// A feed whose newest release is `v9999.0.0` with no assets: an install of it fails at once, so
/// reaching `UpdateError` proves the tick got past the drain without installing anything.
async fn newer_feed() -> (MockServer, String) {
    let feed = MockServer::start().await;
    let body = serde_json::json!([{
        "tag_name": "v9999.0.0", "prerelease": false, "draft": false, "assets": [],
    }]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&feed)
        .await;
    let url = format!("{}/releases", feed.uri());
    (feed, url)
}

fn drain_for_tests(pending: &Arc<AtomicBool>, limit: Duration) -> UpdateDrain {
    UpdateDrain::new(pending.clone()).with_timing(Duration::from_millis(10), limit)
}

/// PCWebber stayed on 0.4.21 all day: every tick found a job running and skipped the check.  A busy
/// worker still checks; it only waits to install.
#[tokio::test]
async fn auto_update_tick_checks_while_a_job_runs() {
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
    let gate = JobGate::new();
    let _held = gate.try_reserve().expect("hold the slot");
    let pending = Arc::new(AtomicBool::new(false));
    let decision = auto_update_tick(
        &cfg,
        &gate,
        &drain_for_tests(&pending, Duration::from_secs(5)),
        &logs,
    )
    .await;
    assert_eq!(decision, AutoUpdateDecision::UpToDate);
    assert!(logs.lock().iter().any(|e| e.message.contains("up to date")));
}

/// A newer version found mid-job: new offers are turned away until the job ends, then it installs.
#[tokio::test]
async fn auto_update_tick_waits_for_the_running_job_then_installs() {
    let (_feed, url) = newer_feed().await;
    let mut cfg = registered_cfg("http://api.invalid");
    cfg.auto_update_enabled = true;
    cfg.auto_update_feed = url;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let gate = JobGate::new();
    let held = gate.try_reserve().expect("hold the slot");
    let pending = Arc::new(AtomicBool::new(false));
    let watched = pending.clone();
    let job = std::thread::spawn(move || {
        // The job runs on while the update waits; it ends once offers are being turned away.
        let started = std::time::Instant::now();
        while !watched.load(Ordering::SeqCst) {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the update never waited"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(held);
    });
    let decision = auto_update_tick(
        &cfg,
        &gate,
        &drain_for_tests(&pending, Duration::from_secs(5)),
        &logs,
    )
    .await;
    job.join().unwrap();
    assert!(
        matches!(decision, AutoUpdateDecision::UpdateError(_)),
        "{decision:?}"
    );
    assert!(
        !pending.load(Ordering::SeqCst),
        "offers flow again after a failed install"
    );
    let entries = logs.lock();
    assert!(entries
        .iter()
        .any(|e| e.message.contains("waits for the running job to end")));
}

/// A job that outlasts the wait: the update gives up until the next check, and offers flow again.
#[tokio::test]
async fn auto_update_tick_gives_up_waiting_for_a_long_job() {
    let (_feed, url) = newer_feed().await;
    let mut cfg = registered_cfg("http://api.invalid");
    cfg.auto_update_enabled = true;
    cfg.auto_update_feed = url;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let gate = JobGate::new();
    let _held = gate.try_reserve().expect("hold the slot");
    let pending = Arc::new(AtomicBool::new(false));
    let decision = auto_update_tick(
        &cfg,
        &gate,
        &drain_for_tests(&pending, Duration::from_millis(50)),
        &logs,
    )
    .await;
    assert_eq!(decision, AutoUpdateDecision::SkippedBusy);
    assert!(!pending.load(Ordering::SeqCst));
    assert!(logs
        .lock()
        .iter()
        .any(|e| e.level == "warn" && e.message.contains("gave up waiting")));
}

#[tokio::test]
async fn auto_update_tick_returns_disabled_when_turned_off() {
    let mut cfg = registered_cfg("http://api.invalid");
    cfg.auto_update_enabled = false;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let decision = auto_update_tick(
        &cfg,
        &JobGate::new(),
        &UpdateDrain::new(Arc::new(AtomicBool::new(false))),
        &logs,
    )
    .await;
    assert_eq!(decision, AutoUpdateDecision::Disabled);
}

// ---------------------------------------------------------------------------
// spawn_auto_updater — the scheduler loop wrapping `auto_update_tick`.
// `auto_update_tick` is exercised directly above; this drives the loop
// body that throttles to the configured interval, fires the check, and
// honours the stop flag.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_auto_updater_throttles_then_checks_until_stopped() {
    // The scheduler must accumulate `auto_update_tick`s and only run a
    // check once the configured `auto_update_interval_secs` elapses
    // (the throttle / `continue` branch), then fire a real update
    // check (the fire branch).  The feed returns no releases, so the
    // worker stays up-to-date and never restarts — the test completing
    // at all proves no spurious restart fired.
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&feed)
        .await;

    let mut cfg = registered_cfg("http://api.invalid");
    cfg.auto_update_enabled = true;
    // One-second interval with 100 ms ticks forces ~10 throttled
    // iterations before the first check fires.
    cfg.auto_update_interval_secs = 1;
    cfg.auto_update_feed = format!("{}/releases", feed.uri());

    let shared = studio_worker::config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let schedule = LoopSchedule {
        auto_update_tick: Duration::from_millis(100),
        shutdown_tick: Duration::from_millis(5),
        ..LoopSchedule::fast_for_tests()
    };

    let handle = spawn_auto_updater(
        shared,
        stop.clone(),
        logs,
        busy,
        Arc::new(AtomicBool::new(false)),
        schedule,
    );

    // Poll until the feed is hit (proving a check fired), then stop.
    let mut fired = false;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if !feed
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
        {
            fired = true;
            break;
        }
    }
    stop.store(true, Ordering::SeqCst);
    let joined = tokio::time::timeout(Duration::from_secs(2), handle).await;

    assert!(fired, "the scheduler must fire at least one update check");
    assert!(
        joined.is_ok(),
        "the loop must observe the stop flag and exit promptly"
    );
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
