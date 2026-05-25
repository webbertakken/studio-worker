//! Auto-update test against a wiremock GitHub Releases feed.
//!
//! We don't actually execute the installer here — that would replace the
//! test binary itself.  We verify the *decision* logic: parsing the feed,
//! comparing semver, and selecting the right asset URL.

use semver::Version;
use studio_worker::test_support::capture as captured_logs_for;
use studio_worker::update;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn release(tag: &str, prerelease: bool, draft: bool) -> serde_json::Value {
    let installer = if cfg!(target_os = "windows") {
        "studio-worker-installer.ps1"
    } else {
        "studio-worker-installer.sh"
    };
    serde_json::json!({
        "tag_name": tag,
        "prerelease": prerelease,
        "draft": draft,
        "assets": [{
            "name": installer,
            "browser_download_url": format!("https://example.com/{}/{}", tag, installer),
        }],
    })
}

#[tokio::test]
async fn check_reports_up_to_date_when_no_newer_release() {
    let server = MockServer::start().await;
    let body = serde_json::json!([
        release("v0.1.0", false, false),
        release("v0.0.9", false, false)
    ]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let feed = format!("{}/releases", server.uri());
    let current = Version::new(0, 1, 0);
    let outcome = std::thread::spawn(move || update::check(&feed, &current, false))
        .join()
        .unwrap()
        .unwrap();
    match outcome {
        update::CheckOutcome::UpToDate { current } => assert_eq!(current, Version::new(0, 1, 0)),
        other => panic!("unexpected: {:?}", other),
    }
}

#[tokio::test]
async fn check_reports_newer_available() {
    let server = MockServer::start().await;
    let body = serde_json::json!([
        release("v0.1.0", false, false),
        release("v0.2.0", false, false),
        release("v0.1.5", false, false),
    ]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let feed = format!("{}/releases", server.uri());
    let current = Version::new(0, 1, 0);
    let outcome = std::thread::spawn(move || update::check(&feed, &current, false))
        .join()
        .unwrap()
        .unwrap();
    match outcome {
        update::CheckOutcome::NewerAvailable { current, latest } => {
            assert_eq!(current, Version::new(0, 1, 0));
            assert_eq!(latest, Version::new(0, 2, 0));
        }
        other => panic!("unexpected: {:?}", other),
    }
}

#[tokio::test]
async fn check_skips_prereleases_by_default() {
    let server = MockServer::start().await;
    let body = serde_json::json!([
        release("v0.1.0", false, false),
        release("v0.3.0-rc.1", true, false),
    ]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let feed = format!("{}/releases", server.uri());
    let current = Version::new(0, 1, 0);
    let outcome = std::thread::spawn(move || update::check(&feed, &current, false))
        .join()
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, update::CheckOutcome::UpToDate { .. }));

    // Opt-in to prereleases and the same feed now reports an upgrade.
    let feed = format!("{}/releases", server.uri());
    let current = Version::new(0, 1, 0);
    let outcome = std::thread::spawn(move || update::check(&feed, &current, true))
        .join()
        .unwrap()
        .unwrap();
    assert!(matches!(
        outcome,
        update::CheckOutcome::NewerAvailable { .. }
    ));
}

#[tokio::test]
async fn check_skips_drafts() {
    let server = MockServer::start().await;
    let body = serde_json::json!([
        release("v0.1.0", false, false),
        release("v0.9.0", false, true), // draft
    ]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let feed = format!("{}/releases", server.uri());
    let current = Version::new(0, 1, 0);
    let outcome = std::thread::spawn(move || update::check(&feed, &current, false))
        .join()
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, update::CheckOutcome::UpToDate { .. }));
}

#[tokio::test]
async fn check_accepts_latest_endpoint_object_shape() {
    let server = MockServer::start().await;
    let body = release("v9.9.9", false, false);
    Mock::given(method("GET"))
        .and(path("/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let feed = format!("{}/releases/latest", server.uri());
    let current = Version::new(0, 1, 0);
    let outcome = std::thread::spawn(move || update::check(&feed, &current, false))
        .join()
        .unwrap()
        .unwrap();
    match outcome {
        update::CheckOutcome::NewerAvailable { latest, .. } => {
            assert_eq!(latest, Version::new(9, 9, 9));
        }
        other => panic!("unexpected: {:?}", other),
    }
}

#[tokio::test]
async fn parse_tag_strips_v_prefix() {
    assert_eq!(update::parse_tag("v1.2.3"), Some(Version::new(1, 2, 3)));
    assert_eq!(update::parse_tag("1.2.3"), Some(Version::new(1, 2, 3)));
    assert_eq!(update::parse_tag("garbage"), None);
}

#[tokio::test]
async fn check_surfaces_5xx_from_feed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    let current = Version::new(0, 1, 0);
    let err = std::thread::spawn(move || update::check(&feed, &current, false))
        .join()
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("503"));
}

