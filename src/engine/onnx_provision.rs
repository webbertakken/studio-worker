//! Runtime provisioning of the ONNX Runtime shared library for `ort` in
//! load-dynamic mode.
//!
//! `ort` (load-dynamic) links nothing native at build time — so the worker
//! binary cross-compiles on every cargo-dist target with no
//! glibc/libstdc++/MSVC-CRT prebuilt-link issues — and loads the ONNX Runtime
//! shared library at runtime from the `ORT_DYLIB_PATH` env var.
//!
//! We download Microsoft's official ONNX Runtime build (version [`ORT_VERSION`],
//! ORT_API_VERSION 24) for the host platform on first use, cache it under
//! `<models_root>/onnxruntime/`, and point `ort` at it. This mirrors how
//! `sd-cli` and model weights are provisioned on demand.
//!
//! **Flavour.** A process loads one ONNX Runtime library, so one flavour
//! serves every ONNX engine (LaMa image, streaming speech).  On an x64 host
//! with a complete CUDA runtime (cudart, cuBLAS, cuRAND, cuDNN 9) the GPU
//! build is used (it includes the CPU provider); otherwise the CPU build.
//! `STUDIO_WORKER_ORT_FLAVOUR=cpu|cuda12|cuda13` pins it.
//!
//! Platforms: Microsoft ships ONNX Runtime for linux x64/arm64, macOS arm64 and
//! windows x64/arm64 (CUDA builds: x64 only). macOS-Intel has no upstream build
//! — the worker still *builds* there (load-dynamic), but onnx jobs are
//! unsupported and fail with a clear message.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::download;

/// ONNX Runtime version for ORT_API_VERSION 24 (`ort` features `api-24`).
pub const ORT_VERSION: &str = "1.24.2";

const TRACE_TARGET: &str = "studio_worker::engine::onnx_provision";

/// Env var that pins the flavour (operator override).
pub const FLAVOUR_ENV: &str = "STUDIO_WORKER_ORT_FLAVOUR";

/// Which ONNX Runtime build the process loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrtFlavour {
    Cpu,
    Cuda12,
    Cuda13,
}

impl OrtFlavour {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda12 => "cuda12",
            Self::Cuda13 => "cuda13",
        }
    }

    pub fn is_cuda(self) -> bool {
        !matches!(self, Self::Cpu)
    }
}

/// The loaded runtime: its flavour and main shared library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrtRuntime {
    pub flavour: OrtFlavour,
    pub lib: PathBuf,
}

enum Archive {
    TarGz,
    Zip,
}

struct PlatformLib {
    /// Release asset filename.
    asset: String,
    kind: Archive,
    /// Canonical filename to cache the main shared library under.
    out_name: &'static str,
}

fn platform_lib() -> Result<PlatformLib> {
    platform_lib_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        OrtFlavour::Cpu,
    )
}

fn platform_lib_for(os: &str, arch: &str, flavour: OrtFlavour) -> Result<PlatformLib> {
    let v = ORT_VERSION;
    let gpu = match flavour {
        OrtFlavour::Cpu => "",
        OrtFlavour::Cuda12 => "-gpu",
        OrtFlavour::Cuda13 => "-gpu_cuda13",
    };
    if flavour.is_cuda() && !matches!((os, arch), ("linux", "x86_64") | ("windows", "x86_64")) {
        bail!("no CUDA build of ONNX Runtime for {os}/{arch} is published upstream");
    }
    let (asset, kind, out_name) = match (os, arch) {
        ("linux", "x86_64") => (
            format!("onnxruntime-linux-x64{gpu}-{v}.tgz"),
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
            format!("onnxruntime-win-x64{gpu}-{v}.zip"),
            Archive::Zip,
            "onnxruntime.dll",
        ),
        ("windows", "aarch64") => (
            format!("onnxruntime-win-arm64-{v}.zip"),
            Archive::Zip,
            "onnxruntime.dll",
        ),
        (os, arch) => bail!(
            "the onnx engines have no ONNX Runtime build for {os}/{arch} \
             (none is published upstream); onnx jobs are unsupported on this platform"
        ),
    };
    Ok(PlatformLib {
        asset,
        kind,
        out_name,
    })
}

fn basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase()
}

fn is_shared_lib(base: &str) -> bool {
    base.contains(".so") || base.ends_with(".dylib") || base.ends_with(".dll")
}

fn is_main_lib(path: &str) -> bool {
    let base = basename(path);
    let is_lib = base.starts_with("libonnxruntime") || base.starts_with("onnxruntime");
    is_lib && is_shared_lib(&base) && !base.contains("providers") && !base.contains("test")
}

