//! Phase 1 of `plans/native-ui.md`: the runtime exposes live state
//! slots (current job, recent jobs, last heartbeat) that the upcoming
//! egui UI subscribes to.  These tests pin the contract: a single
//! `claim_tick` invocation populates the slots correctly, a
//! `heartbeat_tick` records its outcome, and a failed job ends up in
//! the recent ring with a non-empty reason.
//!
//! The slots are written by the existing tick functions — there is no
//! new background loop.  The UI just *reads* what the loops *already
//! know* but currently throw away.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use parking_lot::Mutex;
use studio_worker::config::Config;
use studio_worker::runtime::{
    claim_tick, heartbeat_tick, ClaimOutcome, HeartbeatOutcome, JobOutcome, WorkerObservers,
};
use studio_worker::types::TaskKind;
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

#[tokio::test]
async fn claim_tick_records_completed_job_in_recent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobId": "j-completed",
            "gameId": "g",
            "assetName": "g/creatures/a",
            "model": "synthetic",
            "vramGbEstimate": 1.0,
            "prompt": "a friendly slime",
            "ext": "webp",
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/graphics/api/workers/w-test/jobs/j-completed/complete",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let observers = WorkerObservers::default();

    let outcome = claim_tick(&cfg, &logs, &busy, &observers).await;

    assert_eq!(outcome, ClaimOutcome::RanJob);
    assert!(
        observers.current_job.lock().is_none(),
        "current_job must be cleared after run_job returns"
    );
    let recents = observers.recent_jobs.lock();
    assert_eq!(recents.len(), 1, "exactly one recent job recorded");
    let entry = &recents[0];
    assert_eq!(entry.job_id, "j-completed");
    assert_eq!(entry.kind, TaskKind::Image);
    assert_eq!(entry.model, "synthetic");
    assert_eq!(entry.prompt, "a friendly slime");
    assert!(
        matches!(entry.outcome, JobOutcome::Completed),
        "expected Completed, got {:?}",
        entry.outcome
    );
    assert!(
        entry.finished_at >= entry.started_at,
        "finished_at must be at or after started_at"
    );
}

#[tokio::test]
async fn claim_tick_records_failed_job_when_complete_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobId": "j-fail",
            "gameId": "g",
            "assetName": "g/creatures/a",
            "model": "synthetic",
            "vramGbEstimate": 1.0,
            "prompt": "boom",
            "ext": "webp",
        })))
        .mount(&server)
        .await;
    // /complete blows up — engine succeeded, upload failed.
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/jobs/j-fail/complete"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let observers = WorkerObservers::default();

    let outcome = claim_tick(&cfg, &logs, &busy, &observers).await;

    assert_eq!(outcome, ClaimOutcome::RanJob);
    assert!(observers.current_job.lock().is_none());
    let recents = observers.recent_jobs.lock();
    assert_eq!(recents.len(), 1);
    match &recents[0].outcome {
        JobOutcome::Failed { reason } => assert!(
            !reason.is_empty(),
            "failed outcome must carry a non-empty reason"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn claim_tick_no_jobs_leaves_observers_untouched() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let observers = WorkerObservers::default();

    let outcome = claim_tick(&cfg, &logs, &busy, &observers).await;

    assert_eq!(outcome, ClaimOutcome::NoJobs);
    assert!(observers.current_job.lock().is_none());
    assert!(observers.recent_jobs.lock().is_empty());
}

#[tokio::test]
async fn heartbeat_tick_records_ok_outcome() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/heartbeat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let observers = WorkerObservers::default();

    heartbeat_tick(&cfg, false, &logs, &observers)
        .await
        .unwrap();

    let status = observers
        .last_heartbeat
        .lock()
        .clone()
        .expect("heartbeat status must be recorded");
    assert!(
        matches!(status.outcome, HeartbeatOutcome::Ok),
        "expected Ok, got {:?}",
        status.outcome
    );
}

#[tokio::test]
async fn heartbeat_tick_records_err_outcome_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/heartbeat"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let cfg = registered_cfg(&server.uri());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let observers = WorkerObservers::default();

    heartbeat_tick(&cfg, false, &logs, &observers)
        .await
        .unwrap();

    let status = observers
        .last_heartbeat
        .lock()
        .clone()
        .expect("heartbeat status must be recorded");
    match status.outcome {
        HeartbeatOutcome::Err { reason } => assert!(!reason.is_empty()),
        other => panic!("expected Err, got {other:?}"),
    }
}

#[tokio::test]
async fn recent_jobs_ring_is_bounded() {
    // Bound the ring so the UI doesn't have to truncate on read.  This
    // test populates the ring directly via the public helper to keep
    // the assertion fast and HTTP-free.
    use studio_worker::runtime::push_recent_job_for_tests;

    let observers = WorkerObservers::default();
    for i in 0..(studio_worker::runtime::RECENT_JOBS_CAP + 5) {
        push_recent_job_for_tests(&observers, &format!("j-{i}"));
    }
    let recents = observers.recent_jobs.lock();
    assert_eq!(recents.len(), studio_worker::runtime::RECENT_JOBS_CAP);
    // Newest is at the front; oldest entries fell off the back.
    assert_eq!(recents.front().unwrap().job_id, "j-54");
    assert_eq!(recents.back().unwrap().job_id, "j-5");
}
