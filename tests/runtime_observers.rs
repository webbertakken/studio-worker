//! Smoke test for the `WorkerObservers` slots that the optional native
//! UI subscribes to.
//!
//! Before the WS migration these slots were populated by the polling
//! loops (`heartbeat_tick` / `claim_tick`).  After the migration the
//! WS session is the only writer; wiring the WS dispatch path through
//! to the observers is staged as a follow-up commit so this file just
//! pins the bare contract: `WorkerObservers::default()` gives empty
//! slots, and `push_recent_job_for_tests` populates the ring without
//! blowing past `RECENT_JOBS_CAP`.
use studio_worker::runtime::{push_recent_job_for_tests, WorkerObservers, RECENT_JOBS_CAP};

#[test]
fn default_observers_start_empty() {
    let observers = WorkerObservers::default();
    assert!(observers.current_job.lock().is_none());
    assert!(observers.recent_jobs.lock().is_empty());
    assert!(observers.last_heartbeat.lock().is_none());
}

#[test]
fn recent_jobs_ring_caps_at_recent_jobs_cap() {
    let observers = WorkerObservers::default();
    for i in 0..(RECENT_JOBS_CAP + 10) {
        push_recent_job_for_tests(&observers, &format!("job-{i}"));
    }
    let ring = observers.recent_jobs.lock();
    assert_eq!(ring.len(), RECENT_JOBS_CAP);
    // Newest entries are at the front of the ring.
    assert_eq!(ring[0].job_id, format!("job-{}", RECENT_JOBS_CAP + 9));
}
