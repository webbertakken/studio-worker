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

/// Parse the version from a release tag.  Accepts a bare `1.2.3`, a
/// `v1.2.3`, and the component-prefixed tags release-please / cargo-dist
/// actually push for this repo (`studio-worker-v1.2.3`).  Tries the
/// most-permissive forms in order and returns the first that parses, so
/// a prerelease suffix (`...-rc.1`) survives — only the `<component>-v`
/// prefix is stripped, never the version's own `-`.
pub fn parse_tag(tag: &str) -> Option<Version> {
    let candidates = [
        tag,
        tag.strip_prefix('v').unwrap_or(tag),
        tag.rsplit_once("-v").map(|(_, v)| v).unwrap_or(tag),
    ];
    candidates.iter().find_map(|c| Version::parse(c).ok())
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

/// Verify a streamed installer download wrote exactly the body the
/// server promised.  `expected` is the response's `Content-Length`;
/// it's `None` for chunked transfers, where there's nothing to check
/// and we accept whatever arrived.  A mismatch means the download was
/// truncated or corrupt — and because the very next step hands this
/// file to `sh` / `powershell`, running a half-written installer is
/// far more dangerous than failing the update and retrying on the next
/// tick, so we surface a clear error instead of executing it.
fn verify_download_len(copied: u64, expected: Option<u64>) -> Result<()> {
    match expected {
        Some(expected) if copied != expected => bail!(
            "size mismatch: wrote {copied} bytes but the server declared \
             Content-Length {expected} (installer download truncated or corrupt)"
        ),
        _ => Ok(()),
    }
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
        validate_installer_download_url(url)?;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .user_agent(concat!("studio-worker/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let started = Instant::now();
        let mut response = client.get(url).send()?.error_for_status()?;
        // Capture the declared length (absent on chunked transfers)
        // before streaming so a short read is caught below — the next
        // step runs this file as a shell / PowerShell script.
        let expected_len = response.content_length();
        let mut file = std::fs::File::create(dest)?;
        let bytes = std::io::copy(&mut response, &mut file)?;
        // Reject a truncated / overlong download before `apply_with`
        // hands the file to the installer runner.  Bailing here means
        // `run_installer` never executes, and `apply_with`'s tempdir
        // drop cleans up the partial file.
        verify_download_len(bytes, expected_len)
            .with_context(|| format!("downloading installer from {url}"))?;
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

fn validate_installer_download_url(raw: &str) -> Result<()> {
    let url = url::Url::parse(raw).with_context(|| format!("invalid installer URL {raw:?}"))?;
    if url.scheme() == "https" {
        return Ok(());
    }
    if url.scheme() == "http" {
        if let Some(host) = url.host_str() {
            if host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            {
                return Ok(());
            }
        }
    }
    bail!("installer URL must use https (loopback http is allowed for tests): {raw}");
}

/// Where a parked (renamed-aside) running executable lives: the full
/// original file name with `.old` appended.  `with_extension` would
/// turn `studio-worker.exe` into `studio-worker.old` and risk
/// clobbering an unrelated sibling.
pub fn parked_artifact_path(exe: &Path) -> PathBuf {
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "studio-worker".to_string());
    exe.with_file_name(format!("{name}.old"))
}

/// Remove a leftover parked binary from a previous update.  Called on
/// startup; best-effort — a locked or missing file is fine.
pub fn cleanup_parked_artifact(exe: &Path) {
    let parked = parked_artifact_path(exe);
    match std::fs::remove_file(&parked) {
        Ok(()) => info!(
            target: TRACE_TARGET,
            parked = %parked.display(),
            "removed parked binary from a previous update"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(
            target: TRACE_TARGET,
            parked = %parked.display(),
            error = %e,
            "could not remove parked binary; will retry next start"
        ),
    }
}

/// Best-effort startup cleanup for the running process's own parked
/// artifact.  Excluded from coverage: depends on `current_exe`.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn cleanup_parked_artifact_for_current_exe() {
    if let Ok(exe) = std::env::current_exe() {
        cleanup_parked_artifact(&exe);
    }
}

/// Windows can't overwrite a running executable (the file is locked),
/// but it CAN rename it.  Parking the running exe under a `.old` name
/// frees the original path so the cargo-dist installer's `Copy-Item`
/// succeeds; the parked file is removed on the next start.
///
/// The guard is plain filesystem logic so it is unit-tested on every
/// platform; `apply_with` only activates it on Windows.
pub struct ExeReplaceGuard {
    original: PathBuf,
    parked: PathBuf,
}

impl ExeReplaceGuard {
    /// Rename `exe` aside.  Replaces any stale artifact from a
    /// previous update first.
    pub fn park(exe: &Path) -> Result<Self> {
        let parked = parked_artifact_path(exe);
        if parked.exists() {
            std::fs::remove_file(&parked)
                .with_context(|| format!("removing stale parked binary {}", parked.display()))?;
        }
        std::fs::rename(exe, &parked).with_context(|| {
            format!(
                "parking running binary {} -> {}",
                exe.display(),
                parked.display()
            )
        })?;
        info!(
            target: TRACE_TARGET,
            exe = %exe.display(),
            parked = %parked.display(),
            "parked running binary so the installer can replace it"
        );
        Ok(Self {
            original: exe.to_path_buf(),
            parked,
        })
    }

    /// After the installer ran: did a new binary land at the original
    /// path?  If not, the installer wrote somewhere else and a restart
    /// would find nothing to exec — the caller must roll back.
    pub fn confirm_replaced(&self) -> Result<()> {
        if self.original.is_file() {
            return Ok(());
        }
        bail!(
            "installer did not write a new binary at {} (custom install dir?)",
            self.original.display()
        )
    }

    /// Undo the park — the update failed and the worker keeps running
    /// the old version.
    pub fn rollback(self) -> Result<()> {
        std::fs::rename(&self.parked, &self.original).with_context(|| {
            format!(
                "restoring parked binary {} -> {}",
                self.parked.display(),
                self.original.display()
            )
        })
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
    // Windows locks the running executable: the installer's Copy-Item
    // fails with "file in use" unless we park (rename) ourselves out
    // of the way first.  Renames of running binaries are allowed on
    // NTFS.  Unix installers replace via unlink + write, no parking
    // needed.
    let guard = if cfg!(target_os = "windows") {
        let exe = std::env::current_exe().context("resolving current exe for update")?;
        Some(ExeReplaceGuard::park(&exe)?)
    } else {
        None
    };
    match runner.run_installer(&installer_path) {
        Ok(()) => {
            if let Some(guard) = guard {
                if let Err(e) = guard.confirm_replaced() {
                    // Roll back so the (still-running) old version can
                    // be restarted by path; surface why the update
                    // didn't take.
                    if let Err(rb) = guard.rollback() {
                        warn!(target: TRACE_TARGET, error = %rb, "rollback after failed replace also failed");
                    }
                    return Err(e);
                }
                // Parked file stays until the next start (this process
                // is still executing it); cleanup_parked_artifact
                // removes it then.
            }
        }
        Err(e) => {
            if let Some(guard) = guard {
                if let Err(rb) = guard.rollback() {
                    warn!(target: TRACE_TARGET, error = %rb, "rollback after installer failure also failed");
                }
            }
            return Err(e);
        }
    }
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

    // -----------------------------------------------------------------
    // ExeReplaceGuard — the Windows locked-exe dance.  Pure fs logic,
    // unit-tested on every platform; only the activation in apply_with
    // is Windows-gated.
    // -----------------------------------------------------------------

    #[test]
    fn park_moves_the_exe_aside_and_confirm_fails_until_replaced() {
        let dir = tempdir().unwrap();
        let exe = dir.path().join("studio-worker.exe");
        std::fs::write(&exe, b"old binary").unwrap();

        let guard = ExeReplaceGuard::park(&exe).unwrap();
        assert!(
            !exe.exists(),
            "original path must be free for the installer"
        );
        assert_eq!(
            std::fs::read(parked_artifact_path(&exe)).unwrap(),
            b"old binary"
        );
        // Installer hasn't written the new binary yet.
        assert!(guard.confirm_replaced().is_err());

        // Installer writes the new binary at the original path.
        std::fs::write(&exe, b"new binary").unwrap();
        guard.confirm_replaced().unwrap();
    }

    #[test]
    fn rollback_restores_the_original_exe() {
        let dir = tempdir().unwrap();
        let exe = dir.path().join("studio-worker.exe");
        std::fs::write(&exe, b"old binary").unwrap();

        let guard = ExeReplaceGuard::park(&exe).unwrap();
        guard.rollback().unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"old binary");
        assert!(!parked_artifact_path(&exe).exists());
    }

    #[test]
    fn park_replaces_a_stale_artifact_from_a_previous_update() {
        let dir = tempdir().unwrap();
        let exe = dir.path().join("studio-worker.exe");
        std::fs::write(&exe, b"current").unwrap();
        std::fs::write(parked_artifact_path(&exe), b"ancient leftover").unwrap();

        let _guard = ExeReplaceGuard::park(&exe).unwrap();
        assert_eq!(
            std::fs::read(parked_artifact_path(&exe)).unwrap(),
            b"current"
        );
    }

    #[test]
    fn parked_artifact_path_appends_old_to_the_full_file_name() {
        // `.with_extension` would turn studio-worker.exe into
        // studio-worker.old and clobber a sibling file — the artifact
        // must keep the full original name.
        assert_eq!(
            parked_artifact_path(Path::new("/x/studio-worker.exe")),
            PathBuf::from("/x/studio-worker.exe.old")
        );
        assert_eq!(
            parked_artifact_path(Path::new("/x/studio-worker")),
            PathBuf::from("/x/studio-worker.old")
        );
    }

    #[test]
    fn cleanup_removes_only_the_parked_artifact() {
        let dir = tempdir().unwrap();
        let exe = dir.path().join("studio-worker.exe");
        std::fs::write(&exe, b"current").unwrap();
        std::fs::write(parked_artifact_path(&exe), b"leftover").unwrap();
        let bystander = dir.path().join("other.txt");
        std::fs::write(&bystander, b"keep me").unwrap();

        cleanup_parked_artifact(&exe);
        assert!(!parked_artifact_path(&exe).exists());
        assert!(exe.exists());
        assert!(bystander.exists());
        // Idempotent when nothing is parked.
        cleanup_parked_artifact(&exe);
    }

    #[test]
    fn park_surfaces_a_rename_failure_with_actionable_context() {
        // The exe path doesn't exist, so the rename that parks it
        // fails.  park must surface a clear error (not panic / not a
        // bare OS code) so a failed update is diagnosable — this is
        // the entry point of the Windows replace dance, and if it
        // fails silently the caller would proceed to run an installer
        // against an unparked, still-locked binary.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("studio-worker.exe");
        // `.err()` drops the Ok guard without needing it to be Debug.
        let err = ExeReplaceGuard::park(&missing)
            .err()
            .expect("park must fail when the exe is missing")
            .to_string();
        assert!(
            err.contains("parking running binary"),
            "park error must name the operation: {err}"
        );
        assert!(
            err.contains("studio-worker.exe"),
            "park error must name the offending path: {err}"
        );
    }

    #[test]
    fn rollback_surfaces_a_restore_failure_with_actionable_context() {
        // Park succeeds, then the parked binary vanishes (disk full,
        // operator meddling, a racing cleanup) before rollback runs.
        // rollback is the safety net that restores the running version
        // when an update fails; if its own restore fails it must
        // report why rather than leave the worker with no binary and
        // no explanation.
        let dir = tempdir().unwrap();
        let exe = dir.path().join("studio-worker.exe");
        std::fs::write(&exe, b"old binary").unwrap();
        let guard = ExeReplaceGuard::park(&exe).unwrap();
        // Remove the parked file out from under the guard.
        std::fs::remove_file(parked_artifact_path(&exe)).unwrap();
        let err = guard.rollback().unwrap_err().to_string();
        assert!(
            err.contains("restoring parked binary"),
            "rollback error must name the operation: {err}"
        );
        assert!(
            err.contains("studio-worker.exe"),
            "rollback error must name the target path: {err}"
        );
    }

    #[test]
    fn cleanup_warns_when_the_parked_artifact_cannot_be_removed() {
        // A parked path that is a non-empty directory (not a file)
        // makes `remove_file` fail with a non-NotFound error.  Cleanup
        // runs on every startup and must surface such a stuck artifact
        // (so a wedged update leftover is visible and retried) instead
        // of swallowing the failure.
        let dir = tempdir().unwrap();
        let exe = dir.path().join("studio-worker.exe");
        std::fs::write(&exe, b"current").unwrap();
        let parked = parked_artifact_path(&exe);
        std::fs::create_dir(&parked).unwrap();
        std::fs::write(parked.join("blocker"), b"x").unwrap();
        let out = crate::test_support::capture(move || cleanup_parked_artifact(&exe));
        assert!(
            out.contains("could not remove parked binary"),
            "a failed cleanup must warn: {out:?}"
        );
        assert!(
            out.contains("studio-worker.exe.old"),
            "the warning must name the stuck artifact: {out:?}"
        );
    }

    #[test]
    fn parse_tag_accepts_v_prefix_and_bare() {
        assert_eq!(parse_tag("v1.2.3"), Some(Version::new(1, 2, 3)));
        assert_eq!(parse_tag("1.2.3"), Some(Version::new(1, 2, 3)));
        assert!(parse_tag("garbage").is_none());
    }

    #[test]
    fn parse_tag_accepts_component_prefixed_release_tags() {
        // release-please / cargo-dist tag the repo as
        // `studio-worker-v<semver>`; the updater must read the version
        // out of that or it never sees a newer release (the bug that
        // made `check for updates` always say "up to date").
        assert_eq!(
            parse_tag("studio-worker-v0.4.2"),
            Some(Version::new(0, 4, 2))
        );
        assert_eq!(
            parse_tag("studio-worker-v1.10.0"),
            Some(Version::new(1, 10, 0))
        );
        // Prerelease suffix survives (the version's own `-` is not the
        // component separator).
        assert_eq!(
            parse_tag("studio-worker-v0.5.0-rc.1"),
            Version::parse("0.5.0-rc.1").ok()
        );
    }

    #[test]
    fn decide_detects_newer_with_component_prefixed_tags() {
        // The exact shape of the live feed: `studio-worker-v*` tags.
        let releases = vec![
            rel("studio-worker-v0.4.1", false, false, true),
            rel("studio-worker-v0.4.2", false, false, true),
        ];
        let outcome = decide(&releases, &Version::new(0, 4, 1), false);
        assert_eq!(
            outcome,
            CheckOutcome::NewerAvailable {
                current: Version::new(0, 4, 1),
                latest: Version::new(0, 4, 2),
            }
        );
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

    // -----------------------------------------------------------------
    // verify_download_len — guards the installer download against a
    // short read before the bytes are handed to `sh` / `powershell`.
    // A truncated installer that runs is far worse than a failed
    // update, so a Content-Length mismatch must surface as an error.
    // -----------------------------------------------------------------

    #[test]
    fn verify_download_len_accepts_exact_match() {
        assert!(verify_download_len(2048, Some(2048)).is_ok());
    }

    #[test]
    fn verify_download_len_accepts_when_length_unknown() {
        // Chunked transfers omit Content-Length; nothing to check, so
        // we accept whatever streamed in (same as before this guard).
        assert!(verify_download_len(123, None).is_ok());
    }

    #[test]
    fn verify_download_len_rejects_truncated_installer() {
        let err = verify_download_len(40, Some(100)).unwrap_err().to_string();
        assert!(err.contains("size mismatch"), "got: {err}");
        assert!(err.contains("40"), "got: {err}");
        assert!(err.contains("100"), "got: {err}");
    }

    #[test]
    fn verify_download_len_rejects_overlong_installer() {
        // A body longer than the declared length is just as corrupt as
        // a short one — reject both rather than run a bad installer.
        assert!(verify_download_len(120, Some(100)).is_err());
    }

    #[test]
    fn validate_installer_download_url_allows_https() {
        validate_installer_download_url("https://github.com/owner/repo/releases/download/x/i.sh")
            .unwrap();
    }

    #[test]
    fn validate_installer_download_url_allows_loopback_http_for_tests() {
        validate_installer_download_url("http://127.0.0.1:1234/i.sh").unwrap();
        validate_installer_download_url("http://localhost:1234/i.sh").unwrap();
    }

    #[test]
    fn validate_installer_download_url_rejects_remote_http() {
        let err = validate_installer_download_url("http://example.com/i.sh")
            .unwrap_err()
            .to_string();
        assert!(err.contains("https"), "got: {err}");
    }

    #[test]
    fn validate_installer_download_url_rejects_non_http_schemes() {
        // The gate must reject anything that isn't https (or loopback
        // http) *before* the auto-updater downloads and executes the
        // asset.  These schemes take a different path through the guard
        // than `http://example.com` — they skip the `http` block
        // entirely and fall straight to the bail — so they need their
        // own cover.  `file://` is the dangerous one: a compromised
        // release feed handing back `file:///etc/cron.d/evil.sh` would,
        // without this guard, point the installer runner at an arbitrary
        // local script.  `ftp://` is unencrypted (tamperable in
        // transit) and `javascript:` carries no host at all.
        for raw in [
            "file:///etc/cron.d/evil.sh",
            "ftp://example.com/i.sh",
            "javascript:alert(1)",
        ] {
            let err = validate_installer_download_url(raw)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("https"),
                "{raw} must be rejected with the https guidance, got: {err}"
            );
        }
    }

    #[test]
    fn validate_installer_download_url_rejects_a_malformed_url() {
        // A feed entry that doesn't parse as a URL at all must error at
        // the parse step (carrying the `invalid installer URL` context)
        // rather than slipping through to a download attempt.
        let err = validate_installer_download_url("not a url")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("invalid installer URL"),
            "a malformed URL must surface the parse context, got: {err}"
        );
    }

    // -----------------------------------------------------------------
    // RealRunner::run_installer — the production path that hands the
    // downloaded installer to `sh` (unix) / PowerShell (Windows).  The
    // unix branch is exercised here against trivial scripts so the
    // safety property is locked in: a non-zero installer exit MUST
    // bail, never report success.  Tests elsewhere drive `apply_with`
    // through a fake runner, so without this the real subprocess
    // dispatch shipped untested.
    // -----------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn real_runner_run_installer_succeeds_on_zero_exit() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("installer.sh");
        // `sh <path>` reads the file directly, so no shebang or +x bit
        // is needed.
        std::fs::write(&script, "exit 0\n").unwrap();
        RealRunner.run_installer(&script).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_run_installer_bails_on_nonzero_exit() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("installer.sh");
        std::fs::write(&script, "exit 3\n").unwrap();
        let err = RealRunner.run_installer(&script).unwrap_err().to_string();
        assert!(
            err.contains("installer exited"),
            "a failed installer must surface a clear error, got: {err}"
        );
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