/// Whether `path` in the archive is one this flavour needs: the main
/// library, plus the shared + CUDA providers for a GPU flavour.
fn wanted_lib(flavour: OrtFlavour, path: &str) -> bool {
    let base = basename(path);
    let provider = |name: &str| {
        base.contains(&format!("onnxruntime_providers_{name}")) && is_shared_lib(&base)
    };
    is_main_lib(path) || (flavour.is_cuda() && (provider("shared") || provider("cuda")))
}

/// The name an extracted library is cached under: the main library gets
/// its canonical name, providers keep theirs (ORT looks them up by name).
fn cached_name(path: &str, main_name: &str) -> String {
    if is_main_lib(path) {
        main_name.to_string()
    } else {
        basename(path)
    }
}

/// Where a flavour's libraries are cached.  CPU keeps the historic path.
fn cache_dir(models_root: &Path, flavour: OrtFlavour) -> PathBuf {
    let root = models_root.join("onnxruntime");
    match flavour {
        OrtFlavour::Cpu => root,
        gpu => root.join(gpu.name()),
    }
}

/// The newest complete CUDA runtime in `ldconfig -p` output: cudart,
/// cuBLAS and cuBLASLt of one major, cuRAND 10 and cuDNN 9.
fn cuda_flavour_from_ldconfig(output: &str) -> Option<OrtFlavour> {
    let has = |lib: &str| {
        output
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("{lib} ")))
    };
    let common = has("libcurand.so.10") && has("libcudnn.so.9");
    [(13, OrtFlavour::Cuda13), (12, OrtFlavour::Cuda12)]
        .into_iter()
        .find(|(major, _)| {
            common
                && has(&format!("libcudart.so.{major}"))
                && has(&format!("libcublas.so.{major}"))
                && has(&format!("libcublasLt.so.{major}"))
        })
        .map(|(_, flavour)| flavour)
}

/// Parse the operator's `STUDIO_WORKER_ORT_FLAVOUR`.
fn flavour_override(value: Option<&str>) -> Result<Option<OrtFlavour>> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "cpu" => Ok(Some(OrtFlavour::Cpu)),
        "cuda12" => Ok(Some(OrtFlavour::Cuda12)),
        "cuda13" => Ok(Some(OrtFlavour::Cuda13)),
        other => bail!("{FLAVOUR_ENV}={other:?} is not one of cpu, cuda12, cuda13"),
    }
}

/// Decide the flavour for this host: the override, else a detected CUDA
/// runtime on x64 Linux, else CPU.  Logged either way.
#[cfg_attr(coverage_nightly, coverage(off))]
fn detect_flavour() -> Result<OrtFlavour> {
    if let Some(pinned) = flavour_override(std::env::var(FLAVOUR_ENV).ok().as_deref())? {
        tracing::info!(target: TRACE_TARGET, op = "flavour", flavour = pinned.name(), source = "override", "onnx runtime flavour pinned");
        return Ok(pinned);
    }
    let detected = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        std::process::Command::new("ldconfig")
            .arg("-p")
            .output()
            .ok()
            .and_then(|o| cuda_flavour_from_ldconfig(&String::from_utf8_lossy(&o.stdout)))
    } else {
        None
    };
    let flavour = detected.unwrap_or(OrtFlavour::Cpu);
    tracing::info!(
        target: TRACE_TARGET,
        op = "flavour",
        flavour = flavour.name(),
        source = if detected.is_some() { "cuda_runtime_detected" } else { "no_complete_cuda_runtime" },
        "onnx runtime flavour chosen"
    );
    Ok(flavour)
}

/// The process-wide ONNX Runtime: chosen, provisioned and pointed at once.
/// Every ONNX engine calls this before building a session.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn ensure_runtime(models_root: &Path) -> Result<OrtRuntime> {
    static RUNTIME: OnceLock<OrtRuntime> = OnceLock::new();
    static INIT: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt.clone());
    }
    let _guard = INIT.lock();
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt.clone());
    }
    let runtime = match std::env::var_os("ORT_DYLIB_PATH") {
        // Set by the operator: use it as-is; only a pinned flavour says it is CUDA.
        Some(path) => OrtRuntime {
            flavour: flavour_override(std::env::var(FLAVOUR_ENV).ok().as_deref())?
                .unwrap_or(OrtFlavour::Cpu),
            lib: PathBuf::from(path),
        },
        None => {
            let flavour = detect_flavour()?;
            let lib = provision_flavour(models_root, flavour)?;
            std::env::set_var("ORT_DYLIB_PATH", &lib);
            OrtRuntime { flavour, lib }
        }
    };
    tracing::info!(
        target: TRACE_TARGET,
        op = "ensure",
        flavour = runtime.flavour.name(),
        lib = %runtime.lib.display(),
        "onnx runtime ready"
    );
    let _ = RUNTIME.set(runtime.clone());
    Ok(runtime)
}

