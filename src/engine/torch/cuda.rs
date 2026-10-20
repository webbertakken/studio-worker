//! `CUDA_HOME` without a system toolkit and without root: NVIDIA's redist archives (`nvcc`,
//! `cudart`, ...), each checked against the release manifest's sha256, merged with the CUDA
//! libraries torch's wheels already carry (`docs/engines/torch.md`, "Provisioning").
//!
//! The pip `nvidia-cuda-nvcc-cu12` 12.4 wheel has no `nvcc` binary, hence the redist archives.

use std::path::{Path, PathBuf};

use super::error::TorchError;

/// NVIDIA's redist root; tests point it at a mock server.
pub const REDIST_URL: &str = "https://developer.download.nvidia.com/compute/cuda/redist";
/// The redist platform the archives are taken for.
const REDIST_PLATFORM: &str = "linux-x86_64";
/// The archives `CUDA_HOME` is built from: enough for torch's C++/CUDA extensions.
pub const PARTS: [&str; 5] = [
    "cuda_nvcc",
    "cuda_cudart",
    "cuda_cccl",
    "cuda_profiler_api",
    "cuda_nvtx",
];
/// Libraries (headers and shared objects) taken from the environment's torch wheels
/// (`site-packages/nvidia/<name>`).
pub const WHEEL_LIBRARIES: [&str; 6] = [
    "cublas",
    "cusparse",
    "cusolver",
    "curand",
    "cufft",
    "nvjitlink",
];

/// The sha256 of each pinned release's manifest, which in turn pins every archive's sha256.
pub fn manifest_sha256(version: &str) -> Option<&'static str> {
    match version {
        "12.4.1" => Some("9cd815f3b71c2e3686ef2219b7794b81044f9dcefaa8e21dacfcb5bc4d931892"),
        _ => None,
    }
}

/// One archive of a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    pub part: String,
    pub relative_path: String,
    pub sha256: String,
}

impl Archive {
    pub fn file_name(&self) -> &str {
        self.relative_path
            .rsplit('/')
            .next()
            .unwrap_or(&self.relative_path)
    }
}

/// The archives of [`PARTS`] for Linux x86_64, read from a release manifest
/// (`redistrib_<version>.json`).
pub fn archives(manifest: &str) -> Result<Vec<Archive>, String> {
    let json: serde_json::Value =
        serde_json::from_str(manifest).map_err(|e| format!("the manifest is not JSON: {e}"))?;
    PARTS
        .iter()
        .map(|part| {
            let entry = &json[part][REDIST_PLATFORM];
            let field = |name: &str| {
                entry[name]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("the manifest has no {part} {REDIST_PLATFORM} {name}"))
            };
            Ok(Archive {
                part: (*part).to_string(),
                relative_path: field("relative_path")?,
                sha256: field("sha256")?,
            })
        })
        .collect()
}

/// Symlink every file under `from` into the same place under `to`, keeping what is there.
pub fn link_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&target)?;
            link_tree(&entry.path(), &target)?;
        } else if std::fs::symlink_metadata(&target).is_err() {
            symlink(&entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(not(unix))]
fn symlink(original: &Path, link: &Path) -> std::io::Result<()> {
    std::fs::copy(original, link).map(|_| ())
}

/// For every `libx.so.12...` in `lib`, a `libx.so` link when there is none: the name the linker
/// looks for.
pub fn unversioned_links(lib: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(lib)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let Some((stem, _)) = name.split_once(".so.") else {
            continue;
        };
        let link = lib.join(format!("{stem}.so"));
        if std::fs::symlink_metadata(&link).is_err() {
            symlink(Path::new(&name), &link)?;
        }
    }
    Ok(())
}

/// Assemble `cuda_home` for `version`: download (or reuse) the manifest and check it against
/// `manifest_sha` ([`manifest_sha256`] in production), download (or reuse) and check every
/// archive, extract it, fold `lib` into `lib64`, then link the wheels' libraries from
/// `site_packages` in.
pub fn assemble(
    redist_url: &str,
    version: &str,
    manifest_sha: &str,
    archives_dir: &Path,
    cuda_home: &Path,
    site_packages: &Path,
) -> Result<(), TorchError> {
    let step = |message: String| TorchError::provision("cuda", message);
    let manifest_path = archives_dir.join(format!("redistrib_{version}.json"));
    let base = redist_url.trim_end_matches('/');
    crate::engine::download::download_file_verified(
        &format!("{base}/redistrib_{version}.json"),
        &manifest_path,
        Some(manifest_sha),
    )
    .map_err(|e| step(format!("{e:#}")))?;
    let manifest = std::fs::read_to_string(&manifest_path).map_err(|e| step(e.to_string()))?;
    let wanted = archives(&manifest).map_err(step)?;
    if cuda_home.exists() {
        std::fs::remove_dir_all(cuda_home).map_err(|e| step(e.to_string()))?;
    }
    std::fs::create_dir_all(cuda_home.join("lib64")).map_err(|e| step(e.to_string()))?;
    for archive in &wanted {
        let path = archives_dir.join(archive.file_name());
        crate::engine::download::download_file_verified(
            &format!("{base}/{}", archive.relative_path),
            &path,
            Some(&archive.sha256),
        )
        .map_err(|e| step(format!("{}: {e:#}", archive.part)))?;
        extract(&path, cuda_home).map_err(|e| step(format!("{}: {e}", archive.part)))?;
    }
    let lib = cuda_home.join("lib");
    if lib.is_dir() {
        link_or_move(&lib, &cuda_home.join("lib64")).map_err(|e| step(e.to_string()))?;
    }
    let nvidia = site_packages.join("nvidia");
    for name in WHEEL_LIBRARIES {
        for (from, to) in [("include", "include"), ("lib", "lib64")] {
            let dir = nvidia.join(name).join(from);
            if dir.is_dir() {
                std::fs::create_dir_all(cuda_home.join(to)).map_err(|e| step(e.to_string()))?;
                link_tree(&dir, &cuda_home.join(to)).map_err(|e| step(e.to_string()))?;
            }
        }
    }
    unversioned_links(&cuda_home.join("lib64")).map_err(|e| step(e.to_string()))?;
    Ok(())
}

