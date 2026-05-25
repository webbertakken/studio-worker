//! Cover the spawn_* loop wrappers by running them with millisecond
//! intervals against wiremock servers and asserting they exit cleanly
//! when stop=true.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use studio_worker::config::{self, Config};
use studio_worker::runtime::{
    self, spawn_auto_updater, spawn_claim_loop, spawn_heartbeat, spawn_log_shipper, LoopSchedule,
};
use studio_worker::types::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cfg(api: &str, feed: &str) -> Config {
    Config {
        api_base_url: api.into(),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        engine: "synthetic".into(),
        auto_enabled: true,
        auto_update_enabled: true,
        auto_update_interval_secs: 0,
        auto_update_feed: feed.into(),
        ..Config::default()
    }
}

async fn stop_after(stop: Arc<AtomicBool>, ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    stop.store(true, Ordering::SeqCst);
}

#[tokio::test]
async fn spawn_heartbeat_runs_a_tick_then_exits_on_stop() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/heartbeat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&api)
        .await;
    let cfg = config::shared(cfg(&api.uri(), "http://feed.invalid"));
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let handle = spawn_heartbeat(
        cfg,
        stop.clone(),
        logs,
        busy,
        LoopSchedule::fast_for_tests(),
    );
    stop_after(stop, 30).await;
    handle.await.unwrap();
}

#[tokio::test]
async fn spawn_claim_loop_runs_then_exits_on_stop() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&api)
        .await;
    let cfg = config::shared(cfg(&api.uri(), "http://feed.invalid"));
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let handle = spawn_claim_loop(
        cfg,
        stop.clone(),
        logs,
        busy,
        LoopSchedule::fast_for_tests(),
    );
    stop_after(stop, 30).await;
    handle.await.unwrap();
}

#[tokio::test]
async fn spawn_log_shipper_runs_then_exits_on_stop() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&api)
        .await;
    let cfg = config::shared(cfg(&api.uri(), "http://feed.invalid"));
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(vec![LogEntry {
        ts: "t".into(),
        level: "info".into(),
        category: "x".into(),
        message: "m".into(),
        job_id: None,
    }]));
    let handle = spawn_log_shipper(cfg, stop.clone(), logs, LoopSchedule::fast_for_tests());
    stop_after(stop, 30).await;
    handle.await.unwrap();
}

#[tokio::test]
async fn spawn_auto_updater_runs_then_exits_on_stop() {
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&feed)
        .await;
    let cfg = config::shared(cfg(
        "http://api.invalid",
        &format!("{}/releases", feed.uri()),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let handle = spawn_auto_updater(
        cfg,
        stop.clone(),
        logs,
        busy,
        LoopSchedule::fast_for_tests(),
    );
    stop_after(stop, 30).await;
    handle.await.unwrap();
}

#[tokio::test]
async fn run_loops_with_fast_schedule_starts_and_stops() {
    let api = MockServer::start().await;
    let feed = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/heartbeat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&feed)
        .await;
    let cfg = config::shared(cfg(&api.uri(), &format!("{}/releases", feed.uri())));
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let handle = tokio::spawn(async move {
        let _ =
            runtime::run_loops(cfg, stop_clone, logs, busy, LoopSchedule::fast_for_tests()).await;
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    stop.store(true, Ordering::SeqCst);
    handle.await.unwrap();
}

#[tokio::test]
async fn auto_updater_loop_waits_for_interval_to_elapse() {
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&feed)
        .await;
    let mut c = cfg("http://api.invalid", &format!("{}/releases", feed.uri()));
    c.auto_update_interval_secs = 9999;
    let cfg_shared = config::shared(c);
    let stop = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let handle = spawn_auto_updater(
        cfg_shared,
        stop.clone(),
        logs.clone(),
        busy,
        LoopSchedule::fast_for_tests(),
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    stop.store(true, Ordering::SeqCst);
    handle.await.unwrap();
    // No "up to date" log entry because the interval gate was never crossed.
    assert!(logs
        .lock()
        .iter()
        .all(|e| !e.message.contains("up to date")));
}
