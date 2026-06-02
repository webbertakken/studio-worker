//! Auto-provision the stable-diffusion.cpp `sd-cli` binary.
//!
//! The [`sdcpp`](crate::engine::sdcpp) engine subprocess-invokes
//! `sd-cli` per image job.  Model weights already download on demand
//! (see [`download`](crate::engine::download)); this module fills the
//! remaining gap so a fresh worker is turnkey: on the first image job,
//! if no `sd-cli` is resolvable, we download the platform's prebuilt
//! stable-diffusion.cpp **Vulkan** build (universal across NVIDIA /
//! AMD / Intel, ~37 MB) and extract it into `<models_root>/bin/`, the
//! PATH-free slot the resolver already prefers.
//!
//! The upstream release is pinned for reproducibility.  Overrides:
//!
//! * `STUDIO_WORKER_SDCPP_RELEASE` — a `master-<n>-<sha>` tag to fetch
//!   instead of the pinned default.
//! * `STUDIO_WORKER_SDCPP_URL` — a full zip URL (skips tag/asset
//!   resolution entirely; used by tests and air-gapped mirrors).
//!
//! Windows resolves the sibling `stable-diffusion.dll` automatically
//! (same dir as the `.exe`).  Linux / macOS need the loader pointed at
//! the binary's dir — see [`library_path_env`], applied per job.

use crate::engine::download;
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Tracing target for provisioning.  Stable so operators can filter
/// with `RUST_LOG=studio_worker::engine::sd_provision=info`.
const TRACE_TARGET: &str = "studio_worker::engine::sd_provision";

/// Pinned, known-good upstream release.  Bump deliberately after
/// verifying a newer build still serves our model set.  Overridable
/// per box via `STUDIO_WORKER_SDCPP_RELEASE`.
const DEFAULT_RELEASE_TAG: &str = "master-669-2d40a8b";

/// Env override for the release tag.
const RELEASE_ENV: &str = "STUDIO_WORKER_SDCPP_RELEASE";
/// Env override for the full zip URL (tests / air-gapped mirrors).
const URL_ENV: &str = "STUDIO_WORKER_SDCPP_URL";

/// Platform binary name for stable-diffusion.cpp's CLI.
pub fn binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "sd-cli.exe"
    } else {
        "sd-cli"
    }
}

/// Platform shared-library name shipped alongside the binaries.
fn library_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "stable-diffusion.dll"
    } else if cfg!(target_os = "macos") {
        "libstable-diffusion.dylib"
    } else {
        "libstable-diffusion.so"
    }
}

/// The release tag to provision — env override or the pinned default.
fn release_tag() -> String {
    std::env::var(RELEASE_ENV).unwrap_or_else(|_| DEFAULT_RELEASE_TAG.to_string())
}

/// The short commit sha embedded in asset filenames is the trailing
/// `-`-segment of the release tag (`master-669-2d40a8b` -> `2d40a8b`).
fn sha_from_tag(tag: &str) -> Result<&str> {
    match tag.rsplit_once('-') {
        Some((_, sha)) if !sha.is_empty() => Ok(sha),
        _ => Err(anyhow!("release tag {tag:?} has no '-<sha>' segment")),
    }
}

/// Platform asset suffix (the part between `bin-` and `.zip`).  Vulkan
/// is the universal GPU backend — one build serves NVIDIA, AMD and
/// Intel — so we pick it for every supported target.
fn asset_suffix(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Ok("win-vulkan-x64"),
        ("linux", "x86_64") => Ok("Linux-Ubuntu-24.04-x86_64-vulkan"),
        ("macos", "aarch64") => Ok("Darwin-macOS-15.7.7-arm64"),
        _ => bail!(
            "no prebuilt stable-diffusion.cpp binary for {os}/{arch}; \
             install sd-cli manually — see docs/operations/sd-cli-install.md"
        ),
    }
}

/// Build the asset filename for `tag` on `os`/`arch`.
fn asset_name(tag: &str, os: &str, arch: &str) -> Result<String> {
    let sha = sha_from_tag(tag)?;
    let suffix = asset_suffix(os, arch)?;
    Ok(format!("sd-master-{sha}-bin-{suffix}.zip"))
}