/// Download (if needed) + return the path to the CPU ONNX Runtime shared
/// library for this platform, cached under `<models_root>/onnxruntime/`.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn provision(models_root: &Path) -> Result<PathBuf> {
    provision_flavour(models_root, OrtFlavour::Cpu)
}

/// Download (if needed) + return the main library of `flavour`, with the
/// provider libraries a GPU flavour needs beside it.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn provision_flavour(models_root: &Path, flavour: OrtFlavour) -> Result<PathBuf> {
    let plat = match flavour {
        OrtFlavour::Cpu => platform_lib()?,
        gpu => platform_lib_for(std::env::consts::OS, std::env::consts::ARCH, gpu)?,
    };
    let dir = cache_dir(models_root, flavour);
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
        Archive::TarGz => extract_targz(&archive, &dir, flavour, plat.out_name),
        Archive::Zip => extract_zip(&archive, &dir, flavour, plat.out_name),
    };
    let _ = std::fs::remove_file(&archive);
    extracted?;
    if !dest.is_file() {
        bail!(
            "onnxruntime archive {} contained no shared library",
            plat.asset
        );
    }
    Ok(dest)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn extract_targz(archive: &Path, dir: &Path, flavour: OrtFlavour, main_name: &str) -> Result<()> {
    let file =
        std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue; // skip the unversioned symlinks, take the real files
        }
        let path = entry.path()?.to_string_lossy().into_owned();
        if wanted_lib(flavour, &path) {
            write_lib(&mut entry, &dir.join(cached_name(&path, main_name)))?;
        }
    }
    Ok(())
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn extract_zip(archive: &Path, dir: &Path, flavour: OrtFlavour, main_name: &str) -> Result<()> {
    let file =
        std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_file() && wanted_lib(flavour, &name) {
            write_lib(&mut entry, &dir.join(cached_name(&name, main_name)))?;
        }
    }
    Ok(())
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
    use super::*;

    #[test]
    fn each_flavour_names_its_release_asset() {
        let asset = |os, arch, f| platform_lib_for(os, arch, f).unwrap().asset;
        assert_eq!(
            asset("linux", "x86_64", OrtFlavour::Cpu),
            "onnxruntime-linux-x64-1.24.2.tgz"
        );
        assert_eq!(
            asset("linux", "x86_64", OrtFlavour::Cuda12),
            "onnxruntime-linux-x64-gpu-1.24.2.tgz"
        );
        assert_eq!(
            asset("linux", "x86_64", OrtFlavour::Cuda13),
            "onnxruntime-linux-x64-gpu_cuda13-1.24.2.tgz"
        );
        assert_eq!(
            asset("windows", "x86_64", OrtFlavour::Cuda13),
            "onnxruntime-win-x64-gpu_cuda13-1.24.2.zip"
        );
        assert_eq!(
            asset("macos", "aarch64", OrtFlavour::Cpu),
            "onnxruntime-osx-arm64-1.24.2.tgz"
        );
    }

    #[test]
    fn gpu_flavours_exist_only_on_x64_linux_and_windows() {
        let err = platform_lib_for("linux", "aarch64", OrtFlavour::Cuda13)
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .contains("no CUDA build of ONNX Runtime for linux/aarch64"),
            "{err}"
        );
        assert!(platform_lib_for("freebsd", "x86_64", OrtFlavour::Cpu).is_err());
    }

    #[test]
    fn gpu_flavours_keep_the_cuda_provider_libraries() {
        assert!(wanted_lib(
            OrtFlavour::Cuda13,
            "x/lib/libonnxruntime_providers_cuda.so"
        ));
        assert!(wanted_lib(
            OrtFlavour::Cuda13,
            "x/lib/libonnxruntime_providers_shared.so"
        ));
        assert!(wanted_lib(
            OrtFlavour::Cuda13,
            "x/lib/libonnxruntime.so.1.24.2"
        ));
        assert!(!wanted_lib(
            OrtFlavour::Cuda13,
            "x/lib/libonnxruntime_providers_tensorrt.so"
        ));
        assert!(!wanted_lib(
            OrtFlavour::Cpu,
            "x/lib/libonnxruntime_providers_shared.so"
        ));
        assert!(wanted_lib(
            OrtFlavour::Cpu,
            "x/lib/libonnxruntime.so.1.24.2"
        ));
        assert!(wanted_lib(
            OrtFlavour::Cuda12,
            "x/lib/onnxruntime_providers_cuda.dll"
        ));
    }

    #[test]
    fn extracted_libraries_keep_names_the_loader_finds() {
        assert_eq!(
            cached_name("x/lib/libonnxruntime.so.1.24.2", "libonnxruntime.so"),
            "libonnxruntime.so"
        );
        assert_eq!(
            cached_name(
                "x/lib/libonnxruntime_providers_cuda.so",
                "libonnxruntime.so"
            ),
            "libonnxruntime_providers_cuda.so"
        );
    }

    const LDCONFIG_CUDA13: &str = "\
\tlibcudnn.so.9 (libc6,x86-64) => /lib/x86_64-linux-gnu/libcudnn.so.9
\tlibcurand.so.10 (libc6,x86-64) => /usr/local/cuda/lib64/libcurand.so.10
\tlibcudart.so.13 (libc6,x86-64) => /usr/local/cuda/lib64/libcudart.so.13
\tlibcublasLt.so.13 (libc6,x86-64) => /usr/local/cuda/lib64/libcublasLt.so.13
\tlibcublas.so.13 (libc6,x86-64) => /usr/local/cuda/lib64/libcublas.so.13
\tlibcudart.so.12 (libc6,x86-64) => /opt/old/libcudart.so.12
";

    #[test]
    fn detects_the_newest_complete_cuda_runtime() {
        assert_eq!(
            cuda_flavour_from_ldconfig(LDCONFIG_CUDA13),
            Some(OrtFlavour::Cuda13)
        );
        let only12 = LDCONFIG_CUDA13.replace(".so.13", ".so.12");
        assert_eq!(
            cuda_flavour_from_ldconfig(&only12),
            Some(OrtFlavour::Cuda12)
        );
    }

    #[test]
    fn an_incomplete_cuda_runtime_is_not_used() {
        let no_cudnn = LDCONFIG_CUDA13.replace("libcudnn.so.9", "libcudnn.so.8");
        assert_eq!(cuda_flavour_from_ldconfig(&no_cudnn), None);
        let no_cublas = LDCONFIG_CUDA13.replace("libcublas.so.13", "libcublasx.so.13");
        assert_eq!(cuda_flavour_from_ldconfig(&no_cublas), None);
        assert_eq!(cuda_flavour_from_ldconfig(""), None);
    }

    #[test]
    fn the_operator_can_pin_the_flavour() {
        assert_eq!(
            flavour_override(Some("cpu")).unwrap(),
            Some(OrtFlavour::Cpu)
        );
        assert_eq!(
            flavour_override(Some(" CUDA13 ")).unwrap(),
            Some(OrtFlavour::Cuda13)
        );
        assert_eq!(
            flavour_override(Some("cuda12")).unwrap(),
            Some(OrtFlavour::Cuda12)
        );
        assert_eq!(flavour_override(None).unwrap(), None);
        assert!(flavour_override(Some("rocm")).is_err());
    }

    #[test]
    fn flavours_cache_apart() {
        let root = Path::new("/m");
        assert_eq!(
            cache_dir(root, OrtFlavour::Cpu),
            Path::new("/m/onnxruntime")
        );
        assert_eq!(
            cache_dir(root, OrtFlavour::Cuda13),
            Path::new("/m/onnxruntime/cuda13")
        );
        assert_eq!(OrtFlavour::Cuda12.name(), "cuda12");
        assert!(OrtFlavour::Cuda12.is_cuda() && !OrtFlavour::Cpu.is_cuda());
    }

    #[test]
    fn matches_the_main_shared_library_only() {
        assert!(is_main_lib(
            "onnxruntime-linux-x64-1.24.2/lib/libonnxruntime.so.1.24.2"
        ));
        assert!(is_main_lib(
            "onnxruntime-osx-arm64-1.24.2/lib/libonnxruntime.1.24.2.dylib"
        ));
        assert!(is_main_lib(
            "onnxruntime-win-x64-1.24.2/lib/onnxruntime.dll"
        ));
        // not the providers / test libs, not headers
        assert!(!is_main_lib("lib/libonnxruntime_providers_shared.so"));
        assert!(!is_main_lib("lib/libonnxruntime_test.so"));
        assert!(!is_main_lib("include/onnxruntime_c_api.h"));
    }
}
