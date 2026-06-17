//! Runtime provisioning of the ONNX Runtime shared library for `ort` in
//! load-dynamic mode.
//!
//! `ort` (load-dynamic) links nothing native at build time — so the worker
//! binary cross-compiles on every cargo-dist target with no
//! glibc/libstdc++/MSVC-CRT prebuilt-link issues — and loads the ONNX Runtime
//! shared library at runtime from the `ORT_DYLIB_PATH` env var.
//!
//! We download Microsoft's official ONNX Runtime build (version [`ORT_VERSION`],
//! the one `ort` 2.0.0-rc.12 targets — ORT_API_VERSION 24) for the host
//! platform on first use, cache the shared library under
//! `<models_root>/onnxruntime/`, and point `ort` at it. This mirrors how
//! `sd-cli` and model weights are provisioned on demand.
//!
//! Platforms: Microsoft ships ONNX Runtime for linux x64/arm64, macOS arm64 and
//! windows x64/arm64. macOS-Intel (x86_64-apple-darwin) has no upstream build
//! anywhere — the worker still *builds* there (load-dynamic), but onnx jobs are
//! unsupported and fail with a clear message.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use super::download;

/// ONNX Runtime version matching `ort` 2.0.0-rc.12 (ORT_API_VERSION 24).
pub const ORT_VERSION: &str = "1.24.2";

enum Archive {
    TarGz,
    Zip,
}

struct PlatformLib {
    /// Release asset filename.
    asset: String,
    kind: Archive,
    /// Canonical filename to cache the extracted shared library under.
    out_name: &'static str,
}

fn platform_lib() -> Result<PlatformLib> {
    let v = ORT_VERSION;
    let (asset, kind, out_name) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => (
            format!("onnxruntime-linux-x64-{v}.tgz"),
            Archive::TarGz,
            "libonnxruntime.so",
        ),
        ("linux", "aarch64") => (
            format!("onnxruntime-linux-aarch64-{v}.tgz"),
            Archive::TarGz,
            "libonnxruntime.so",
        ),
        ("macos", "aarch64") => (
            format!("onnxruntime-osx-arm64-{v}.tgz"),
            Archive::TarGz,
            "libonnxruntime.dylib",
        ),
        ("windows", "x86_64") => (
            format!("onnxruntime-win-x64-{v}.zip"),
            Archive::Zip,
            "onnxruntime.dll",
        ),
        ("windows", "aarch64") => (
            format!("onnxruntime-win-arm64-{v}.zip"),
            Archive::Zip,
            "onnxruntime.dll",
        ),
        (os, arch) => bail!(
            "the onnx (LaMa) engine has no ONNX Runtime build for {os}/{arch} \
             (none is published upstream); onnx jobs are unsupported on this platform"
        ),
    };
    Ok(PlatformLib {
        asset,
        kind,
        out_name,
    })
}

/// True for the main onnxruntime shared library entry (not a provider/test lib).
fn is_main_lib(path: &str) -> bool {
    let base = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    let is_lib = base.starts_with("libonnxruntime") || base.starts_with("onnxruntime");
    let ext_ok = base.contains(".so") || base.ends_with(".dylib") || base.ends_with(".dll");
    is_lib && ext_ok && !base.contains("providers") && !base.contains("test")
}

/// Download (if needed) + return the path to the ONNX Runtime shared library for
/// this platform, cached under `<models_root>/onnxruntime/`.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn provision(models_root: &Path) -> Result<PathBuf> {
    let plat = platform_lib()?;
    let dir = models_root.join("onnxruntime");
    let dest = dir.join(plat.out_name);
    if dest.is_file() {
        return Ok(dest);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let url = format!(
        "https://github.com/microsoft/onnxruntime/releases/download/v{ORT_VERSION}/{}",
        plat.asset
    );
    let archive = dir.join(&plat.asset);
    download::download_file(&url, &archive)
        .with_context(|| format!("downloading ONNX Runtime {ORT_VERSION} from {url}"))?;
    let extracted = match plat.kind {
        Archive::TarGz => extract_targz(&archive, &dest),
        Archive::Zip => extract_zip(&archive, &dest),
    };
    let _ = std::fs::remove_file(&archive);
    if !extracted? {
        bail!("onnxruntime archive {} contained no shared library", plat.asset);
    }
    Ok(dest)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn extract_targz(archive: &Path, dest: &Path) -> Result<bool> {
    let file = std::fs::File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue; // skip the unversioned symlink, take the real .so/.dylib
        }
        let path = entry.path()?.to_string_lossy().into_owned();
        if is_main_lib(&path) {
            write_lib(&mut entry, dest)?;
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn extract_zip(archive: &Path, dest: &Path) -> Result<bool> {
    let file = std::fs::File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_file() && is_main_lib(entry.name()) {
            write_lib(&mut entry, dest)?;
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn write_lib(reader: &mut impl std::io::Read, dest: &Path) -> Result<()> {
    let mut out =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    std::io::copy(reader, &mut out)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_main_lib;

    #[test]
    fn matches_the_main_shared_library_only() {
        assert!(is_main_lib("onnxruntime-linux-x64-1.24.2/lib/libonnxruntime.so.1.24.2"));
        assert!(is_main_lib("onnxruntime-osx-arm64-1.24.2/lib/libonnxruntime.1.24.2.dylib"));
        assert!(is_main_lib("onnxruntime-win-x64-1.24.2/lib/onnxruntime.dll"));
        // not the providers / test libs, not headers
        assert!(!is_main_lib("lib/libonnxruntime_providers_shared.so"));
        assert!(!is_main_lib("lib/libonnxruntime_test.so"));
        assert!(!is_main_lib("include/onnxruntime_c_api.h"));
    }
}