#[tokio::test]
async fn apply_with_fake_runner_runs_full_flow() {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use studio_worker::update::UpdateRunner;

    struct FakeRunner {
        downloads: Mutex<Vec<String>>,
        installs: Mutex<Vec<PathBuf>>,
    }
    impl UpdateRunner for FakeRunner {
        fn download(&self, url: &str, dest: &Path) -> anyhow::Result<()> {
            self.downloads.lock().unwrap().push(url.to_string());
            std::fs::write(dest, b"#!/bin/sh\necho ok\n").unwrap();
            Ok(())
        }
        fn run_installer(&self, p: &Path) -> anyhow::Result<()> {
            self.installs.lock().unwrap().push(p.to_path_buf());
            Ok(())
        }
    }

    let server = MockServer::start().await;
    let body = serde_json::json!([release("v0.2.0", false, false)]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    let target = Version::new(0, 2, 0);

    let runner = std::sync::Arc::new(FakeRunner {
        downloads: Mutex::new(Vec::new()),
        installs: Mutex::new(Vec::new()),
    });
    let runner_clone = runner.clone();
    let result = std::thread::spawn(move || update::apply_with(&feed, &target, &*runner_clone))
        .join()
        .unwrap();
    result.unwrap();
    assert_eq!(runner.downloads.lock().unwrap().len(), 1);
    assert_eq!(runner.installs.lock().unwrap().len(), 1);
    assert!(runner.downloads.lock().unwrap()[0].contains("v0.2.0"));
}

#[tokio::test]
async fn apply_with_errors_when_release_missing() {
    use std::path::Path;
    use studio_worker::update::UpdateRunner;
    struct Noop;
    impl UpdateRunner for Noop {
        fn download(&self, _u: &str, _d: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_installer(&self, _p: &Path) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let server = MockServer::start().await;
    let body = serde_json::json!([release("v0.1.0", false, false)]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    let missing = Version::new(9, 9, 9);
    let err = std::thread::spawn(move || update::apply_with(&feed, &missing, &Noop))
        .join()
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("release 9.9.9"));
}

#[tokio::test]
async fn apply_with_propagates_download_errors() {
    use std::path::Path;
    use studio_worker::update::UpdateRunner;
    struct DownloadFails;
    impl UpdateRunner for DownloadFails {
        fn download(&self, _u: &str, _d: &Path) -> anyhow::Result<()> {
            anyhow::bail!("simulated download fail")
        }
        fn run_installer(&self, _p: &Path) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let server = MockServer::start().await;
    let body = serde_json::json!([release("v0.2.0", false, false)]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    let target = Version::new(0, 2, 0);
    let err = std::thread::spawn(move || update::apply_with(&feed, &target, &DownloadFails))
        .join()
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("simulated download"));
}

#[tokio::test]
async fn apply_with_propagates_run_installer_errors() {
    use std::path::Path;
    use studio_worker::update::UpdateRunner;
    struct InstallFails;
    impl UpdateRunner for InstallFails {
        fn download(&self, _u: &str, dest: &Path) -> anyhow::Result<()> {
            std::fs::write(dest, b"#!/bin/sh\n").unwrap();
            Ok(())
        }
        fn run_installer(&self, _p: &Path) -> anyhow::Result<()> {
            anyhow::bail!("simulated installer fail")
        }
    }
    let server = MockServer::start().await;
    let body = serde_json::json!([release("v0.2.0", false, false)]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    let target = Version::new(0, 2, 0);
    let err = std::thread::spawn(move || update::apply_with(&feed, &target, &InstallFails))
        .join()
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("simulated installer"));
}

#[tokio::test]
async fn real_runner_can_be_constructed() {
    let _ = update::RealRunner;
}

#[tokio::test]
async fn restart_argv_returns_current_exe() {
    let (bin, _args) = update::restart_argv();
    assert!(!bin.as_os_str().is_empty());
}

#[tokio::test]
async fn apply_helper_wraps_real_runner() {
    // `apply` calls `apply_with(&RealRunner)`; we only verify it errors
    // cleanly when the feed has no matching release so RealRunner never
    // actually runs (no real installer is executed).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    let err = std::thread::spawn(move || update::apply(&feed, &Version::new(9, 9, 9)))
        .join()
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("release 9.9.9"));
}

// ---------------------------------------------------------------------------
// Tracing emission — proves the auto-update path leaves operator-visible
// breadcrumbs for every state transition (feed fetch, download, installer
// run).  Without this an update that stalls or misbehaves is invisible
// outside of the runtime's coarse-grained log-shipper entries.
//
// The shared `test_support::capture` helper (re-exported above as
// `captured_logs_for`) installs one process-global subscriber +
// thread-local sink.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_releases_emits_debug_event_on_success() {
    let server = MockServer::start().await;
    let body = serde_json::json!([release("v0.1.0", false, false)]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    let logs = captured_logs_for(move || {
        update::fetch_releases(&feed).unwrap();
    });

    assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
    assert!(logs.contains("/releases"), "expected feed url: {logs}");
    assert!(logs.contains("status=200"), "expected status field: {logs}");
    assert!(
        logs.contains("releases=1"),
        "expected releases count: {logs}"
    );
    assert!(logs.contains("elapsed_ms"), "expected elapsed_ms: {logs}");
}

#[tokio::test]
async fn fetch_releases_emits_warn_event_on_non_2xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    let logs = captured_logs_for(move || {
        let _ = update::fetch_releases(&feed);
    });

    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(logs.contains("status=503"), "expected status field: {logs}");
    assert!(logs.contains("/releases"), "expected feed url: {logs}");
}

#[tokio::test]
async fn apply_with_emits_info_events_for_every_state_transition() {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use studio_worker::update::UpdateRunner;

    struct FakeRunner {
        installs: Mutex<Vec<PathBuf>>,
    }
    impl UpdateRunner for FakeRunner {
        fn download(&self, _u: &str, dest: &Path) -> anyhow::Result<()> {
            std::fs::write(dest, b"#!/bin/sh\necho ok\n").unwrap();
            Ok(())
        }
        fn run_installer(&self, p: &Path) -> anyhow::Result<()> {
            self.installs.lock().unwrap().push(p.to_path_buf());
            Ok(())
        }
    }

    let server = MockServer::start().await;
    let body = serde_json::json!([release("v0.2.0", false, false)]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());

    let logs = captured_logs_for(move || {
        let runner = FakeRunner {
            installs: Mutex::new(Vec::new()),
        };
        update::apply_with(&feed, &Version::new(0, 2, 0), &runner).unwrap();
    });

    assert!(logs.contains("INFO"), "expected INFO event, got: {logs}");
    assert!(
        logs.contains("applying update"),
        "expected apply-start breadcrumb: {logs}"
    );
    assert!(
        logs.contains("downloading installer"),
        "expected download breadcrumb: {logs}"
    );
    assert!(
        logs.contains("running installer"),
        "expected installer run breadcrumb: {logs}"
    );
    assert!(
        logs.contains("installer completed"),
        "expected installer-completed breadcrumb: {logs}"
    );
    assert!(
        logs.contains("latest=0.2.0"),
        "expected target version field: {logs}"
    );
}

#[tokio::test]
async fn apply_with_errors_when_installer_asset_missing() {
    use std::path::Path;
    use studio_worker::update::UpdateRunner;
    struct Noop;
    impl UpdateRunner for Noop {
        fn download(&self, _u: &str, _d: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_installer(&self, _p: &Path) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let server = MockServer::start().await;
    let body = serde_json::json!([{
        "tag_name": "v0.5.0",
        "prerelease": false,
        "draft": false,
        "assets": [{
            "name": "unrelated.txt",
            "browser_download_url": "https://example.com/x",
        }],
    }]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    let target = Version::new(0, 5, 0);
    let err = std::thread::spawn(move || update::apply_with(&feed, &target, &Noop))
        .join()
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("installer asset"));
}
