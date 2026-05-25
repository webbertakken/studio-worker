//! Proves the config persistence layer (`config.rs`) leaves
//! operator-visible tracing breadcrumbs.  Without these, a worker that
//! silently loads (or worse, silently overwrites with defaults) the
//! wrong config gives operators nothing in `journalctl` to point at
//! the file that was actually consulted.
//!
//! The config file embeds two secrets — `bootstrap_token` and
//! `auth_token` — so this suite also pins the rule that **neither
//! value may ever appear in the tracing output**.  Loosening that
//! check is a production-security regression.
//!
//! Uses the shared `studio_worker::test_support::capture` helper,
//! which installs one process-global subscriber + thread-local sink
//! (see that module for the rationale).

use studio_worker::config::{self, Config};
use studio_worker::test_support::capture;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// load() — file missing → default created
// ---------------------------------------------------------------------------

#[test]
fn load_emits_info_with_source_default_created_when_file_missing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sub").join("config.toml");
    let path_str = path.to_string_lossy().to_string();
    let logs = capture(move || {
        let (_cfg, _path) = config::load(Some(&path_str)).expect("load must succeed");
    });
    assert!(logs.contains("INFO"), "expected INFO event, got: {logs}");
    assert!(
        logs.contains("op=\"load\""),
        "expected op=load, got: {logs}"
    );
    assert!(
        logs.contains("source=\"default_created\""),
        "expected source=default_created, got: {logs}"
    );
    assert!(
        logs.contains("config.toml"),
        "expected config_path field naming the file, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// load() — existing file is parsed
// ---------------------------------------------------------------------------

#[test]
fn load_emits_debug_with_source_existing_file_when_file_present() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let cfg = Config {
        engine: "gradio".into(),
        ..Config::default()
    };
    config::save(&cfg, &path).unwrap();
    let path_str = path.to_string_lossy().to_string();

    let logs = capture(move || {
        let (loaded, _) = config::load(Some(&path_str)).expect("load must succeed");
        assert_eq!(loaded.engine, "gradio");
    });
    assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
    assert!(
        logs.contains("op=\"load\""),
        "expected op=load, got: {logs}"
    );
    assert!(
        logs.contains("source=\"existing_file\""),
        "expected source=existing_file, got: {logs}"
    );
    // `%cfg.engine` (Display) renders unquoted; `?` would quote it.
    assert!(
        logs.contains("engine=gradio"),
        "expected engine field, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// save() — emits a breadcrumb so we can correlate state mutations with
// the file that was written.
// ---------------------------------------------------------------------------

#[test]
fn save_emits_debug_event_with_config_path() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let path_for_log = path.clone();
    let logs = capture(move || {
        let cfg = Config::default();
        config::save(&cfg, &path_for_log).expect("save must succeed");
    });
    assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
    assert!(
        logs.contains("op=\"save\""),
        "expected op=save, got: {logs}"
    );
    assert!(
        logs.contains("config.toml"),
        "expected config_path field naming the file, got: {logs}"
    );
    assert!(path.exists(), "file should have been written");
}

// ---------------------------------------------------------------------------
// Security: never leak the `bootstrap_token` or `auth_token` values into
// the tracing stream.  These are the two secrets in `Config`; if either
// shows up verbatim in logs, an operator viewing `journalctl` or
// shipping logs off-box would inadvertently leak credentials.
// ---------------------------------------------------------------------------

#[test]
fn load_never_logs_secret_token_values() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let cfg = Config {
        bootstrap_token: "BOOTSTRAP-SECRET-DO-NOT-LOG".into(),
        auth_token: Some("AUTH-SECRET-DO-NOT-LOG".into()),
        ..Config::default()
    };
    config::save(&cfg, &path).unwrap();
    let path_str = path.to_string_lossy().to_string();

    let logs = capture(move || {
        let _ = config::load(Some(&path_str)).expect("load must succeed");
    });
    assert!(
        !logs.contains("BOOTSTRAP-SECRET-DO-NOT-LOG"),
        "bootstrap_token leaked into logs: {logs}"
    );
    assert!(
        !logs.contains("AUTH-SECRET-DO-NOT-LOG"),
        "auth_token leaked into logs: {logs}"
    );
    // Sanity: the load event itself fired, so we know the log capture
    // is wired up and the absence of secrets above isn't because the
    // event simply didn't run.
    assert!(
        logs.contains("op=\"load\""),
        "expected the load event to fire, got: {logs}"
    );
}

#[test]
fn save_never_logs_secret_token_values() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let cfg = Config {
        bootstrap_token: "BOOTSTRAP-SECRET-DO-NOT-LOG".into(),
        auth_token: Some("AUTH-SECRET-DO-NOT-LOG".into()),
        ..Config::default()
    };
    let logs = capture(move || {
        config::save(&cfg, &path).expect("save must succeed");
    });
    assert!(
        !logs.contains("BOOTSTRAP-SECRET-DO-NOT-LOG"),
        "bootstrap_token leaked into logs: {logs}"
    );
    assert!(
        !logs.contains("AUTH-SECRET-DO-NOT-LOG"),
        "auth_token leaked into logs: {logs}"
    );
    assert!(
        logs.contains("op=\"save\""),
        "expected the save event to fire, got: {logs}"
    );
}
