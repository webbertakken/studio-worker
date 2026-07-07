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
async fn check_reports_newer_with_live_component_prefixed_tags() {
    // Regression: the live GitHub feed tags releases `studio-worker-v*`
    // (release-please / cargo-dist), not bare `v*`.  The updater must
    // read the version out of that shape or `check for updates` always
    // reports "up to date" even when a newer build is published.
    let server = MockServer::start().await;
    let body = serde_json::json!([
        release("studio-worker-v0.4.1", false, false),
        release("studio-worker-v0.4.2", false, false),
    ]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let feed = format!("{}/releases", server.uri());
    let current = Version::new(0, 4, 1);
    let outcome = std::thread::spawn(move || update::check(&feed, &current, false))
        .join()
        .unwrap()
        .unwrap();
    match outcome {
        update::CheckOutcome::NewerAvailable { current, latest } => {
            assert_eq!(current, Version::new(0, 4, 1));
            assert_eq!(latest, Version::new(0, 4, 2));
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
    // The component-prefixed shape the repo actually ships.
    assert_eq!(
        update::parse_tag("studio-worker-v0.4.2"),
        Some(Version::new(0, 4, 2))
    );
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

#[cfg(not(target_os = "windows"))] // apply_with parks the real exe on Windows; ExeReplaceGuard is unit-tested in update.rs
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
        fn fetch_checksum(&self, _u: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
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
        fn fetch_checksum(&self, _u: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
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
        fn fetch_checksum(&self, _u: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
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

#[cfg(not(target_os = "windows"))] // apply_with parks the real exe on Windows; ExeReplaceGuard is unit-tested in update.rs
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
        fn fetch_checksum(&self, _u: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
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

// ---------------------------------------------------------------------------
// RealRunner::download — the live-network installer fetch.  These drive
// the real reqwest::blocking path against a wiremock server so the
// happy path and the truncated-download guard are both proven
// end-to-end (not just the pure `verify_download_len` unit tests).
//
// reqwest::blocking panics if called from inside a tokio runtime, so
// each download runs on a plain OS thread — the same pattern the
// `check_*` tests above use for `update::check`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn real_runner_download_writes_body_when_length_matches() {
    use studio_worker::update::{RealRunner, UpdateRunner};
    let server = MockServer::start().await;
    let body = b"#!/bin/sh\necho real installer\n".to_vec();
    Mock::given(method("GET"))
        .and(path("/installer.sh"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;
    let url = format!("{}/installer.sh", server.uri());
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("installer.sh");
    let dest_for_thread = dest.clone();
    std::thread::spawn(move || RealRunner.download(&url, &dest_for_thread))
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);
}

#[tokio::test]
async fn real_runner_download_rejects_truncated_body() {
    use studio_worker::update::{RealRunner, UpdateRunner};
    let server = MockServer::start().await;
    // Declare a Content-Length far larger than the body actually sent.
    // reqwest sees the framed message end early and the download must
    // not silently succeed — a half-written installer would then be
    // executed.
    Mock::given(method("GET"))
        .and(path("/installer.sh"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", "9999")
                .set_body_bytes(b"too short".to_vec()),
        )
        .mount(&server)
        .await;
    let url = format!("{}/installer.sh", server.uri());
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("installer.sh");
    let dest_for_thread = dest.clone();
    let result = std::thread::spawn(move || RealRunner.download(&url, &dest_for_thread))
        .join()
        .unwrap();
    assert!(
        result.is_err(),
        "a truncated installer download must be rejected, not silently accepted"
    );
}

#[tokio::test]
async fn restart_argv_returns_current_exe() {
    let (bin, _args) = update::restart_argv();
    assert!(!bin.as_os_str().is_empty());
}

#[cfg(not(target_os = "windows"))] // apply_with parks the real exe on Windows; ExeReplaceGuard is unit-tested in update.rs
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

#[cfg(not(target_os = "windows"))] // apply_with parks the real exe on Windows; ExeReplaceGuard is unit-tested in update.rs
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
        fn fetch_checksum(&self, _u: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
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
        fn fetch_checksum(&self, _u: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
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

// ---------------------------------------------------------------------------
// Installer checksum gate: apply_with verifies the downloaded installer
// against the release's `<asset>.sha256` sidecar before executing it.
// ---------------------------------------------------------------------------

/// sha256 of the b"#!/bin/sh\necho ok\n" body the checksum fakes write.
const OK_BODY_SHA256: &str = "b4d644d4279594903f1a9911956432d9473041f2984fc6014c14d7402c7d126c";

struct ChecksumRunner {
    checksum: Option<String>,
    installs: std::sync::Mutex<Vec<std::path::PathBuf>>,
}

impl studio_worker::update::UpdateRunner for ChecksumRunner {
    fn download(&self, _u: &str, dest: &std::path::Path) -> anyhow::Result<()> {
        std::fs::write(dest, b"#!/bin/sh\necho ok\n").unwrap();
        Ok(())
    }
    fn fetch_checksum(&self, url: &str) -> anyhow::Result<Option<String>> {
        assert!(
            url.ends_with(".sha256"),
            "checksum must be fetched from the sidecar URL, got {url}"
        );
        Ok(self.checksum.clone())
    }
    fn run_installer(&self, p: &std::path::Path) -> anyhow::Result<()> {
        self.installs.lock().unwrap().push(p.to_path_buf());
        Ok(())
    }
}

async fn feed_with_v020() -> (MockServer, String) {
    let server = MockServer::start().await;
    let body = serde_json::json!([release("v0.2.0", false, false)]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let feed = format!("{}/releases", server.uri());
    (server, feed)
}

#[cfg(not(target_os = "windows"))] // apply_with parks the real exe on Windows; ExeReplaceGuard is unit-tested in update.rs
#[tokio::test]
async fn apply_runs_the_installer_when_the_sidecar_matches() {
    let (_server, feed) = feed_with_v020().await;
    let runner = std::sync::Arc::new(ChecksumRunner {
        checksum: Some(format!("{OK_BODY_SHA256}  studio-worker-installer.sh\n")),
        installs: std::sync::Mutex::new(Vec::new()),
    });
    let runner_clone = runner.clone();
    std::thread::spawn(move || update::apply_with(&feed, &Version::new(0, 2, 0), &*runner_clone))
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(runner.installs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn apply_refuses_to_run_an_installer_with_a_wrong_checksum() {
    let (_server, feed) = feed_with_v020().await;
    let runner = std::sync::Arc::new(ChecksumRunner {
        checksum: Some(format!("{}  studio-worker-installer.sh", "0".repeat(64))),
        installs: std::sync::Mutex::new(Vec::new()),
    });
    let runner_clone = runner.clone();
    let err = std::thread::spawn(move || {
        update::apply_with(&feed, &Version::new(0, 2, 0), &*runner_clone)
    })
    .join()
    .unwrap()
    .unwrap_err()
    .to_string();
    assert!(err.contains("sha256 mismatch"), "got: {err}");
    assert!(
        runner.installs.lock().unwrap().is_empty(),
        "a mismatched installer must never execute"
    );
}

#[tokio::test]
async fn apply_refuses_an_unparseable_sidecar() {
    let (_server, feed) = feed_with_v020().await;
    let runner = std::sync::Arc::new(ChecksumRunner {
        checksum: Some("total garbage, no digest here".into()),
        installs: std::sync::Mutex::new(Vec::new()),
    });
    let runner_clone = runner.clone();
    let err = std::thread::spawn(move || {
        update::apply_with(&feed, &Version::new(0, 2, 0), &*runner_clone)
    })
    .join()
    .unwrap()
    .unwrap_err()
    .to_string();
    assert!(err.contains("no parseable sha256"), "got: {err}");
    assert!(runner.installs.lock().unwrap().is_empty());
}

#[cfg(not(target_os = "windows"))] // apply_with parks the real exe on Windows; ExeReplaceGuard is unit-tested in update.rs
#[tokio::test]
async fn apply_warns_but_proceeds_when_no_sidecar_is_published() {
    // Older releases predate the sidecar; the transport is still https
    // pinned to GitHub, so the update must go ahead — with a warn.
    let (_server, feed) = feed_with_v020().await;
    let runner = std::sync::Arc::new(ChecksumRunner {
        checksum: None,
        installs: std::sync::Mutex::new(Vec::new()),
    });
    let runner_clone = runner.clone();
    let feed_clone = feed.clone();
    let logs = std::thread::spawn(move || {
        captured_logs_for(move || {
            update::apply_with(&feed_clone, &Version::new(0, 2, 0), &*runner_clone).unwrap();
        })
    })
    .join()
    .unwrap();
    assert_eq!(runner.installs.lock().unwrap().len(), 1);
    assert!(
        logs.contains("no installer checksum sidecar"),
        "skipping verification must leave a breadcrumb: {logs}"
    );
}

// ---------------------------------------------------------------------------
// RealRunner::fetch_checksum against a live loopback server.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn real_runner_fetch_checksum_distinguishes_present_absent_and_broken() {
    use studio_worker::update::{RealRunner, UpdateRunner};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/i.sh.sha256"))
        .respond_with(ResponseTemplate::new(200).set_body_string("abc  i.sh"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/broken.sha256"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let base = server.uri();

    let (present, absent, broken) = std::thread::spawn(move || {
        (
            RealRunner.fetch_checksum(&format!("{base}/i.sh.sha256")),
            RealRunner.fetch_checksum(&format!("{base}/missing.sha256")),
            RealRunner.fetch_checksum(&format!("{base}/broken.sha256")),
        )
    })
    .join()
    .unwrap();

    assert_eq!(present.unwrap(), Some("abc  i.sh".to_string()));
    assert_eq!(absent.unwrap(), None, "404 means no sidecar, not an error");
    assert!(
        broken.is_err(),
        "a 5xx must be a hard error so a blocked fetch can't pass for an absent sidecar"
    );
}
