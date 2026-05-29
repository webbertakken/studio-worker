//! Proves the config persistence layer (`config.rs`) leaves
//! operator-visible tracing breadcrumbs.  Without these, a worker that
//! silently loads (or worse, silently overwrites with defaults) the
//! wrong config gives operators nothing in `journalctl` to point at
//! the file that was actually consulted.
//!
//! The config file embeds two secrets — `registration_secret` and
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
        api_base_url: "https://canary.example/".into(),
        ..Config::default()
    };
    config::save(&cfg, &path).unwrap();
    let path_str = path.to_string_lossy().to_string();

    let logs = capture(move || {
        let (loaded, _) = config::load(Some(&path_str)).expect("load must succeed");
        assert_eq!(loaded.api_base_url, "https://canary.example/");
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
    // `%cfg.api_base_url` (Display) renders unquoted; `?` would quote it.
    assert!(
        logs.contains("api_base_url=https://canary.example/"),
        "expected api_base_url field, got: {logs}"
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
// save() — failure path leaves an operator-visible WARN breadcrumb.
// Without it a failed persist (disk full, read-only path, permissions)
// is invisible in `journalctl` / Sentry: the only callers that logged
// the Err were the ones that remembered to, and the UI Save button
// swallowed it entirely (`let _ = draft.save(...)`).
// ---------------------------------------------------------------------------

/// A regular file standing where `save()` needs a directory makes
/// `create_dir_all` on the parent fail, so `save()` errors.  Portable
/// across OSes (no reliance on `/proc`).
fn unwritable_target(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"i am a file, not a directory").unwrap();
    blocker.join("config.toml")
}

#[test]
fn save_emits_warn_event_when_write_fails() {
    let dir = tempdir().unwrap();
    let target = unwritable_target(&dir);
    let logs = capture(move || {
        let cfg = Config::default();
        let res = config::save(&cfg, &target);
        assert!(res.is_err(), "save into a file-as-parent must fail");
    });
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"save\""),
        "expected op=save, got: {logs}"
    );
    assert!(
        logs.contains("blocker"),
        "expected the failing config_path in the log, got: {logs}"
    );
}

#[test]
fn save_failure_never_logs_secret_token_values() {
    let dir = tempdir().unwrap();
    let target = unwritable_target(&dir);
    let cfg = Config {
        registration_secret: Some("REG-SECRET-DO-NOT-LOG".into()),
        auth_token: Some("AUTH-SECRET-DO-NOT-LOG".into()),
        ..Config::default()
    };
    let logs = capture(move || {
        let _ = config::save(&cfg, &target);
    });
    assert!(
        !logs.contains("REG-SECRET-DO-NOT-LOG"),
        "registration_secret leaked into the failure log: {logs}"
    );
    assert!(
        !logs.contains("AUTH-SECRET-DO-NOT-LOG"),
        "auth_token leaked into the failure log: {logs}"
    );
    assert!(
        logs.contains("op=\"save\""),
        "expected the save event to fire, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// Security: never leak the `registration_secret` or `auth_token` values into
// the tracing stream.  These are the two secrets in `Config`; if either
// shows up verbatim in logs, an operator viewing `journalctl` or
// shipping logs off-box would inadvertently leak credentials.
// ---------------------------------------------------------------------------

#[test]
fn load_never_logs_secret_token_values() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let cfg = Config {
        registration_secret: Some("REG-SECRET-DO-NOT-LOG".into()),
        auth_token: Some("AUTH-SECRET-DO-NOT-LOG".into()),
        ..Config::default()
    };
    config::save(&cfg, &path).unwrap();
    let path_str = path.to_string_lossy().to_string();

    let logs = capture(move || {
        let _ = config::load(Some(&path_str)).expect("load must succeed");
    });
    assert!(
        !logs.contains("REG-SECRET-DO-NOT-LOG"),
        "registration_secret leaked into logs: {logs}"
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
        registration_secret: Some("REG-SECRET-DO-NOT-LOG".into()),
        auth_token: Some("AUTH-SECRET-DO-NOT-LOG".into()),
        ..Config::default()
    };
    let logs = capture(move || {
        config::save(&cfg, &path).expect("save must succeed");
    });
    assert!(
        !logs.contains("REG-SECRET-DO-NOT-LOG"),
        "registration_secret leaked into logs: {logs}"
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
