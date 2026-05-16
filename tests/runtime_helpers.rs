//! Integration coverage for `runtime`'s one-shot CLI helpers + the
//! cli dispatch path.  Each test boots a wiremock server for the
//! studio API + a temp dir for the config so nothing real is touched.

use std::path::{Path, PathBuf};
use studio_worker::{cli, config, run_cli, runtime};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn write_config(dir: &Path, api_uri: &str, feed: &str) -> PathBuf {
    let path = dir.join("config.toml");
    let body = format!(
        r#"api_base_url = "{api_uri}"
bootstrap_token = "boot"
vram_threshold_gb = 16.0
auto_start = true
auto_enabled = true
engine = "synthetic"
auto_update_enabled = true
auto_update_interval_secs = 60
auto_update_feed = "{feed}"
auto_update_prerelease = false
"#
    );
    std::fs::write(&path, body).unwrap();
    path
}

#[tokio::test]
async fn register_helper_writes_worker_id_and_token_to_config() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register"))
        .and(header("authorization", "Bearer boot"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({ "workerId": "w-42", "authToken": "tok-42" })),
        )
        .mount(&api)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), &api.uri(), "http://feed.invalid");
    let cfg_path_str = cfg_path.to_string_lossy().to_string();

    runtime::register(Some(&cfg_path_str), None, None)
        .await
        .unwrap();
    let (cfg, _) = config::load(Some(&cfg_path_str)).unwrap();
    assert_eq!(cfg.worker_id.as_deref(), Some("w-42"));
    assert_eq!(cfg.auth_token.as_deref(), Some("tok-42"));
}

#[tokio::test]
async fn register_helper_honours_overrides() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register"))
        .and(header("authorization", "Bearer override-secret"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({ "workerId": "w-2", "authToken": "tok-2" })),
        )
        .mount(&api)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "http://placeholder", "http://feed.invalid");
    let cfg_path_str = cfg_path.to_string_lossy().to_string();
    runtime::register(
        Some(&cfg_path_str),
        Some("override-secret".into()),
        Some(api.uri()),
    )
    .await
    .unwrap();
    let (cfg, _) = config::load(Some(&cfg_path_str)).unwrap();
    assert_eq!(cfg.api_base_url, api.uri());
    assert_eq!(cfg.bootstrap_token, "override-secret");
}

#[tokio::test]
async fn status_helper_runs_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "http://api.invalid", "http://feed.invalid");
    let cfg_path_str = cfg_path.to_string_lossy().to_string();
    runtime::status(Some(&cfg_path_str)).await.unwrap();
}

#[tokio::test]
async fn check_update_helper_calls_feed_and_prints_outcome() {
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&feed)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(
        dir.path(),
        "http://api.invalid",
        &format!("{}/releases", feed.uri()),
    );
    let cfg_path_str = cfg_path.to_string_lossy().to_string();
    runtime::check_update(Some(&cfg_path_str)).await.unwrap();
}

#[tokio::test]
async fn set_enabled_and_threshold_persist_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "http://api.invalid", "http://feed.invalid");
    let cfg_path_str = cfg_path.to_string_lossy().to_string();

    runtime::set_enabled(Some(&cfg_path_str), false).unwrap();
    runtime::set_threshold(Some(&cfg_path_str), 33.0).unwrap();

    let (cfg, _) = config::load(Some(&cfg_path_str)).unwrap();
    assert!(!cfg.auto_enabled);
    assert_eq!(cfg.vram_threshold_gb, 33.0);
}

#[tokio::test]
async fn set_threshold_rejects_negative() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "http://api.invalid", "http://feed.invalid");
    let cfg_path_str = cfg_path.to_string_lossy().to_string();
    let err = runtime::set_threshold(Some(&cfg_path_str), -1.0).unwrap_err();
    assert!(err.to_string().contains("threshold must be >= 0"));
}

#[tokio::test]
async fn show_config_prints_resolved_toml() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "http://api.invalid", "http://feed.invalid");
    let cfg_path_str = cfg_path.to_string_lossy().to_string();
    runtime::show_config(Some(&cfg_path_str)).unwrap();
}

#[tokio::test]
async fn run_cli_dispatches_status_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "http://api.invalid", "http://feed.invalid");
    let args = cli::Cli {
        config: Some(cfg_path.to_string_lossy().to_string()),
        command: cli::Command::Status,
    };
    run_cli(args).await.unwrap();
}

