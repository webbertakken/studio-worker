//! Phase 4 of plans/auto-register-with-approval.md \u2014 the new
//! `studio-worker register` flags:
//!
//! - `--reset` clears local registration state so the next launch
//!   starts a fresh auto-register.
//! - `--label` seeds the human label shown in the studio's Pending
//!   Workers panel.
//! - Bare `register` (no flags + no operator token in config) is
//!   a no-op on the network: it just persists the empty change set
//!   so the next launch auto-registers.

use studio_worker::{config, runtime};
use tempfile::tempdir;

fn write_minimal_cfg(path: &std::path::Path) {
    let cfg = config::Config {
        api_base_url: "http://127.0.0.1:0".into(),
        worker_id: Some("w-old".into()),
        auth_token: Some("tok-old".into()),
        install_id: Some("install-keep-me".into()),
        registration_request_id: Some("rr-stale".into()),
        registration_secret: Some("secret-stale".into()),
        label: Some("alice's rig".into()),
        engine: "synthetic".into(),
        auto_update_enabled: false,
        ..config::Config::default()
    };
    config::save(&cfg, path).unwrap();
}

#[tokio::test]
async fn reset_clears_worker_id_and_pending_state() {
    let dir = tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    write_minimal_cfg(&cfg_path);
    let cfg_path_str = cfg_path.to_string_lossy().to_string();

    runtime::register(
        Some(&cfg_path_str),
        runtime::RegisterArgs {
            reset: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (loaded, _) = config::load(Some(&cfg_path_str)).unwrap();
    assert!(loaded.worker_id.is_none());
    assert!(loaded.auth_token.is_none());
    assert!(loaded.registration_request_id.is_none());
    assert!(loaded.registration_secret.is_none());
    assert!(
        loaded.install_id.is_none(),
        "install_id is reset too so the next run gets a fresh fingerprint"
    );
    // Label is preserved across resets so the user doesn't have to
    // retype it.
    assert_eq!(loaded.label.as_deref(), Some("alice's rig"));
}

#[tokio::test]
async fn label_set_alone_persists_without_network() {
    let dir = tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    // Pristine config: no worker_id.
    let cfg = config::Config {
        api_base_url: "http://127.0.0.1:0".into(),
        worker_id: None,
        auth_token: None,
        engine: "synthetic".into(),
        auto_update_enabled: false,
        ..config::Config::default()
    };
    config::save(&cfg, &cfg_path).unwrap();
    let cfg_path_str = cfg_path.to_string_lossy().to_string();

    runtime::register(
        Some(&cfg_path_str),
        runtime::RegisterArgs {
            label: Some("alice's gaming rig".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (loaded, _) = config::load(Some(&cfg_path_str)).unwrap();
    assert_eq!(loaded.label.as_deref(), Some("alice's gaming rig"));
}

#[tokio::test]
async fn empty_label_clears_existing_label() {
    let dir = tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let cfg = config::Config {
        api_base_url: "http://127.0.0.1:0".into(),
        label: Some("old label".into()),
        engine: "synthetic".into(),
        auto_update_enabled: false,
        ..config::Config::default()
    };
    config::save(&cfg, &cfg_path).unwrap();
    let cfg_path_str = cfg_path.to_string_lossy().to_string();

    runtime::register(
        Some(&cfg_path_str),
        runtime::RegisterArgs {
            label: Some("   ".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (loaded, _) = config::load(Some(&cfg_path_str)).unwrap();
    assert!(loaded.label.is_none(), "whitespace-only label clears it");
}

#[tokio::test]
async fn bare_register_does_not_touch_network() {
    // No mock server \u2014 if the helper attempted a network call the
    // test would either hang or fail; we expect Ok and no panic.
    let dir = tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let cfg = config::Config {
        api_base_url: "http://127.0.0.1:1".into(), // unreachable
        engine: "synthetic".into(),
        auto_update_enabled: false,
        ..config::Config::default()
    };
    config::save(&cfg, &cfg_path).unwrap();
    let cfg_path_str = cfg_path.to_string_lossy().to_string();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime::register(Some(&cfg_path_str), runtime::RegisterArgs::default()),
    )
    .await;
    assert!(result.is_ok(), "register helper should return promptly");
    result.unwrap().unwrap();
}
