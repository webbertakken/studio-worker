//! Updating the worker from inside itself (docs/runtime/updates.md, "Self-install"): download the
//! release's archive for this target, verify it against the release's sha256 sidecar, take the
//! binary out of the archive and swap it in place of the running one. No shell, no PowerShell and
//! no installer script run, so a managed Windows machine whose Attack Surface Reduction rules block
//! a script host changing an executable still updates.
//!
//! Every step logs under `studio_worker::update` with `op = "self_install"` and its step; a failure
//! the operating system caused (access denied, blocked by policy, flagged by the antivirus) reads
//! "update blocked by the OS: <reason>" ([`describe_failure`]).

use crate::types::GithubRelease;
use crate::update::{parse_checksum_file, verify_file_sha256, ExeReplaceGuard};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tracing::info;

const TRACE_TARGET: &str = "studio_worker::update";

/// The binary inside a release archive.
pub fn binary_name() -> &'static str {
    if cfg!(windows) {
        "studio-worker.exe"
    } else {
        "studio-worker"
    }
}

/// The release archive this self-install takes: `studio-worker-<target>.zip`.
pub fn archive_asset_name(target: &str) -> String {
    format!("studio-worker-{target}.zip")
}

/// What a self-install needs from the outside world; tests inject a fake.
pub trait SelfInstallIo {
    /// Download `url` to `dest` (https, GitHub only).
    fn download(&self, url: &str, dest: &Path) -> Result<()>;
    /// The `<asset>.sha256` sidecar's text, or `None` when the release publishes none.
    fn fetch_checksum(&self, url: &str) -> Result<Option<String>>;
}

