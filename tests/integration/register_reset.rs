//! Contracts for `studio-worker register`:
//!
//! - `--reset` clears local registration state (worker_id, auth_token,
//!   install_id, registration_request_id, registration_secret) so the
//!   next launch starts a fresh auto-register.
//! - Bare `register` (no flags + no operator token in config) is
//!   a no-op on the network: it just rewrites the file with whatever
//!   was already on disk so the next launch auto-registers.

use studio_worker::{config, runtime};
use tempfile::tempdir;

fn write_registered_cfg(path: &std::path::Path) {
    let cfg = config::Config {
        api_base_url: "http://127.0.0.1:0".into(),
        worker_id: Some("w-old".into()),
        auth_token: Some("tok-old".into()),
        install_id: Some("install-stale".into()),
        registration_request_id: Some("rr-stale".into()),
        registration_secret: Some("secret-stale".into()),
        auto_update_enabled: false,
        ..config::Config::default()
    };
    config::save(&cfg, path).unwrap();
}

#[tokio::test]
async fn reset_clears_worker_id_and_pending_state() {
    let dir = tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    write_registered_cfg(&cfg_path);
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
}

#[tokio::test]
async fn bare_register_does_not_touch_network() {
    // No mock server — if the helper attempted a network call the
    // test would either hang or fail; we expect Ok and no panic.
    let dir = tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let cfg = config::Config {
        api_base_url: "http://127.0.0.1:1".into(), // unreachable
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

#[tokio::test]
async fn api_base_url_override_persists() {
    let dir = tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let cfg = config::Config {
        api_base_url: "https://old.example".into(),
        auto_update_enabled: false,
        ..config::Config::default()
    };
    config::save(&cfg, &cfg_path).unwrap();
    let cfg_path_str = cfg_path.to_string_lossy().to_string();

    runtime::register(
        Some(&cfg_path_str),
        runtime::RegisterArgs {
            api_base_url: Some("https://new.example/".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (loaded, _) = config::load(Some(&cfg_path_str)).unwrap();
    assert_eq!(loaded.api_base_url, "https://new.example/");
}