/// The full release-download URL for `tag` on `os`/`arch`.
fn download_url(tag: &str, os: &str, arch: &str) -> Result<String> {
    let asset = asset_name(tag, os, arch)?;
    Ok(format!(
        "https://github.com/leejet/stable-diffusion.cpp/releases/download/{tag}/{asset}"
    ))
}

/// Resolve the zip URL to fetch: the `STUDIO_WORKER_SDCPP_URL`
/// override if set, otherwise the pinned/overridden release for this
/// host's platform.
fn resolve_url() -> Result<String> {
    if let Ok(url) = std::env::var(URL_ENV) {
        if !url.is_empty() {
            return Ok(url);
        }
    }
    download_url(&release_tag(), std::env::consts::OS, std::env::consts::ARCH)
}

/// If a stable-diffusion shared library sits next to `sd_cli`, return
/// the `(env-var, dir)` the per-job `Command` must set so the dynamic
/// linker finds it.  Returns `None` on Windows (sibling DLLs resolve
/// automatically) and when no sibling library is present (e.g. an
/// operator's wrapper-script install manages its own load path).
pub fn library_path_env(sd_cli: &Path) -> Option<(&'static str, PathBuf)> {
    if cfg!(target_os = "windows") {
        return None;
    }
    let dir = sd_cli.parent()?;
    if dir.join(library_name()).is_file() {
        let var = if cfg!(target_os = "macos") {
            "DYLD_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        Some((var, dir.to_path_buf()))
    } else {
        None
    }
}

/// Extract every file in the zip at `zip_path` into `dest_dir`,
/// flattened to bare file names.  Flattening is also the zip-slip
/// defence: `Path::file_name` drops every directory component, so a
/// crafted `../../etc/passwd` entry can only ever land as `passwd`
/// inside `dest_dir`.  Returns the number of files written.
#[cfg_attr(coverage_nightly, coverage(off))]
fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<usize> {
    let file =
        std::fs::File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", zip_path.display()))?;
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let mut written = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let Some(file_name) = Path::new(entry.name()).file_name().map(|n| n.to_owned()) else {
            warn!(
                target: TRACE_TARGET,
                op = "extract",
                name = entry.name(),
                "skipping zip entry with no file name"
            );
            continue;
        };
        let out = dest_dir.join(&file_name);
        let mode = entry.unix_mode();
        let mut writer =
            std::fs::File::create(&out).with_context(|| format!("creating {}", out.display()))?;
        std::io::copy(&mut entry, &mut writer)
            .with_context(|| format!("writing {}", out.display()))?;
        drop(writer);
        apply_unix_mode(&out, mode)?;
        written += 1;
    }
    Ok(written)
}

/// Apply the zip entry's unix mode when present.  No-op off unix.
#[cfg(unix)]
fn apply_unix_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_unix_mode(_path: &Path, _mode: Option<u32>) -> Result<()> {
    Ok(())
}

