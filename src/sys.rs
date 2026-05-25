//! Host-system probes: hostname, OS user, VRAM.
//!
//! Every probe emits a structured tracing breadcrumb so an operator can
//! tell from the logs *why* a worker reports the values it does (in
//! particular, why VRAM came back as `0.0` — was the sysfs tree missing,
//! present-but-unparseable, or is the worker running on a non-Linux
//! host?).  Silent `0.0` makes "this worker claims nothing" impossible
//! to diagnose from logs alone.
use anyhow::Result;
use std::path::Path;

pub fn machine_name() -> String {
    let name = hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "unknown-host".to_string());
    tracing::debug!(
        target: "studio_worker::sys",
        op = "machine_name",
        value = %name,
        "resolved host machine name"
    );
    name
}

pub fn username() -> String {
    let user = whoami::username();
    tracing::debug!(
        target: "studio_worker::sys",
        op = "username",
        value = %user,
        "resolved OS user"
    );
    user
}

/// Detect physical VRAM on the host, in GB.  Returns 0.0 when we can't
/// probe (no NVIDIA GPU, no driver) — the engine still runs in synthetic
/// mode for low-end / CI machines.
///
/// This intentionally avoids a hard dependency on `nvml-wrapper` because
/// it brings a heavy NVML build dep that we don't want at the CI layer.
/// We probe `/proc/driver/nvidia/gpus/*/information` on Linux and just
/// return 0 elsewhere.
pub fn detect_vram_gb() -> Result<f32> {
    #[cfg(target_os = "linux")]
    let gb = detect_vram_gb_from_sysfs(Path::new("/proc/driver/nvidia/gpus"));
    #[cfg(not(target_os = "linux"))]
    let gb = {
        tracing::info!(
            target: "studio_worker::sys",
            op = "probe_vram",
            source = "unsupported_platform",
            vram_gb = 0.0,
            "VRAM probe unsupported on this OS — defaulting to 0 GB"
        );
        0.0_f32
    };
    Ok(gb)
}

/// VRAM probe driven by a configurable sysfs root.  Public-in-crate so
/// the integration tests can exercise both the "missing root" and
/// "populated root" branches without a real `/proc/driver/nvidia` tree.
///
/// Emits exactly one tracing event per call describing the outcome:
///
/// - `INFO source="no_nvidia_sysfs"` — `root` is not a directory.  This
///   is the normal case on CI runners / non-GPU hosts.
/// - `INFO source="nvidia_sysfs"` — at least one GPU's `information`
///   file was parseable.  `gpu_count` reflects how many contributed.
/// - `WARN source="sysfs_unparseable"` — directories were present but
///   no `Video Memory` line was readable.  This is the surprising case
///   we want operators to notice (e.g. driver version bump).
pub fn detect_vram_gb_from_sysfs(root: &Path) -> f32 {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => {
            tracing::info!(
                target: "studio_worker::sys",
                op = "probe_vram",
                source = "no_nvidia_sysfs",
                vram_gb = 0.0,
                root = %root.display(),
                "no NVIDIA sysfs tree at probe root — defaulting to 0 GB VRAM"
            );
            return 0.0;
        }
    };

    let mut total_mib: f64 = 0.0;
    let mut gpu_count: u32 = 0;
    let mut parseable: u32 = 0;
    for entry in entries.flatten() {
        gpu_count += 1;
        let info_path = entry.path().join("information");
        if let Ok(content) = std::fs::read_to_string(&info_path) {
            let mut found = false;
            for line in content.lines() {
                if let Some(rest) = line.trim().strip_prefix("Video Memory:") {
                    if let Some(mib) = parse_mib(rest) {
                        total_mib += mib;
                        found = true;
                    }
                }
            }
            if found {
                parseable += 1;
            }
        }
    }

    let vram_gb = (total_mib / 1024.0) as f32;
    if parseable > 0 {
        tracing::info!(
            target: "studio_worker::sys",
            op = "probe_vram",
            source = "nvidia_sysfs",
            vram_gb = vram_gb,
            gpu_count = parseable,
            "detected NVIDIA VRAM via sysfs"
        );
    } else {
        tracing::warn!(
            target: "studio_worker::sys",
            op = "probe_vram",
            source = "sysfs_unparseable",
            vram_gb = 0.0,
            gpu_count = gpu_count,
            root = %root.display(),
            "NVIDIA sysfs entries present but no Video Memory line parsed — driver layout change?"
        );
    }
    vram_gb
}

fn parse_mib(s: &str) -> Option<f64> {
    // Strings look like " 24576 MiB" or "24576 MB"
    let trimmed = s.trim();
    let mut parts = trimmed.split_whitespace();
    let value = parts.next()?.parse::<f64>().ok()?;
    let unit = parts.next().unwrap_or("MiB");
    match unit.to_ascii_lowercase().as_str() {
        "mib" | "mb" => Some(value),
        "gib" | "gb" => Some(value * 1024.0),
        _ => Some(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mib_handles_mib() {
        assert_eq!(parse_mib(" 24576 MiB"), Some(24576.0));
        assert_eq!(parse_mib("12288 MB"), Some(12288.0));
        assert_eq!(parse_mib("24 GiB"), Some(24576.0));
    }

    #[test]
    fn machine_name_returns_non_empty() {
        assert!(!machine_name().is_empty());
    }

    #[test]
    fn username_returns_non_empty() {
        assert!(!username().is_empty());
    }

    #[test]
    fn detect_vram_gb_from_sysfs_returns_zero_when_root_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(detect_vram_gb_from_sysfs(&missing), 0.0);
    }

    #[test]
    fn detect_vram_gb_from_sysfs_sums_parseable_gpus() {
        let dir = tempfile::tempdir().unwrap();
        for (bus, mib) in [("0000:01:00.0", "12288"), ("0000:02:00.0", "24576")] {
            let gpu = dir.path().join(bus);
            std::fs::create_dir_all(&gpu).unwrap();
            std::fs::write(
                gpu.join("information"),
                format!("Model: x\nVideo Memory: {mib} MiB\n"),
            )
            .unwrap();
        }
        // (12288 + 24576) / 1024 = 36 GiB
        let gb = detect_vram_gb_from_sysfs(dir.path());
        assert!((gb - 36.0).abs() < 1e-3, "got {gb}");
    }
}