/// Move everything in `from` into `to`, then remove `from`.
pub(super) fn link_or_move(from: &Path, to: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if std::fs::symlink_metadata(&target).is_err() {
            std::fs::rename(entry.path(), target)?;
        }
    }
    std::fs::remove_dir_all(from)
}

/// `tar -xJf <archive> -C <into> --strip-components=1`.
pub(super) fn extract(archive: &Path, into: &Path) -> Result<(), String> {
    let output = crate::process::command("tar")
        .arg("-xJf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .arg("--strip-components=1")
        .output()
        .map_err(|e| format!("running tar: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "tar failed on {}: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// `<venv>/lib/python3.10/site-packages` for a `3.10.x` interpreter.
pub fn site_packages(venv: &Path, python: &str) -> PathBuf {
    let minor: Vec<&str> = python.split('.').take(2).collect();
    venv.join("lib")
        .join(format!("python{}", minor.join(".")))
        .join("site-packages")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> String {
        let mut json = serde_json::json!({ "release_date": "2024-04-17" });
        for part in PARTS {
            json[part] = serde_json::json!({ REDIST_PLATFORM: {
                "relative_path": format!("{part}/linux-x86_64/{part}-linux-x86_64-12.4.1-archive.tar.xz"),
                "sha256": format!("{part}-sum"),
            }, "linux-sbsa": { "relative_path": "elsewhere" } });
        }
        json.to_string()
    }

    #[test]
    fn the_manifest_names_every_part_with_its_checksum() {
        let found = archives(&manifest()).unwrap();
        assert_eq!(found.len(), PARTS.len());
        assert_eq!(found[0].part, "cuda_nvcc");
        assert_eq!(found[0].sha256, "cuda_nvcc-sum");
        assert_eq!(
            found[0].file_name(),
            "cuda_nvcc-linux-x86_64-12.4.1-archive.tar.xz"
        );
    }

    #[test]
    fn a_manifest_missing_a_part_or_broken_is_refused() {
        let mut json: serde_json::Value = serde_json::from_str(&manifest()).unwrap();
        json["cuda_nvtx"][REDIST_PLATFORM]
            .as_object_mut()
            .unwrap()
            .remove("sha256");
        let err = archives(&json.to_string()).unwrap_err();
        assert!(err.contains("cuda_nvtx linux-x86_64 sha256"), "{err}");
        assert!(archives("not json").unwrap_err().contains("not JSON"));
    }

    #[test]
    fn only_the_pinned_release_has_a_manifest_checksum() {
        assert!(manifest_sha256("12.4.1").is_some());
        assert!(manifest_sha256("12.8.0").is_none());
    }

    #[test]
    fn site_packages_follow_the_minor_version() {
        assert_eq!(
            site_packages(Path::new("/e/.venv"), "3.10.22"),
            Path::new("/e/.venv/lib/python3.10/site-packages")
        );
    }

    #[cfg(unix)]
    #[test]
    fn wheel_libraries_are_linked_in_and_get_unversioned_names() {
        let dir = tempfile::tempdir().unwrap();
        let wheel = dir.path().join("nvidia/cublas");
        std::fs::create_dir_all(wheel.join("include/sub")).unwrap();
        std::fs::create_dir_all(wheel.join("lib")).unwrap();
        std::fs::write(wheel.join("include/sub/cublas.h"), "h").unwrap();
        std::fs::write(wheel.join("lib/libcublas.so.12"), "so").unwrap();
        let home = dir.path().join("cuda");
        std::fs::create_dir_all(home.join("include")).unwrap();
        std::fs::write(home.join("include/cublas.h"), "kept").unwrap();
        link_tree(&wheel.join("include"), &home.join("include")).unwrap();
        std::fs::create_dir_all(home.join("lib64")).unwrap();
        link_tree(&wheel.join("lib"), &home.join("lib64")).unwrap();
        unversioned_links(&home.join("lib64")).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join("include/sub/cublas.h")).unwrap(),
            "h"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("lib64/libcublas.so")).unwrap(),
            "so"
        );
        unversioned_links(&home.join("lib64")).unwrap();
    }
}
