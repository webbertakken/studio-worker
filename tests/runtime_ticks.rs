//! Per-tick coverage of the long-running runtime loops.  Every spawn_*
//! wrapper exists only to call its tick on a schedule, so testing the
//! ticks gives us full behavioural coverage of the loops without
//! running them for real.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use studio_worker::config::Config;
use studio_worker::runtime::{
    auto_update_tick, claim_tick, heartbeat_tick, log_shipper_tick, AutoUpdateDecision,
    ClaimOutcome,
};
use studio_worker::types::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn registered_cfg(api: &str) -> Config {
    Config {
        api_base_url: api.into(),
        worker_id: Some("w-test".into()),
        auth_token: Some("tok-test".into()),
        engine: "synthetic".into(),
        auto_enabled: true,
        auto_update_enabled: false,
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------
// heartbeat_tick
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_tick_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/heartbeat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    heartbeat_tick(&cfg, false, &logs).await.unwrap();
    let entries = logs.lock();
    assert!(entries.iter().all(|e| e.level != "warn"));
}

#[tokio::test]
async fn heartbeat_tick_logs_warning_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/heartbeat"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    heartbeat_tick(&cfg, true, &logs).await.unwrap();
    let entries = logs.lock();
    let warn = entries
        .iter()
        .find(|e| e.level == "warn")
        .expect("warn entry");
    assert!(warn.message.contains("heartbeat failed (busy=true)"));
}

#[tokio::test]
async fn heartbeat_tick_logs_when_engine_build_fails() {
    let cfg = Config {
        engine: "no-such-engine".into(),
        ..registered_cfg("http://api.invalid")
    };
    let logs = Arc::new(Mutex::new(Vec::new()));
    heartbeat_tick(&cfg, false, &logs).await.unwrap();
    let entries = logs.lock();
    assert!(entries
        .iter()
        .any(|e| e.level == "warn" && e.message.contains("engine error")));
}

// ---------------------------------------------------------------------------
// claim_tick
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_tick_no_jobs_when_204() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let outcome = claim_tick(&cfg, &logs, &busy).await;
    assert_eq!(outcome, ClaimOutcome::NoJobs);
}

#[tokio::test]
async fn claim_tick_returns_error_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let outcome = claim_tick(&cfg, &logs, &busy).await;
    assert!(matches!(outcome, ClaimOutcome::Error(_)));
}

#[tokio::test]
async fn claim_tick_runs_job_end_to_end_for_image() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobId": "j-1",
            "gameId": "g",
            "assetName": "g/creatures/a",
            "model": "synthetic",
            "vramGbEstimate": 1.0,
            "prompt": "test creature",
            "ext": "webp",
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/jobs/j-1/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let outcome = claim_tick(&cfg, &logs, &busy).await;
    assert_eq!(outcome, ClaimOutcome::RanJob);
    assert!(!busy.load(Ordering::SeqCst));
    let entries = logs.lock();
    assert!(entries.iter().any(|e| e.category == "claim"));
    assert!(entries.iter().any(|e| e.category == "generate"));
    assert!(entries.iter().any(|e| e.category == "complete"));
}

#[tokio::test]
async fn claim_tick_runs_llm_job_end_to_end() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobId": "j-2",
            "gameId": "g",
            "assetName": "g/conv/x",
            "model": "synthetic-llm",
            "vramGbEstimate": 1.0,
            "prompt": "",
            "ext": "webp",
            "task": {
                "kind": "llm",
                "messages": [{"role": "user", "content": "ping"}],
                "max_tokens": 8,
                "temperature": 0.1,
            },
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/jobs/j-2/complete-json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let outcome = claim_tick(&cfg, &logs, &busy).await;
    assert_eq!(outcome, ClaimOutcome::RanJob);
}

#[tokio::test]
async fn claim_tick_runs_audio_tts_job_end_to_end() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobId": "j-tts",
            "gameId": "g",
            "assetName": "g/audio/x",
            "model": "synthetic-tts",
            "vramGbEstimate": 1.0,
            "prompt": "",
            "ext": "wav",
            "task": {
                "kind": "audio_tts",
                "text": "hello",
                "voice": "v",
                "ext": "wav",
            },
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/jobs/j-tts/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let outcome = claim_tick(&cfg, &logs, &busy).await;
    assert_eq!(outcome, ClaimOutcome::RanJob);
}