/// Copy the entry named `name` (at the archive's root or in a folder) out of a zip into `dest`.
pub fn extract_named_file(archive: &Path, name: &str, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive)
        .with_context(|| format!("opening the release archive {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("reading the release archive")?;
    let index = (0..zip.len())
        .find(|&index| {
            zip.by_index(index)
                .ok()
                .and_then(|entry| {
                    entry
                        .enclosed_name()
                        .and_then(|path| path.file_name().map(|n| n == name))
                })
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("the release archive holds no {name}"))?;
    let mut entry = zip.by_index(index)?;
    let mut out = std::fs::File::create(dest)
        .with_context(|| format!("writing the new binary to {}", dest.display()))?;
    std::io::copy(&mut entry, &mut out).context("extracting the new binary")?;
    Ok(())
}

/// Replace `exe` with the release's binary for `target`, in this process. The running binary is
/// parked beside itself (`<exe>.old`, removed on the next start); any failure after that puts it
/// back, so the worker keeps the version it runs.
pub fn self_install(
    release: &GithubRelease,
    target: &str,
    exe: &Path,
    io: &dyn SelfInstallIo,
) -> Result<()> {
    let asset = archive_asset_name(target);
    let url = release
        .assets
        .iter()
        .find(|candidate| candidate.name == asset)
        .map(|candidate| candidate.browser_download_url.clone())
        .ok_or_else(|| anyhow!("release {} has no {asset}", release.tag_name))?;
    let work = tempfile::tempdir().context("creating a working folder for the update")?;
    let archive = work.path().join(&asset);
    info!(target: TRACE_TARGET, op = "self_install", step = "download", url = %url, "downloading the release archive");
    io.download(&url, &archive)?;
    // A self-install verifies or refuses: there is no installer to fall back on.
    let sidecar = io
        .fetch_checksum(&format!("{url}.sha256"))?
        .ok_or_else(|| {
            anyhow!(
                "release {} publishes no checksum for {asset}; refusing to install it",
                release.tag_name
            )
        })?;
    let expected = parse_checksum_file(&sidecar).ok_or_else(|| {
        anyhow!("the checksum sidecar for {asset} holds no sha256; refusing to install it")
    })?;
    verify_file_sha256(&archive, &expected)?;
    info!(target: TRACE_TARGET, op = "self_install", step = "verify", asset = %asset, "release archive matches its checksum");
    let new_binary: PathBuf = work.path().join(binary_name());
    extract_named_file(&archive, binary_name(), &new_binary)?;
    info!(target: TRACE_TARGET, op = "self_install", step = "extract", "new binary taken out of the archive");
    let guard = ExeReplaceGuard::park(exe)?;
    let placed = std::fs::copy(&new_binary, exe)
        .with_context(|| format!("placing the new binary at {}", exe.display()))
        .and_then(|_| guard.confirm_replaced());
    if let Err(error) = placed {
        if let Err(rollback) = guard.rollback() {
            tracing::warn!(target: TRACE_TARGET, op = "self_install", step = "rollback", error = %rollback, "could not put the old binary back");
        }
        return Err(error);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(exe, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", exe.display()))?;
    }
    info!(target: TRACE_TARGET, op = "self_install", step = "swap", exe = %exe.display(), "new binary in place; restart pending");
    Ok(())
}

/// Windows errors that mean the operating system, not the update, stopped the swap.
const OS_BLOCKS: [(i32, &str); 5] = [
    (5, "access denied"),
    (32, "the file is in use by another process"),
    (225, "the antivirus flagged the file"),
    (1260, "blocked by group policy"),
    (4551, "blocked by an application control policy"),
];

/// How an update failure reads in the worker's log: "update blocked by the OS: <reason>" when the
/// operating system refused a file operation, else "update failed: <error>".
pub fn describe_failure(error: &anyhow::Error) -> String {
    for cause in error.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            // Windows' own codes; elsewhere a raw code means something else (5 is EIO on Linux).
            let known = io
                .raw_os_error()
                .filter(|_| cfg!(windows))
                .and_then(|code| OS_BLOCKS.iter().find(|(known, _)| *known == code))
                .map(|(_, reason)| (*reason).to_string());
            if let Some(reason) = known {
                return format!("update blocked by the OS: {reason} ({error:#})");
            }
            if io.kind() == std::io::ErrorKind::PermissionDenied {
                return format!("update blocked by the OS: permission denied ({error:#})");
            }
        }
    }
    format!("update failed: {error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GithubReleaseAsset;
    use std::cell::RefCell;
    use std::io::Write as _;

    fn zip_with(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("release.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        path
    }

    fn sha256_of(path: &Path) -> String {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path).unwrap();
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    struct FakeIo {
        archive: PathBuf,
        checksum: Option<String>,
        downloads: RefCell<Vec<String>>,
    }

    impl SelfInstallIo for FakeIo {
        fn download(&self, url: &str, dest: &Path) -> Result<()> {
            self.downloads.borrow_mut().push(url.to_string());
            std::fs::copy(&self.archive, dest)?;
            Ok(())
        }
        fn fetch_checksum(&self, _url: &str) -> Result<Option<String>> {
            Ok(self.checksum.clone())
        }
    }

    fn release(target: &str) -> GithubRelease {
        let name = archive_asset_name(target);
        GithubRelease {
            tag_name: "studio-worker-v0.4.22".into(),
            prerelease: false,
            draft: false,
            assets: vec![GithubReleaseAsset {
                browser_download_url: format!("https://github.com/x/y/releases/download/t/{name}"),
                name,
            }],
        }
    }

    fn setup(
        entries: &[(&str, &[u8])],
        checksum: Option<&str>,
    ) -> (tempfile::TempDir, PathBuf, FakeIo) {
        let dir = tempfile::tempdir().unwrap();
        let archive = zip_with(dir.path(), entries);
        let exe = dir.path().join("bin").join(binary_name());
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"old").unwrap();
        let checksum = checksum
            .map(str::to_string)
            .or_else(|| Some(sha256_of(&archive)));
        let io = FakeIo {
            archive,
            checksum,
            downloads: RefCell::new(vec![]),
        };
        (dir, exe, io)
    }

    #[test]
    fn swaps_in_the_verified_binary_without_any_script() {
        let (_dir, exe, io) = setup(
            &[(&format!("studio-worker-t/{}", binary_name()), b"new")],
            None,
        );
        self_install(&release("t"), "t", &exe, &io).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(
            std::fs::read(crate::update::parked_artifact_path(&exe)).unwrap(),
            b"old"
        );
        assert_eq!(io.downloads.borrow().len(), 1);
        assert!(io.downloads.borrow()[0].ends_with("studio-worker-t.zip"));
    }

    #[test]
    fn refuses_an_archive_that_does_not_match_its_checksum_and_keeps_the_old_binary() {
        let (_dir, exe, io) = setup(&[(binary_name(), b"new")], Some(&"0".repeat(64)));
        let error = self_install(&release("t"), "t", &exe, &io).unwrap_err();
        assert!(
            format!("{error:#}").contains("sha256 mismatch"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn refuses_a_release_without_a_checksum() {
        let (_dir, exe, mut io) = setup(&[(binary_name(), b"new")], None);
        io.checksum = None;
        let error = self_install(&release("t"), "t", &exe, &io).unwrap_err();
        assert!(
            format!("{error:#}").contains("publishes no checksum"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn refuses_an_archive_without_the_binary() {
        let (_dir, exe, io) = setup(&[("README.md", b"hi")], None);
        let error = self_install(&release("t"), "t", &exe, &io).unwrap_err();
        assert!(format!("{error:#}").contains("holds no"), "{error:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn refuses_a_release_without_this_targets_archive() {
        let (_dir, exe, io) = setup(&[(binary_name(), b"new")], None);
        let error = self_install(&release("other"), "t", &exe, &io).unwrap_err();
        assert!(
            format!("{error:#}").contains("has no studio-worker-t.zip"),
            "{error:#}"
        );
    }

    #[test]
    fn names_an_os_refusal_as_blocked() {
        #[cfg(windows)]
        {
            let denied = anyhow::Error::new(std::io::Error::from_raw_os_error(5))
                .context("placing the new binary");
            assert!(
                describe_failure(&denied).starts_with("update blocked by the OS: access denied")
            );
        }
        let permission =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(describe_failure(&permission)
            .starts_with("update blocked by the OS: permission denied"));
        let other = anyhow!("release 1.2.3 not present in feed");
        assert_eq!(
            describe_failure(&other),
            "update failed: release 1.2.3 not present in feed"
        );
    }
}
