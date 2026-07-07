//! Proves the `run` startup banner and the `set_threshold` mutation
//! helper emit operator-visible `tracing` events.
//!
//! Without these the only thing a `journalctl -u studio-worker` shows
//! on a healthy worker boot is whatever the loops happen to log on
//! their first tick — the actual config in effect (API URL, VRAM
//! threshold, auto-update settings, agent version, models root) is
//! invisible.
//!
//! Uses the shared `studio_worker::test_support::capture` helper
//! (aliased locally as `captured_logs_for` to keep call sites
//! readable).

use std::path::PathBuf;
use studio_worker::config::{self, Config};
use studio_worker::runtime;
use studio_worker::test_support::capture as captured_logs_for;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Startup banner — fires once at `run` boot, lists what the worker is
// actually configured with.
// ---------------------------------------------------------------------------

#[test]
fn log_startup_banner_records_key_config_fields() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let cfg = Config {
        api_base_url: "https://studio.example.com".into(),
        vram_threshold_gb: 8.5,
        auto_start: false,
        auto_update_enabled: true,
        auto_update_interval_secs: 900,
        models_root: PathBuf::from("/tmp/audit-models"),
        worker_id: Some("worker-42".into()),
        ..Config::default()
    };
    let path_for_thread = path.clone();
    let logs = captured_logs_for(move || {
        runtime::log_startup_banner(&cfg, &path_for_thread);
    });

    assert!(
        logs.contains("studio_worker::runtime"),
        "expected runtime target: {logs}"
    );
    assert!(logs.contains("INFO"), "expected INFO event: {logs}");
    // Assert the field is present and names the config file rather than
    // matching the full path verbatim: on Windows the path has
    // backslashes that tracing escapes (`\\`), so a raw `path.display()`
    // compare never matches.
    assert!(
        logs.contains("config_path=\"") && logs.contains("config.toml"),
        "expected config_path field: {logs}"
    );
    assert!(
        logs.contains("api_base_url=\"https://studio.example.com\""),
        "expected api_base_url field: {logs}"
    );
    assert!(
        logs.contains("vram_threshold_gb=8.5"),
        "expected vram_threshold_gb field: {logs}"
    );
    assert!(
        logs.contains("auto_start=false"),
        "expected auto_start field: {logs}"
    );
    assert!(
        logs.contains("auto_update_enabled=true"),
        "expected auto_update_enabled field: {logs}"
    );
    assert!(
        logs.contains("auto_update_interval_secs=900"),
        "expected auto_update_interval_secs field: {logs}"
    );
    assert!(
        logs.contains("models_root=\"/tmp/audit-models\""),
        "expected models_root field: {logs}"
    );
    assert!(
        logs.contains("worker_id=\"worker-42\""),
        "expected worker_id field: {logs}"
    );
    assert!(
        logs.contains(&format!("version=\"{}\"", studio_worker::AGENT_VERSION)),
        "expected version field: {logs}"
    );
}

#[test]
fn log_startup_banner_marks_unregistered_worker() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let cfg = Config {
        worker_id: None,
        ..Config::default()
    };
    let path_for_thread = path.clone();
    let logs = captured_logs_for(move || {
        runtime::log_startup_banner(&cfg, &path_for_thread);
    });

    assert!(
        logs.contains("worker_id=\"(unregistered)\""),
        "expected worker_id=(unregistered): {logs}"
    );
}

// ---------------------------------------------------------------------------
// State-mutation audit trail — `set_threshold` persists a change that
// affects job intake.  It MUST leave a tracing record so the operator
// can correlate "worker stopped claiming jobs of size X" with the
// actual CLI invocation that flipped the threshold.
// ---------------------------------------------------------------------------

#[test]
fn set_threshold_emits_audit_trail() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    config::save(&Config::default(), &path).unwrap();

    let path_str = path.to_string_lossy().to_string();
    let logs = captured_logs_for(move || {
        runtime::set_threshold(Some(&path_str), 24.0).unwrap();
    });

    assert!(
        logs.contains("studio_worker::runtime"),
        "expected runtime target: {logs}"
    );
    assert!(logs.contains("INFO"), "expected INFO event: {logs}");
    assert!(
        logs.contains("op=\"set_threshold\""),
        "expected op field: {logs}"
    );
    assert!(
        logs.contains("vram_threshold_gb=24"),
        "expected vram_threshold_gb field: {logs}"
    );
}
