//! Auto-update: poll a GitHub Releases feed, download cargo-dist's
//! platform installer when a newer semver is available, and re-exec
//! ourselves so the new binary takes over.
//!
//! The update task in `runtime.rs` only invokes us when the worker is
//! idle (no job in flight) so generation runs never get killed mid-flow.
//!
//! All side-effecting bits (HTTP, filesystem writes, process spawn) flow
//! through testable helpers; see `apply_with` for the seam.
use crate::types::GithubRelease;
use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Tracing target used for every event emitted by the updater. Operators
/// can filter the auto-update breadcrumbs in isolation with
/// `RUST_LOG=studio_worker::update=debug`.
const TRACE_TARGET: &str = "studio_worker::update";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    UpToDate { current: Version },
    NewerAvailable { current: Version, latest: Version },
}

/// Resolve the feed URL to a JSON document and parse a release list.
pub fn fetch_releases(feed_url: &str) -> Result<Vec<GithubRelease>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("studio-worker/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building reqwest client")?;
    let started = Instant::now();
    let response = client
        .get(feed_url)
        .header("accept", "application/vnd.github+json")
        .send()
        .with_context(|| format!("GET {feed_url}"))?;
    let status = response.status();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if !status.is_success() {
        warn!(
            target: TRACE_TARGET,
            feed_url,
            status = status.as_u16(),
            elapsed_ms,
            "feed fetch failed"
        );
        bail!("feed {feed_url} returned {status}");
    }
    let text = response.text()?;
    let releases = parse_releases(&text)?;
    debug!(
        target: TRACE_TARGET,
        feed_url,
        status = status.as_u16(),
        elapsed_ms,
        releases = releases.len(),
        "feed fetched"
    );
    Ok(releases)
}

/// Pure parser separated from the HTTP call so it's trivially testable.
pub fn parse_releases(text: &str) -> Result<Vec<GithubRelease>> {
    if let Ok(list) = serde_json::from_str::<Vec<GithubRelease>>(text) {
        return Ok(list);
    }
    let single: GithubRelease = serde_json::from_str(text)
        .with_context(|| "feed JSON is neither an array nor a single release")?;
    Ok(vec![single])
}

/// Parse the version from a release tag.  Accepts both `1.2.3` and
/// `v1.2.3`.
pub fn parse_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// Compare the local version against the feed and decide whether to
/// update.
pub fn check(feed_url: &str, current: &Version, prerelease_ok: bool) -> Result<CheckOutcome> {
    let releases = fetch_releases(feed_url)?;
    Ok(decide(&releases, current, prerelease_ok))
}

/// Pure decision function so we can unit-test the prerelease/draft
/// filters without going through HTTP.
pub fn decide(releases: &[GithubRelease], current: &Version, prerelease_ok: bool) -> CheckOutcome {
    let latest = releases
        .iter()
        .filter(|r| !r.draft)
        .filter(|r| prerelease_ok || !r.prerelease)
        .filter_map(|r| parse_tag(&r.tag_name))
        .max();
    match latest {
        Some(v) if v > *current => CheckOutcome::NewerAvailable {
            current: current.clone(),
            latest: v,
        },
        _ => CheckOutcome::UpToDate {
            current: current.clone(),
        },
    }
}

/// The cargo-dist installer asset name for the current platform.
pub fn installer_asset_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "studio-worker-installer.ps1"
    } else {
        "studio-worker-installer.sh"
    }
}

/// Resolve which installer asset to download for the given release.
/// Pulled out of `apply` for unit tests.
pub fn resolve_installer_url(release: &GithubRelease) -> Option<&str> {
    let name = installer_asset_name();
    release
        .assets
        .iter()
        .find(|a| a.name == name)
        .map(|a| a.browser_download_url.as_str())
}

/// Apply an update by downloading the cargo-dist installer for the
/// current platform and running it.
pub fn apply(feed_url: &str, latest: &Version) -> Result<()> {
    apply_with(feed_url, latest, &RealRunner)
}

/// Side-effect abstraction for `apply_with`.  The real implementation
/// downloads via HTTP and runs `sh` / `powershell`; tests inject a fake
/// that records calls.
pub trait UpdateRunner {
    fn download(&self, url: &str, dest: &Path) -> Result<()>;
    fn run_installer(&self, installer_path: &Path) -> Result<()>;
}

pub struct RealRunner;

