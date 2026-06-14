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
use tracing::{debug, info, warn};

/// Tracing target for provisioning.  Stable so operators can filter
/// with `RUST_LOG=studio_worker::engine::sd_provision=info`.
const TRACE_TARGET: &str = "studio_worker::engine::sd_provision";

/// Pinned, known-good upstream release.  Bump deliberately after
/// verifying a newer build still serves our model set.  Overridable
/// per box via `STUDIO_WORKER_SDCPP_RELEASE`.  When bumping, also update
/// the pinned URL in `docs/operations/sd-cli-install.md` and the
/// `sdcpp-prebuilt.yml` workflow default so the manual playbook, the
/// self-hosted arm64 build, and the auto-provisioner all share one
/// known-good sd.cpp commit.
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

/// The Vulkan loader the prebuilt sd-cli links against, per OS.
/// `None` on macOS, where the build targets Metal and no Vulkan loader
/// is involved.
fn vulkan_loader_name() -> Option<&'static str> {
    if cfg!(target_os = "windows") {
        Some("vulkan-1.dll")
    } else if cfg!(target_os = "macos") {
        None
    } else {
        Some("libvulkan.so.1")
    }
}

/// Per-OS remedy for a missing Vulkan loader.  We can't auto-provision
/// it: it ships with the GPU driver (Windows) or a system package +
/// driver (Linux), neither of which we can install unattended.
fn vulkan_remedy() -> &'static str {
    if cfg!(target_os = "windows") {
        "install/update your GPU driver (NVIDIA, AMD, or Intel) — it ships \
         the Vulkan runtime (vulkan-1.dll)"
    } else {
        "install the Vulkan loader + a GPU driver, e.g. on Debian/Ubuntu \
         `sudo apt install libvulkan1 mesa-vulkan-drivers` (plus the \
         vendor driver for NVIDIA/AMD); verify with `vulkaninfo --summary`"
    }
}

/// Whether the Vulkan loader can actually be loaded by the dynamic
/// linker.  Uses the same `dlopen`/`LoadLibrary` mechanism sd-cli
/// relies on, so a true result means sd-cli will find the loader too.
/// Always `true` on macOS (Metal, no Vulkan).  Excluded from coverage:
/// the outcome is host-GPU-dependent and unstable across CI runners.
#[cfg_attr(coverage_nightly, coverage(off))]
fn vulkan_loader_loads() -> bool {
    match vulkan_loader_name() {
        None => true,
        Some(name) => unsafe { libloading::Library::new(name).is_ok() },
    }
}

/// Preflight the GPU runtime sd-cli needs.  Returns a clear, actionable
/// error when the Vulkan loader is absent so the operator sees exactly
/// what to install instead of a cryptic sd-cli linker/instance crash.
/// `probe` is injected so the decision + message are unit-testable
/// without depending on the host's GPU stack.
fn vulkan_runtime_status_with(loader_loads: bool) -> Result<()> {
    let Some(loader) = vulkan_loader_name() else {
        return Ok(()); // macOS / Metal: nothing to check.
    };
    if loader_loads {
        return Ok(());
    }
    bail!(
        "Vulkan runtime not available: the loader `{loader}` could not be \
         loaded, so stable-diffusion.cpp cannot run on the GPU. We cannot \
         auto-provision it — {}.",
        vulkan_remedy()
    )
}

/// Live preflight: probes the real loader.  Excluded from coverage for
/// the same host-dependent reason as [`vulkan_loader_loads`]; the
/// decision logic is covered via [`vulkan_runtime_status_with`].
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn vulkan_runtime_status() -> Result<()> {
    vulkan_runtime_status_with(vulkan_loader_loads())
}