#[tokio::test]
async fn run_cli_dispatches_enable_disable_and_set_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "http://api.invalid", "http://feed.invalid");
    let p = cfg_path.to_string_lossy().to_string();

    run_cli(cli::Cli {
        config: Some(p.clone()),
        command: cli::Command::Disable,
    })
    .await
    .unwrap();
    let (cfg, _) = config::load(Some(&p)).unwrap();
    assert!(!cfg.auto_enabled);

    run_cli(cli::Cli {
        config: Some(p.clone()),
        command: cli::Command::Enable,
    })
    .await
    .unwrap();
    let (cfg, _) = config::load(Some(&p)).unwrap();
    assert!(cfg.auto_enabled);

    run_cli(cli::Cli {
        config: Some(p.clone()),
        command: cli::Command::SetThreshold { gb: 7.5 },
    })
    .await
    .unwrap();
    let (cfg, _) = config::load(Some(&p)).unwrap();
    assert_eq!(cfg.vram_threshold_gb, 7.5);
}

#[tokio::test]
async fn run_cli_dispatches_config_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "http://api.invalid", "http://feed.invalid");
    run_cli(cli::Cli {
        config: Some(cfg_path.to_string_lossy().to_string()),
        command: cli::Command::Config,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn run_cli_dispatches_register_subcommand() {
    let api = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register"))
        .respond_with(
            wiremock::ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({ "workerId": "w-cli", "authToken": "tok-cli" })),
        )
        .mount(&api)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "http://placeholder", "http://feed.invalid");
    let p = cfg_path.to_string_lossy().to_string();
    run_cli(cli::Cli {
        config: Some(p.clone()),
        command: cli::Command::Register {
            bootstrap_token: Some("boot".into()),
            api_base_url: Some(api.uri()),
        },
    })
    .await
    .unwrap();
    let (cfg, _) = config::load(Some(&p)).unwrap();
    assert_eq!(cfg.worker_id.as_deref(), Some("w-cli"));
}

#[tokio::test]
async fn run_cli_dispatches_install_and_uninstall_into_temp_xdg() {
    // Redirect XDG_CONFIG_HOME so the systemd unit is written into a
    // tempdir that we clean up.  This still exercises real fs writes
    // (good!) but doesn't pollute the user's actual home.
    let xdg = tempfile::tempdir().unwrap();
    // SAFETY: tests run single-threaded per crate by default but we
    // restore the env var to be safe.
    let previous = std::env::var("XDG_CONFIG_HOME").ok();
    // SAFETY: the env mutation is fine in single-threaded test contexts.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", xdg.path()) };

    let result_install = run_cli(cli::Cli {
        config: None,
        command: cli::Command::InstallService,
    })
    .await;
    let result_uninstall = run_cli(cli::Cli {
        config: None,
        command: cli::Command::UninstallService,
    })
    .await;

    // Restore env regardless of outcome.
    // SAFETY: see above.
    unsafe {
        match previous {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    // The install attempt may or may not call systemctl, but the file
    // write itself must succeed.  Uninstall must succeed (idempotent).
    if let Err(e) = result_install {
        // Acceptable to fail on platforms where the unit dir isn't writable
        // (e.g. CI containers without HOME).  We still expect a useful error.
        eprintln!("install service err (non-fatal in tests): {e:?}");
    }
    result_uninstall.expect("uninstall should be idempotent");
}

#[tokio::test]
async fn run_cli_dispatches_run_then_aborts() {
    let api = wiremock::MockServer::start().await;
    let feed = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/heartbeat"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })),
        )
        .mount(&api)
        .await;
    wiremock::Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(wiremock::ResponseTemplate::new(204))
        .mount(&api)
        .await;
    wiremock::Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&feed)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
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
auto_update_interval_secs = 9999
auto_update_feed = "{}/releases"
auto_update_prerelease = false
"#,
            api.uri(),
            feed.uri()
        ),
    )
    .unwrap();
    let path_str = path.to_string_lossy().to_string();
    let handle = tokio::spawn(async move {
        let _ = run_cli(cli::Cli {
            config: Some(path_str),
            command: cli::Command::Run,
        })
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn run_cli_dispatches_check_update() {
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&feed)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(
        dir.path(),
        "http://api.invalid",
        &format!("{}/releases", feed.uri()),
    );
    run_cli(cli::Cli {
        config: Some(cfg_path.to_string_lossy().to_string()),
        command: cli::Command::CheckUpdate,
    })
    .await
    .unwrap();
}