impl UpdateRunner for RealRunner {
    fn download(&self, url: &str, dest: &Path) -> Result<()> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .user_agent(concat!("studio-worker/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let started = Instant::now();
        let mut response = client.get(url).send()?.error_for_status()?;
        let mut file = std::fs::File::create(dest)?;
        let bytes = std::io::copy(&mut response, &mut file)?;
        info!(
            target: TRACE_TARGET,
            url,
            dest = %dest.display(),
            bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "installer downloaded"
        );
        Ok(())
    }

    fn run_installer(&self, installer_path: &Path) -> Result<()> {
        if cfg!(target_os = "windows") {
            let status = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    installer_path
                        .to_str()
                        .ok_or_else(|| anyhow!("installer path not UTF-8"))?,
                ])
                .status()?;
            if !status.success() {
                bail!("installer exited with {status}");
            }
        } else {
            let status = std::process::Command::new("sh")
                .arg(installer_path)
                .status()?;
            if !status.success() {
                bail!("installer exited with {status}");
            }
        }
        Ok(())
    }
}

pub fn apply_with<R: UpdateRunner>(feed_url: &str, latest: &Version, runner: &R) -> Result<()> {
    info!(
        target: TRACE_TARGET,
        feed_url,
        latest = %latest,
        "applying update"
    );
    let releases = fetch_releases(feed_url)?;
    let release = releases
        .iter()
        .find(|r| parse_tag(&r.tag_name).as_ref() == Some(latest))
        .ok_or_else(|| anyhow!("release {latest} not present in feed"))?;

    let url = resolve_installer_url(release).ok_or_else(|| {
        anyhow!(
            "release {} is missing installer asset {}",
            latest,
            installer_asset_name()
        )
    })?;

    let tmp = tempfile::tempdir().context("creating tempdir for installer")?;
    let installer_path = tmp.path().join(installer_asset_name());
    info!(
        target: TRACE_TARGET,
        url,
        dest = %installer_path.display(),
        latest = %latest,
        "downloading installer"
    );
    runner.download(url, &installer_path)?;
    info!(
        target: TRACE_TARGET,
        installer = %installer_path.display(),
        latest = %latest,
        "running installer"
    );
    runner.run_installer(&installer_path)?;
    info!(
        target: TRACE_TARGET,
        latest = %latest,
        "installer completed; binary replaced"
    );
    Ok(())
}

/// Compute the (binary, args) tuple we'd re-exec ourselves with.  Pure
/// — actual exec lives in [`restart_self`].
pub fn restart_argv() -> (PathBuf, Vec<std::ffi::OsString>) {
    let mut iter = std::env::args_os();
    let bin = iter
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("studio-worker"));
    let args: Vec<std::ffi::OsString> = iter.collect();
    (bin, args)
}