/// Choose the release tag from an optional override, logging which
/// source won so an operator can confirm their
/// `STUDIO_WORKER_SDCPP_RELEASE` took effect (rather than being
/// silently ignored, e.g. a typo'd var name).  Pure — the override is
/// injected — so the decision + breadcrumb are unit-testable without
/// touching the process-global environment.
fn select_release_tag(override_tag: Option<String>) -> String {
    match override_tag {
        Some(tag) => {
            info!(
                target: TRACE_TARGET,
                op = "resolve-url",
                tag = %tag,
                source = RELEASE_ENV,
                "using sd-cli release-tag override"
            );
            tag
        }
        None => {
            debug!(
                target: TRACE_TARGET,
                op = "resolve-url",
                tag = DEFAULT_RELEASE_TAG,
                "using pinned sd-cli release tag"
            );
            DEFAULT_RELEASE_TAG.to_string()
        }
    }
}

/// The release tag to provision — env override or the pinned default.
/// Thin env-reading wrapper over [`select_release_tag`]; excluded from
/// coverage because it reads the process environment (the decision +
/// logging are covered via [`select_release_tag`]).
#[cfg_attr(coverage_nightly, coverage(off))]
fn release_tag() -> String {
    select_release_tag(std::env::var(RELEASE_ENV).ok())
}

/// The short commit sha embedded in asset filenames is the trailing
/// `-`-segment of the release tag (`master-669-2d40a8b` -> `2d40a8b`).
fn sha_from_tag(tag: &str) -> Result<&str> {
    match tag.rsplit_once('-') {
        Some((_, sha)) if !sha.is_empty() => Ok(sha),
        _ => Err(anyhow!("release tag {tag:?} has no '-<sha>' segment")),
    }
}

/// Where a platform's prebuilt zip is hosted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetSource {
    /// leejet/stable-diffusion.cpp's own releases.
    Upstream,
    /// Our own releases — platforms upstream doesn't prebuild
    /// (currently Linux aarch64), built by `sdcpp-prebuilt.yml` at the
    /// same sd.cpp commit.
    SelfHosted,
}

/// Pick the prebuilt for a target: its host and the asset suffix (the
/// part between `bin-` and `.zip`).  Vulkan is the universal GPU
/// backend (one build serves NVIDIA / AMD / Intel); macOS ships a
/// universal2 Metal binary, so Intel + Apple-Silicon share one asset.
fn asset_plan(os: &str, arch: &str) -> Result<(AssetSource, &'static str)> {
    use AssetSource::*;
    match (os, arch) {
        ("windows", "x86_64") => Ok((Upstream, "win-vulkan-x64")),
        ("linux", "x86_64") => Ok((Upstream, "Linux-Ubuntu-24.04-x86_64-vulkan")),
        // The upstream Darwin build is a universal2 binary (x86_64 +
        // arm64), so Intel Macs use the very same asset.
        ("macos", "aarch64") | ("macos", "x86_64") => Ok((Upstream, "Darwin-macOS-15.7.7-arm64")),
        // Upstream has no aarch64 Linux build; we publish our own.
        ("linux", "aarch64") => Ok((SelfHosted, "Linux-aarch64-vulkan")),
        _ => bail!(
            "no prebuilt stable-diffusion.cpp binary for {os}/{arch}; \
             install sd-cli manually — see docs/operations/sd-cli-install.md"
        ),
    }
}

/// Build the asset filename for `sha` + `suffix` (upstream's naming
/// convention, which our self-hosted builds mirror).
fn asset_name(sha: &str, suffix: &str) -> String {
    format!("sd-master-{sha}-bin-{suffix}.zip")
}

/// Our release tag holding the self-hosted prebuilts for `upstream_tag`.
fn self_hosted_tag(upstream_tag: &str) -> String {
    format!("sdcpp-prebuilt-{upstream_tag}")
}

/// The full release-download URL for `tag` on `os`/`arch`, routed to
/// upstream or our own releases depending on the platform.
fn download_url(tag: &str, os: &str, arch: &str) -> Result<String> {
    let sha = sha_from_tag(tag)?;
    let (source, suffix) = asset_plan(os, arch)?;
    let asset = asset_name(sha, suffix);
    Ok(match source {
        AssetSource::Upstream => format!(
            "https://github.com/leejet/stable-diffusion.cpp/releases/download/{tag}/{asset}"
        ),
        AssetSource::SelfHosted => format!(
            "https://github.com/webbertakken/studio-worker/releases/download/{}/{asset}",
            self_hosted_tag(tag)
        ),
    })
}