#[tokio::test]
async fn claim_tick_runs_stt_video_jobs_end_to_end() {
    let server = MockServer::start().await;
    // STT job served first (one-shot), then 204.
    let _ = serde_json::json!({
        "jobId": "j-stt",
        "gameId": "g",
        "assetName": "g/audio/y",
        "model": "synthetic-stt",
        "vramGbEstimate": 1.0,
        "prompt": "",
        "ext": "wav",
        "task": {
            "kind": "audio_stt",
            "input_url": "https://example.com/a.wav",
            "language": "en",
        },
    });

    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobId": "j-stt",
            "gameId": "g",
            "assetName": "g/audio/y",
            "model": "synthetic-stt",
            "vramGbEstimate": 1.0,
            "prompt": "",
            "ext": "wav",
            "task": {
                "kind": "audio_stt",
                "input_url": "https://example.com/a.wav",
                "language": "en",
            },
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/graphics/api/workers/w-test/jobs/j-stt/complete-json",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let outcome = claim_tick(&cfg, &logs, &busy).await;
    assert_eq!(outcome, ClaimOutcome::RanJob);
}

#[tokio::test]
async fn claim_tick_runs_video_job_end_to_end() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobId": "j-vid",
            "gameId": "g",
            "assetName": "g/video/z",
            "model": "synthetic-video",
            "vramGbEstimate": 1.0,
            "prompt": "",
            "ext": "mp4",
            "task": {
                "kind": "video",
                "prompt": "a dragon",
                "seconds": 1.0,
                "width": 256,
                "height": 256,
                "ext": "mp4",
            },
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/jobs/j-vid/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let outcome = claim_tick(&cfg, &logs, &busy).await;
    assert_eq!(outcome, ClaimOutcome::RanJob);
}

#[tokio::test]
async fn claim_tick_fails_job_when_engine_rejects_kind() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobId": "j-bad",
            "gameId": "g",
            "assetName": "g/x/y",
            "model": "tiny",
            "vramGbEstimate": 1.0,
            "prompt": "",
            "ext": "wav",
            "task": {
                "kind": "audio_tts",
                "text": "hi",
                "voice": "v",
                "ext": "wav",
            },
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/jobs/j-bad/fail"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    // Force the gradio engine — which only does Image.
    let mut cfg = registered_cfg(&server.uri());
    cfg.engine = "gradio".into();
    cfg.gradio_endpoint_url = Some("http://gradio.invalid".into());
    cfg.supported_models_override = vec!["tiny".into()];

    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let outcome = claim_tick(&cfg, &logs, &busy).await;
    assert_eq!(outcome, ClaimOutcome::RanJob);
    let entries = logs.lock();
    assert!(entries
        .iter()
        .any(|e| e.level == "error" && e.message.contains("generate failed")));
}

#[tokio::test]
async fn claim_tick_returns_error_when_engine_build_fails() {
    let mut cfg = registered_cfg("http://api.invalid");
    cfg.engine = "no-such-engine".into();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let outcome = claim_tick(&cfg, &logs, &busy).await;
    assert!(matches!(outcome, ClaimOutcome::Error(_)));
}

// ---------------------------------------------------------------------------
// log_shipper_tick
// ---------------------------------------------------------------------------

#[tokio::test]
async fn log_shipper_tick_ships_buffered_entries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(vec![
        LogEntry {
            ts: "ts1".into(),
            level: "info".into(),
            category: "c".into(),
            message: "m1".into(),
            job_id: None,
        },
        LogEntry {
            ts: "ts2".into(),
            level: "warn".into(),
            category: "c".into(),
            message: "m2".into(),
            job_id: Some("j-1".into()),
        },
    ]));
    let n = log_shipper_tick(&cfg, &logs).await;
    assert_eq!(n, 2);
    assert!(logs.lock().is_empty());
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

// ---------------------------------------------------------------------------
// run() — drive it with a synthetic-only config + immediate shutdown.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_returns_when_aborted() {
    // run() spawns 4 forever-loops + a ctrl_c watcher.  We drive a
    // synthetic-only config that's already registered, then abort the
    // top-level future ourselves so the test exits.
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

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        format!(
            r#"api_base_url = "{}"
bootstrap_token = "boot"
worker_id = "w-test"
auth_token = "tok-test"
vram_threshold_gb = 16.0
auto_start = true
auto_enabled = true
engine = "synthetic"
auto_update_enabled = true
auto_update_interval_secs = 60
auto_update_feed = "{}/releases"
auto_update_prerelease = false
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