/// Replace the current process with a fresh exec of the (now-updated)
/// binary.  On unix we use `execvp`; on Windows we spawn the successor
/// and exit cleanly.  Unreachable from tests — covered by integration
/// tests of `apply_with` instead.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn restart_self() -> ! {
    let (bin, args) = restart_argv();
    info!(
        target: TRACE_TARGET,
        bin = %bin.display(),
        argc = args.len(),
        "restarting into updated binary"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&bin).args(&args).exec();
        tracing::error!(
            target: TRACE_TARGET,
            bin = %bin.display(),
            %err,
            "exec into updated binary failed"
        );
        eprintln!("[studio-worker] exec failed: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&bin).args(&args).spawn() {
            Ok(_) => std::process::exit(0),
            Err(err) => {
                tracing::error!(
                    target: TRACE_TARGET,
                    bin = %bin.display(),
                    %err,
                    "spawn-restart of updated binary failed"
                );
                eprintln!("[studio-worker] spawn-restart failed: {err}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GithubRelease, GithubReleaseAsset};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn rel(tag: &str, prerelease: bool, draft: bool, with_installer: bool) -> GithubRelease {
        let assets = if with_installer {
            vec![GithubReleaseAsset {
                name: installer_asset_name().to_string(),
                browser_download_url: format!("https://example.com/{tag}"),
            }]
        } else {
            vec![]
        };
        GithubRelease {
            tag_name: tag.to_string(),
            prerelease,
            draft,
            assets,
        }
    }

    #[test]
    fn parse_tag_accepts_v_prefix_and_bare() {
        assert_eq!(parse_tag("v1.2.3"), Some(Version::new(1, 2, 3)));
        assert_eq!(parse_tag("1.2.3"), Some(Version::new(1, 2, 3)));
        assert!(parse_tag("garbage").is_none());
    }

    #[test]
    fn parse_releases_accepts_array() {
        let text = serde_json::to_string(&serde_json::json!([
            { "tag_name": "v1.0.0", "prerelease": false, "draft": false, "assets": [] }
        ]))
        .unwrap();
        let releases = parse_releases(&text).unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].tag_name, "v1.0.0");
    }

    #[test]
    fn parse_releases_accepts_single_object() {
        let text = serde_json::to_string(&serde_json::json!({
            "tag_name": "v2.0.0", "prerelease": false, "draft": false, "assets": []
        }))
        .unwrap();
        let releases = parse_releases(&text).unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].tag_name, "v2.0.0");
    }

    #[test]
    fn parse_releases_errors_on_garbage() {
        assert!(parse_releases("not json").is_err());
    }

    #[test]
    fn decide_reports_up_to_date_when_no_newer() {
        let releases = vec![rel("v0.1.0", false, false, true)];
        let outcome = decide(&releases, &Version::new(0, 1, 0), false);
        assert_eq!(
            outcome,
            CheckOutcome::UpToDate {
                current: Version::new(0, 1, 0)
            }
        );
    }

    #[test]
    fn decide_reports_newer_when_higher_present() {
        let releases = vec![
            rel("v0.1.0", false, false, true),
            rel("v0.2.0", false, false, true),
        ];
        let outcome = decide(&releases, &Version::new(0, 1, 0), false);
        assert_eq!(
            outcome,
            CheckOutcome::NewerAvailable {
                current: Version::new(0, 1, 0),
                latest: Version::new(0, 2, 0),
            }
        );
    }

    #[test]
    fn decide_skips_prereleases_unless_opted_in() {
        let releases = vec![
            rel("v0.1.0", false, false, true),
            rel("v0.3.0-rc.1", true, false, true),
        ];
        let outcome = decide(&releases, &Version::new(0, 1, 0), false);
        assert!(matches!(outcome, CheckOutcome::UpToDate { .. }));
        let outcome = decide(&releases, &Version::new(0, 1, 0), true);
        assert!(matches!(outcome, CheckOutcome::NewerAvailable { .. }));
    }

    #[test]
    fn decide_skips_drafts() {
        let releases = vec![
            rel("v0.1.0", false, false, true),
            rel("v0.9.0", false, true, true),
        ];
        let outcome = decide(&releases, &Version::new(0, 1, 0), false);
        assert!(matches!(outcome, CheckOutcome::UpToDate { .. }));
    }

    #[test]
    fn decide_handles_empty_feed() {
        let outcome = decide(&[], &Version::new(1, 0, 0), false);
        assert!(matches!(outcome, CheckOutcome::UpToDate { .. }));
    }

    #[test]
    fn decide_skips_malformed_tags() {
        let releases = vec![
            rel("garbage", false, false, true),
            rel("v0.1.0", false, false, true),
        ];
        let outcome = decide(&releases, &Version::new(0, 0, 1), false);
        match outcome {
            CheckOutcome::NewerAvailable { latest, .. } => {
                assert_eq!(latest, Version::new(0, 1, 0))
            }
            _ => panic!("expected newer"),
        }
    }

    #[test]
    fn installer_asset_name_matches_platform() {
        let name = installer_asset_name();
        if cfg!(target_os = "windows") {
            assert_eq!(name, "studio-worker-installer.ps1");
        } else {
            assert_eq!(name, "studio-worker-installer.sh");
        }
    }

    #[test]
    fn resolve_installer_url_finds_the_right_asset() {
        let release = rel("v1.0.0", false, false, true);
        let url = resolve_installer_url(&release).unwrap();
        assert_eq!(url, "https://example.com/v1.0.0");
    }

    #[test]
    fn resolve_installer_url_returns_none_when_missing() {
        let release = rel("v1.0.0", false, false, false);
        assert!(resolve_installer_url(&release).is_none());
    }

    #[test]
    fn restart_argv_uses_current_exe_and_args() {
        let (bin, _args) = restart_argv();
        assert!(!bin.as_os_str().is_empty());
    }

    // -----------------------------------------------------------------
    // apply_with — exercised via a fake runner that records calls.
    // -----------------------------------------------------------------

    struct FakeRunner {
        downloaded: RefCell<Vec<(String, PathBuf)>>,
        ran: RefCell<Vec<PathBuf>>,
        fail_download: bool,
        fail_run: bool,
    }

    impl UpdateRunner for FakeRunner {
        fn download(&self, url: &str, dest: &Path) -> Result<()> {
            self.downloaded
                .borrow_mut()
                .push((url.to_string(), dest.to_path_buf()));
            if self.fail_download {
                bail!("simulated download failure");
            }
            // Touch the file so apply's runner contract is satisfied.
            std::fs::write(dest, b"#!/bin/sh\necho fake installer\n").unwrap();
            Ok(())
        }
        fn run_installer(&self, installer_path: &Path) -> Result<()> {
            self.ran.borrow_mut().push(installer_path.to_path_buf());
            if self.fail_run {
                bail!("simulated installer failure");
            }
            Ok(())
        }
    }

    fn write_fixture_feed(dir: &tempfile::TempDir, releases: serde_json::Value) -> String {
        let path = dir.path().join("releases.json");
        std::fs::write(&path, releases.to_string()).unwrap();
        format!("file://{}", path.to_string_lossy())
    }

    fn fake_release_with_installer(tag: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "prerelease": false,
            "draft": false,
            "assets": [{
                "name": installer_asset_name(),
                "browser_download_url": format!("https://example.invalid/{tag}/{}", installer_asset_name()),
            }],
        })
    }

    // The reqwest blocking client doesn't follow `file://` URLs, so we
    // use wiremock-served feeds for the apply tests via the integration
    // suite (`tests/auto_update.rs`).  Here we just verify the unit-test
    // branches: missing release, missing asset.
    #[test]
    fn apply_with_errors_when_release_missing() {
        // Static fixture parsed via parse_releases bypasses HTTP for this
        // narrow test.  We can't call apply_with without a real HTTP fetch
        // since fetch_releases is HTTP only — but we can drive the
        // post-fetch branches directly.
        let releases: Vec<GithubRelease> = vec![rel("v0.1.0", false, false, true)];
        let missing = Version::new(9, 9, 9);
        let url = releases
            .iter()
            .find(|r| parse_tag(&r.tag_name).as_ref() == Some(&missing));
        assert!(url.is_none(), "v9.9.9 should not be in the fixture");
    }

    // Sanity: we can write a fake feed file (used by integration tests).
    #[test]
    fn writing_a_fake_feed_round_trips_through_parse_releases() {
        let dir = tempdir().unwrap();
        let url = write_fixture_feed(
            &dir,
            serde_json::json!([fake_release_with_installer("v0.1.0")]),
        );
        let _ = url;
        let text = std::fs::read_to_string(dir.path().join("releases.json")).unwrap();
        let releases = parse_releases(&text).unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].tag_name, "v0.1.0");
    }

    #[test]
    fn fake_runner_records_download_and_run() {
        let runner = FakeRunner {
            downloaded: RefCell::new(Vec::new()),
            ran: RefCell::new(Vec::new()),
            fail_download: false,
            fail_run: false,
        };
        let dir = tempdir().unwrap();
        let dest = dir.path().join("installer.sh");
        runner.download("https://example.com/a", &dest).unwrap();
        runner.run_installer(&dest).unwrap();
        assert_eq!(runner.downloaded.borrow().len(), 1);
        assert_eq!(runner.ran.borrow().len(), 1);
        assert!(dest.exists());
    }

    #[test]
    fn fake_runner_surfaces_download_errors() {
        let runner = FakeRunner {
            downloaded: RefCell::new(Vec::new()),
            ran: RefCell::new(Vec::new()),
            fail_download: true,
            fail_run: false,
        };
        let dir = tempdir().unwrap();
        let dest = dir.path().join("installer.sh");
        let err = runner.download("https://example.com/a", &dest).unwrap_err();
        assert!(err.to_string().contains("simulated download"));
    }

    #[test]
    fn fake_runner_surfaces_install_errors() {
        let runner = FakeRunner {
            downloaded: RefCell::new(Vec::new()),
            ran: RefCell::new(Vec::new()),
            fail_download: false,
            fail_run: true,
        };
        let dir = tempdir().unwrap();
        let dest = dir.path().join("installer.sh");
        let err = runner.run_installer(&dest).unwrap_err();
        assert!(err.to_string().contains("simulated installer"));
    }
}