/// Choose the zip URL: a non-empty `STUDIO_WORKER_SDCPP_URL` override
/// wins (and is logged so the operator can confirm it took effect),
/// otherwise fall back to `default_url`.  Pure — both inputs are
/// injected — so the precedence + breadcrumb are unit-testable without
/// touching the environment or the network.
fn select_url(
    override_url: Option<String>,
    default_url: impl FnOnce() -> Result<String>,
) -> Result<String> {
    if let Some(url) = override_url {
        if !url.is_empty() {
            info!(
                target: TRACE_TARGET,
                op = "resolve-url",
                url = %url,
                source = URL_ENV,
                "using sd-cli zip-URL override"
            );
            return Ok(url);
        }
        // Present but empty (`STUDIO_WORKER_SDCPP_URL=`): the override is
        // dropped and the pinned default is used.  Surface it instead of
        // silently swallowing it, so a blank env value (a unit-file or CI
        // misconfiguration) doesn't leave the operator wondering why their
        // mirror override never took effect.
        warn!(
            target: TRACE_TARGET,
            op = "resolve-url",
            source = URL_ENV,
            "ignoring empty STUDIO_WORKER_SDCPP_URL override; using the default release URL"
        );
    }
    default_url()
}

/// Resolve the zip URL to fetch: the `STUDIO_WORKER_SDCPP_URL`
/// override if set, otherwise the pinned/overridden release for this
/// host's platform.  Thin env-reading wrapper over [`select_url`];
/// excluded from coverage because it reads the process environment.
#[cfg_attr(coverage_nightly, coverage(off))]
fn resolve_url() -> Result<String> {
    select_url(std::env::var(URL_ENV).ok(), || {
        download_url(&release_tag(), std::env::consts::OS, std::env::consts::ARCH)
    })
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

/// Best-effort removal of the provisioning scratch zip + staging dir.
/// Unlike a bare `let _ = remove(..)`, a failed removal is logged: a
/// leftover multi-hundred-MB scratch file silently filling the disk is
/// the exact failure this cleanup guards against, so operators must see
/// it.  A NotFound (the path was already gone) is the normal case and
/// stays quiet.
fn clean_scratch(zip_path: &Path, staging: &Path) {
    if let Err(e) = std::fs::remove_file(zip_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(
                target: TRACE_TARGET,
                op = "cleanup",
                path = %zip_path.display(),
                error = %e,
                "could not remove sd-cli scratch zip; it may fill the disk"
            );
        }
    }
    if let Err(e) = std::fs::remove_dir_all(staging) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(
                target: TRACE_TARGET,
                op = "cleanup",
                path = %staging.display(),
                error = %e,
                "could not remove sd-cli staging dir; it may fill the disk"
            );
        }
    }
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
    // multi-hundred-MB files filling the disk.  Removal failures are
    // logged, not swallowed.
    clean_scratch(&zip_path, &staging);

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
    fn asset_plan_picks_vulkan_or_universal_for_supported_targets() {
        use AssetSource::*;
        assert_eq!(
            asset_plan("windows", "x86_64").unwrap(),
            (Upstream, "win-vulkan-x64")
        );
        assert_eq!(
            asset_plan("linux", "x86_64").unwrap(),
            (Upstream, "Linux-Ubuntu-24.04-x86_64-vulkan")
        );
        assert_eq!(
            asset_plan("macos", "aarch64").unwrap(),
            (Upstream, "Darwin-macOS-15.7.7-arm64")
        );
    }

    #[test]
    fn asset_plan_makes_intel_mac_and_arm_linux_first_class() {
        use AssetSource::*;
        // Intel Macs ride the upstream universal2 Darwin binary.
        assert_eq!(
            asset_plan("macos", "x86_64").unwrap(),
            (Upstream, "Darwin-macOS-15.7.7-arm64")
        );
        // aarch64 Linux has no upstream build, so we self-host one.
        assert_eq!(
            asset_plan("linux", "aarch64").unwrap(),
            (SelfHosted, "Linux-aarch64-vulkan")
        );
    }

    #[test]
    fn asset_plan_rejects_unsupported_targets_with_guidance() {
        let err = asset_plan("freebsd", "x86_64").unwrap_err().to_string();
        assert!(err.contains("no prebuilt"), "got: {err}");
        assert!(
            err.contains("sd-cli-install.md"),
            "points to the doc: {err}"
        );
        assert!(asset_plan("windows", "aarch64").is_err());
    }

    #[test]
    fn asset_name_embeds_sha_and_platform() {
        assert_eq!(
            asset_name("2d40a8b", "win-vulkan-x64"),
            "sd-master-2d40a8b-bin-win-vulkan-x64.zip"
        );
        assert_eq!(
            asset_name("2d40a8b", "Linux-aarch64-vulkan"),
            "sd-master-2d40a8b-bin-Linux-aarch64-vulkan.zip"
        );
    }

    #[test]
    fn download_url_targets_upstream_for_covered_platforms() {
        let url = download_url("master-669-2d40a8b", "windows", "x86_64").unwrap();
        let expected = concat!(
            "https://github.com/leejet/stable-diffusion.cpp/releases/download/",
            "master-669-2d40a8b/sd-master-2d40a8b-bin-win-vulkan-x64.zip"
        );
        assert_eq!(url, expected);
    }

    #[test]
    fn download_url_targets_our_release_for_arm_linux() {
        let url = download_url("master-669-2d40a8b", "linux", "aarch64").unwrap();
        let expected = concat!(
            "https://github.com/webbertakken/studio-worker/releases/download/",
            "sdcpp-prebuilt-master-669-2d40a8b/",
            "sd-master-2d40a8b-bin-Linux-aarch64-vulkan.zip"
        );
        assert_eq!(url, expected);
    }

    #[test]
    fn download_url_uses_universal_darwin_asset_for_intel_mac() {
        let arm = download_url("master-669-2d40a8b", "macos", "aarch64").unwrap();
        let intel = download_url("master-669-2d40a8b", "macos", "x86_64").unwrap();
        assert_eq!(arm, intel, "Intel Macs use the same universal2 asset");
        assert!(intel.contains("Darwin-macOS-15.7.7-arm64"), "got: {intel}");
    }

    #[test]
    fn select_release_tag_prefers_the_override() {
        assert_eq!(
            select_release_tag(Some("master-700-deadbee".into())),
            "master-700-deadbee"
        );
    }

    #[test]
    fn select_release_tag_falls_back_to_the_pinned_default() {
        assert_eq!(select_release_tag(None), DEFAULT_RELEASE_TAG);
    }

    #[test]
    fn select_release_tag_logs_the_override_source() {
        let logs = crate::test_support::capture(|| {
            let _ = select_release_tag(Some("master-700-deadbee".into()));
        });
        assert!(
            logs.contains("STUDIO_WORKER_SDCPP_RELEASE"),
            "override log must name the env var: {logs}"
        );
        assert!(logs.contains("master-700-deadbee"), "got: {logs}");
        assert!(logs.contains("override"), "got: {logs}");
    }

    #[test]
    fn select_url_prefers_a_non_empty_override() {
        let url = select_url(Some("https://mirror.example/sd.zip".into()), || {
            panic!("default must not be consulted when an override is present")
        })
        .unwrap();
        assert_eq!(url, "https://mirror.example/sd.zip");
    }

    #[test]
    fn select_url_ignores_an_empty_override_and_falls_back() {
        let url = select_url(Some(String::new()), || Ok("fallback".into())).unwrap();
        assert_eq!(url, "fallback");
    }

    #[test]
    fn select_url_falls_back_when_no_override_is_set() {
        let url = select_url(None, || Ok("fallback".into())).unwrap();
        assert_eq!(url, "fallback");
    }

    #[test]
    fn select_url_propagates_a_default_resolution_error() {
        let err = select_url(None, || bail!("no prebuilt for this platform"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no prebuilt"), "got: {err}");
    }

    #[test]
    fn select_url_logs_the_override_source() {
        let logs = crate::test_support::capture(|| {
            let _ = select_url(Some("https://mirror.example/sd.zip".into()), || {
                Ok("unused".into())
            });
        });
        assert!(
            logs.contains("STUDIO_WORKER_SDCPP_URL"),
            "override log must name the env var: {logs}"
        );
        assert!(
            logs.contains("https://mirror.example/sd.zip"),
            "got: {logs}"
        );
    }

    #[test]
    fn select_url_warns_when_the_override_is_present_but_empty() {
        // An override that's present but empty (`STUDIO_WORKER_SDCPP_URL=`,
        // e.g. an `Environment="STUDIO_WORKER_SDCPP_URL="` line in a unit file
        // or a CI that sets the var conditionally and leaves it blank) is a
        // misconfiguration: the override is dropped and the pinned default is
        // used.  Without a breadcrumb the operator has no trace of why their
        // mirror override never took effect — the symmetric silent gap to the
        // non-empty "took effect" log above.
        let logs = crate::test_support::capture(|| {
            let url = select_url(Some(String::new()), || Ok("fallback".into())).unwrap();
            assert_eq!(url, "fallback", "an empty override must still fall back");
        });
        assert!(
            logs.contains("WARN"),
            "expected a WARN breadcrumb, got: {logs}"
        );
        assert!(
            logs.contains("STUDIO_WORKER_SDCPP_URL"),
            "the warning must name the ignored env var: {logs}"
        );
        assert!(
            logs.contains("op=\"resolve-url\""),
            "expected the resolve-url op field: {logs}"
        );
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
    fn install_dir_skips_subdirectories_and_counts_only_files() {
        // `install_dir` publishes a *flat* set of files; a directory in
        // staging (a malformed build archive, or a future extract that
        // stops flattening) must be skipped, not recursed into or
        // copied as-is, and must not inflate the moved-file count the
        // provisioner relies on.
        let staging = tempdir().unwrap();
        let target = tempdir().unwrap();
        std::fs::write(staging.path().join("sd-cli"), b"binary").unwrap();
        std::fs::write(staging.path().join("libstable-diffusion.so"), b"lib").unwrap();
        let nested = staging.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("buried"), b"should-not-publish").unwrap();

        let moved = install_dir(staging.path(), target.path()).unwrap();

        // Only the two top-level files count; the directory is skipped.
        assert_eq!(moved, 2);
        assert!(target.path().join("sd-cli").is_file());
        assert!(target.path().join("libstable-diffusion.so").is_file());
        // The directory (and its contents) must never reach the target.
        assert!(
            !target.path().join("nested").exists(),
            "a staging subdirectory must not be published"
        );
        assert!(
            !target.path().join("buried").exists(),
            "a staging subdirectory's contents must not be flattened into the target"
        );
    }

    #[test]
    fn clean_scratch_removes_zip_and_staging_quietly() {
        let dir = tempdir().unwrap();
        let zip = dir.path().join("scratch.zip");
        let staging = dir.path().join("staging");
        std::fs::write(&zip, b"zip").unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("sd-cli"), b"bin").unwrap();

        let (zip_c, staging_c) = (zip.clone(), staging.clone());
        let logs = crate::test_support::capture(move || clean_scratch(&zip_c, &staging_c));

        assert!(!zip.exists(), "scratch zip must be removed");
        assert!(!staging.exists(), "staging dir must be removed");
        assert!(
            !logs.contains("could not remove"),
            "a clean removal must not warn: {logs}"
        );
    }

    #[test]
    fn clean_scratch_is_silent_when_paths_are_already_gone() {
        let dir = tempdir().unwrap();
        let zip = dir.path().join("missing.zip");
        let staging = dir.path().join("missing-staging");

        let (zip_c, staging_c) = (zip.clone(), staging.clone());
        let logs = crate::test_support::capture(move || clean_scratch(&zip_c, &staging_c));

        // A NotFound (already gone) is the normal case and must stay quiet.
        assert!(
            !logs.contains("could not remove"),
            "an already-clean slot must not warn: {logs}"
        );
    }

    #[test]
    fn clean_scratch_warns_when_removal_fails() {
        let dir = tempdir().unwrap();
        // A directory where the zip is expected makes `remove_file` fail
        // with a non-NotFound error; a file where the staging dir is
        // expected makes `remove_dir_all` fail likewise.  A leftover
        // multi-hundred-MB scratch file silently filling the disk is
        // exactly what this guards against, so both must surface.
        let zip = dir.path().join("zip-slot");
        std::fs::create_dir_all(&zip).unwrap();
        let staging = dir.path().join("staging-slot");
        std::fs::write(&staging, b"not a dir").unwrap();

        let (zip_c, staging_c) = (zip.clone(), staging.clone());
        let logs = crate::test_support::capture(move || clean_scratch(&zip_c, &staging_c));

        assert!(
            logs.matches("could not remove").count() >= 2,
            "both failed removals must warn: {logs}"
        );
        assert!(
            logs.contains("fill the disk"),
            "the warning must flag the disk-fill risk: {logs}"
        );
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

    #[test]
    fn extract_zip_skips_directory_entries() {
        // Real stable-diffusion.cpp prebuilt zips carry explicit
        // directory entries (a top-level `build/` marker, a `bin/`
        // dir, etc.).  Those must be skipped, not turned into spurious
        // empty files in the flat output dir: a directory entry's name
        // ends in `/`, so without the `is_dir` guard `file_name()`
        // would strip the slash and write an empty `build` file
        // alongside the real binary, and inflate the written-file count
        // the provisioner reports.
        let dir = tempdir().unwrap();
        let zip_path = dir.path().join("with-dirs.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zw.add_directory("build/", opts).unwrap();
            zw.start_file("sd-cli", opts).unwrap();
            zw.write_all(b"binary").unwrap();
            zw.add_directory("nested/empty/", opts).unwrap();
            zw.finish().unwrap();
        }
        let dest = dir.path().join("out");
        let count = extract_zip(&zip_path, &dest).unwrap();
        // Only the single real file counts; both directory entries are skipped.
        assert_eq!(
            count, 1,
            "directory entries must not count as written files"
        );
        assert_eq!(std::fs::read(dest.join("sd-cli")).unwrap(), b"binary");
        // No spurious file is created from a directory entry's slash-stripped name.
        assert!(
            !dest.join("build").exists(),
            "a directory entry must not become a file in the flat output"
        );
        assert!(
            !dest.join("empty").exists(),
            "a nested directory entry must not become a file either"
        );
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

    #[test]
    fn vulkan_status_ok_when_loader_loads() {
        // macOS short-circuits to Ok regardless; elsewhere a loadable
        // loader is Ok.
        assert!(vulkan_runtime_status_with(true).is_ok());
    }

    #[test]
    fn vulkan_status_errors_with_actionable_remedy_when_missing() {
        let result = vulkan_runtime_status_with(false);
        if cfg!(target_os = "macos") {
            // Metal build: there is no Vulkan loader to miss.
            assert!(result.is_ok());
        } else {
            let err = result.unwrap_err().to_string();
            assert!(err.contains("Vulkan runtime"), "got: {err}");
            assert!(
                err.contains("auto-provision"),
                "must say we can't auto-provision it: {err}"
            );
            // The remedy names the concrete fix for this OS.
            if cfg!(target_os = "windows") {
                assert!(err.contains("vulkan-1.dll"), "got: {err}");
                assert!(err.contains("GPU driver"), "got: {err}");
            } else {
                assert!(err.contains("libvulkan1"), "got: {err}");
                assert!(err.contains("vulkaninfo"), "got: {err}");
            }
        }
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