/// Ensure `path` is executable (owner +x) on unix.  No-op off unix.
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod +x {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Publish every file from `staging` into `target` (created if
/// needed), overwriting existing files.  Prefers an intra-filesystem
/// rename (instant for the ~100 MB library) and falls back to a copy
/// across filesystems.
fn install_dir(staging: &Path, target: &Path) -> Result<usize> {
    std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    let mut moved = 0usize;
    for entry in
        std::fs::read_dir(staging).with_context(|| format!("reading {}", staging.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let from = entry.path();
        let to = target.join(entry.file_name());
        if to.exists() {
            std::fs::remove_file(&to).with_context(|| format!("replacing {}", to.display()))?;
        }
        if std::fs::rename(&from, &to).is_err() {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
        }
        moved += 1;
    }
    Ok(moved)
}

/// Ensure `sd-cli` is installed under `<models_root>/bin/`, downloading
/// and extracting the platform's stable-diffusion.cpp build when it's
/// missing.  Returns the resolved binary path.  Idempotent: a binary
/// already present short-circuits the download.
///
/// Excluded from coverage: drives a real network download + filesystem
/// extraction.  The pure pieces it composes ([`asset_name`],
/// [`download_url`], [`install_dir`], [`library_path_env`]) and the
/// full path against a served fake zip are covered by tests.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn provision(models_root: &Path) -> Result<PathBuf> {
    let target_dir = models_root.join("bin");
    let binary = target_dir.join(binary_name());
    if binary.is_file() {
        return Ok(binary);
    }

    let url = resolve_url()?;
    info!(
        target: TRACE_TARGET,
        op = "provision",
        url = %url,
        dest = %target_dir.display(),
        "sd-cli not found; provisioning stable-diffusion.cpp"
    );

    std::fs::create_dir_all(models_root)
        .with_context(|| format!("creating {}", models_root.display()))?;
    let stamp = format!("{}-{}", std::process::id(), now_nanos());
    let zip_path = models_root.join(format!(".sd-cli-{stamp}.zip"));
    let staging = models_root.join(format!(".sd-cli-staging-{stamp}"));

    let result = (|| -> Result<PathBuf> {
        download::download_file(&url, &zip_path)
            .with_context(|| format!("downloading sd-cli zip from {url}"))?;
        let count = extract_zip(&zip_path, &staging)?;
        let staged_binary = staging.join(binary_name());
        if !staged_binary.is_file() {
            bail!(
                "downloaded sd-cli zip from {url} did not contain {} (extracted {count} files)",
                binary_name()
            );
        }
        install_dir(&staging, &target_dir)?;
        make_executable(&binary)?;
        if !binary.is_file() {
            bail!("sd-cli install left no binary at {}", binary.display());
        }
        Ok(binary.clone())
    })();

    // Best-effort cleanup of the scratch zip + staging dir on every
    // exit path so a failed provision can't leave half-extracted
    // multi-hundred-MB files filling the disk.
    let _ = std::fs::remove_file(&zip_path);
    let _ = std::fs::remove_dir_all(&staging);

    match &result {
        Ok(path) => info!(
            target: TRACE_TARGET,
            op = "provision",
            path = %path.display(),
            "sd-cli provisioned"
        ),
        Err(e) => warn!(
            target: TRACE_TARGET,
            op = "provision",
            error = %e,
            "sd-cli provisioning failed"
        ),
    }
    result
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn now_nanos() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn sha_from_tag_takes_trailing_segment() {
        assert_eq!(sha_from_tag("master-669-2d40a8b").unwrap(), "2d40a8b");
        assert_eq!(sha_from_tag("master-1-abc").unwrap(), "abc");
    }

    #[test]
    fn sha_from_tag_rejects_a_tag_without_a_sha() {
        assert!(sha_from_tag("master").is_err());
        assert!(sha_from_tag("trailing-").is_err());
    }

    #[test]
    fn asset_suffix_picks_vulkan_for_supported_targets() {
        assert_eq!(asset_suffix("windows", "x86_64").unwrap(), "win-vulkan-x64");
        assert_eq!(
            asset_suffix("linux", "x86_64").unwrap(),
            "Linux-Ubuntu-24.04-x86_64-vulkan"
        );
        assert_eq!(
            asset_suffix("macos", "aarch64").unwrap(),
            "Darwin-macOS-15.7.7-arm64"
        );
    }

    #[test]
    fn asset_suffix_rejects_unsupported_targets_with_guidance() {
        let err = asset_suffix("linux", "aarch64").unwrap_err().to_string();
        assert!(err.contains("no prebuilt"), "got: {err}");
        assert!(
            err.contains("sd-cli-install.md"),
            "points to the doc: {err}"
        );
        assert!(asset_suffix("freebsd", "x86_64").is_err());
        assert!(asset_suffix("macos", "x86_64").is_err());
    }

    #[test]
    fn asset_name_embeds_sha_and_platform() {
        assert_eq!(
            asset_name("master-669-2d40a8b", "windows", "x86_64").unwrap(),
            "sd-master-2d40a8b-bin-win-vulkan-x64.zip"
        );
        assert_eq!(
            asset_name("master-669-2d40a8b", "linux", "x86_64").unwrap(),
            "sd-master-2d40a8b-bin-Linux-Ubuntu-24.04-x86_64-vulkan.zip"
        );
    }

    #[test]
    fn download_url_targets_the_upstream_release() {
        let url = download_url("master-669-2d40a8b", "windows", "x86_64").unwrap();
        let expected = concat!(
            "https://github.com/leejet/stable-diffusion.cpp/releases/download/",
            "master-669-2d40a8b/sd-master-2d40a8b-bin-win-vulkan-x64.zip"
        );
        assert_eq!(url, expected);
    }

    #[test]
    fn install_dir_moves_files_and_overwrites() {
        let staging = tempdir().unwrap();
        let target = tempdir().unwrap();
        std::fs::write(staging.path().join("sd-cli"), b"new-binary").unwrap();
        std::fs::write(staging.path().join("libstable-diffusion.so"), b"lib").unwrap();
        // A stale file in target must be overwritten, not duplicated.
        std::fs::write(target.path().join("sd-cli"), b"old-binary").unwrap();

        let moved = install_dir(staging.path(), target.path()).unwrap();
        assert_eq!(moved, 2);
        assert_eq!(
            std::fs::read(target.path().join("sd-cli")).unwrap(),
            b"new-binary"
        );
        assert_eq!(
            std::fs::read(target.path().join("libstable-diffusion.so")).unwrap(),
            b"lib"
        );
        // Files were moved, so staging is now empty of them.
        assert!(!staging.path().join("sd-cli").exists());
    }

    #[test]
    fn extract_zip_flattens_and_defuses_zip_slip() {
        let dir = tempdir().unwrap();
        let zip_path = dir.path().join("test.zip");
        // Build a zip with a nested + a path-traversal entry; both must
        // land flat inside dest, never escaping it.
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zw.start_file("sd-cli", opts).unwrap();
            zw.write_all(b"binary").unwrap();
            zw.start_file("nested/libstable-diffusion.so", opts)
                .unwrap();
            zw.write_all(b"lib").unwrap();
            zw.start_file("../../escape.txt", opts).unwrap();
            zw.write_all(b"evil").unwrap();
            zw.finish().unwrap();
        }
        let dest = dir.path().join("out");
        let count = extract_zip(&zip_path, &dest).unwrap();
        assert_eq!(count, 3);
        assert_eq!(std::fs::read(dest.join("sd-cli")).unwrap(), b"binary");
        assert_eq!(
            std::fs::read(dest.join("libstable-diffusion.so")).unwrap(),
            b"lib"
        );
        // The traversal entry was flattened into dest, not written to a
        // parent directory.
        assert!(dest.join("escape.txt").is_file());
        assert!(!dir.path().join("escape.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extract_zip_preserves_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let zip_path = dir.path().join("exec.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o755);
            zw.start_file("sd-cli", opts).unwrap();
            zw.write_all(b"#!/bin/sh\n").unwrap();
            zw.finish().unwrap();
        }
        let dest = dir.path().join("out");
        extract_zip(&zip_path, &dest).unwrap();
        let mode = std::fs::metadata(dest.join("sd-cli"))
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "exec bit must survive: {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn library_path_env_points_loader_at_sibling_lib() {
        let dir = tempdir().unwrap();
        let sd_cli = dir.path().join(binary_name());
        std::fs::write(&sd_cli, b"bin").unwrap();
        // No sibling library yet -> nothing to set.
        assert!(library_path_env(&sd_cli).is_none());
        // Drop the platform library next to it.
        std::fs::write(dir.path().join(library_name()), b"lib").unwrap();
        let (var, env_dir) = library_path_env(&sd_cli).expect("sibling lib resolved");
        assert!(var == "LD_LIBRARY_PATH" || var == "DYLD_LIBRARY_PATH");
        assert_eq!(env_dir, dir.path());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn library_path_env_is_none_on_windows() {
        let dir = tempdir().unwrap();
        let sd_cli = dir.path().join(binary_name());
        std::fs::write(&sd_cli, b"bin").unwrap();
        std::fs::write(dir.path().join(library_name()), b"lib").unwrap();
        assert!(library_path_env(&sd_cli).is_none());
    }
}
