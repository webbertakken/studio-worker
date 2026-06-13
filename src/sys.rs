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
use std::sync::OnceLock;

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

/// Cached result of the (relatively expensive) VRAM probe.  Total VRAM
/// is a static hardware property, so we probe at most once per process —
/// `build_capabilities` runs on every 5s heartbeat and must not spawn an
/// `nvidia-smi` subprocess each tick.
static VRAM_GB: OnceLock<f32> = OnceLock::new();

/// Detect physical VRAM on the host, in GB.  Returns 0.0 when we can't
/// probe (no NVIDIA GPU, no driver) — the engine still runs in synthetic
/// mode for low-end / CI machines.
///
/// This intentionally avoids a hard dependency on `nvml-wrapper` because
/// it brings a heavy NVML build dep that we don't want at the CI layer.
/// On Linux we first try the dependency-free
/// `/proc/driver/nvidia/gpus/*/information` sysfs probe; current NVIDIA
/// drivers (5xx) dropped the `Video Memory` line from that file, so we
/// fall back to `nvidia-smi` (which ships with every driver, on every
/// platform, and whose `--query-gpu` interface is stable across
/// versions).  The result is memoised since it can't change while the
/// process runs.
pub fn detect_vram_gb() -> Result<f32> {
    Ok(*VRAM_GB.get_or_init(probe_vram_gb))
}

fn probe_vram_gb() -> f32 {
    // Linux exposes a cheap, dependency-free sysfs probe; try it first
    // so the common case never spawns a subprocess.
    #[cfg(target_os = "linux")]
    {
        let from_sysfs = detect_vram_gb_from_sysfs(Path::new("/proc/driver/nvidia/gpus"));
        if from_sysfs > 0.0 {
            return from_sysfs;
        }
    }
    // Fallback for every platform: `nvidia-smi`.  On a host with no
    // NVIDIA tooling the command simply fails to spawn and we return 0.
    detect_vram_gb_via_nvidia_smi().unwrap_or(0.0)
}

/// Probe VRAM via `nvidia-smi --query-gpu=memory.total`.  Returns `None`
/// when the binary is absent (no driver / non-NVIDIA host) or exits
/// non-zero, in which cases the caller defaults to 0 GB.
fn detect_vram_gb_via_nvidia_smi() -> Option<f32> {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output();
    match output {
        Ok(o) if o.status.success() => vram_gb_from_smi_stdout(&String::from_utf8_lossy(&o.stdout)),
        Ok(o) => {
            tracing::warn!(
                target: "studio_worker::sys",
                op = "probe_vram",
                source = "nvidia_smi_failed",
                code = ?o.status.code(),
                "nvidia-smi exited non-zero while probing VRAM — defaulting to 0 GB"
            );
            None
        }
        Err(e) => {
            tracing::info!(
                target: "studio_worker::sys",
                op = "probe_vram",
                source = "nvidia_smi_absent",
                error = %e,
                "nvidia-smi not available — cannot probe VRAM; defaulting to 0 GB"
            );
            None
        }
    }
}

/// Convert the stdout of an `nvidia-smi` memory query to GB and emit the
/// probe breadcrumb.  Split out from the subprocess plumbing so the
/// parse + conversion + logging are unit-testable without a real
/// `nvidia-smi` on the box (CI has none).
fn vram_gb_from_smi_stdout(stdout: &str) -> Option<f32> {
    let mib = parse_nvidia_smi_mib(stdout)?;
    let vram_gb = (mib / 1024.0) as f32;
    tracing::info!(
        target: "studio_worker::sys",
        op = "probe_vram",
        source = "nvidia_smi",
        vram_gb = vram_gb,
        "detected NVIDIA VRAM via nvidia-smi fallback"
    );
    Some(vram_gb)
}

/// Sum the per-GPU MiB totals from
/// `nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits`.
/// One line per GPU, each a bare MiB integer (e.g. `24564`).  Tolerates
/// a trailing unit token (if `nounits` is ever dropped) and ignores
/// blank / `[N/A]` lines.  Returns `None` when no line yielded a number.
fn parse_nvidia_smi_mib(stdout: &str) -> Option<f64> {
    let mut total: f64 = 0.0;
    let mut any = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(mib) = trimmed
            .split_whitespace()
            .next()
            .and_then(|tok| tok.parse::<f64>().ok())
        {
            total += mib;
            any = true;
        }
    }
    any.then_some(total)
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
///   no `Video Memory` line was readable (current 5xx drivers dropped
///   it).  The caller then falls back to `nvidia-smi`; the warn is the
///   breadcrumb that the cheap sysfs path no longer works on this host.
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
            "NVIDIA sysfs entries present but no Video Memory line (current 5xx drivers dropped it) — falling back to nvidia-smi"
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

    // -----------------------------------------------------------------
    // nvidia-smi fallback — current NVIDIA drivers (5xx) dropped the
    // "Video Memory" line from the sysfs `information` file, so the
    // sysfs probe yields 0 on otherwise-capable hosts.  `nvidia-smi`
    // ships with every driver and its `--query-gpu` interface is stable
    // across versions, so it's the layout-proof fallback.
    // -----------------------------------------------------------------

    #[test]
    fn parse_nvidia_smi_mib_reads_a_single_bare_value() {
        assert_eq!(parse_nvidia_smi_mib("24564\n"), Some(24564.0));
    }

    #[test]
    fn parse_nvidia_smi_mib_sums_multiple_gpus() {
        assert_eq!(parse_nvidia_smi_mib("24564\n24564\n"), Some(49128.0));
    }

    #[test]
    fn parse_nvidia_smi_mib_tolerates_units_and_crlf_whitespace() {
        // If `nounits` is ever dropped the value arrives as "24564 MiB".
        assert_eq!(parse_nvidia_smi_mib("  24564 MiB \r\n"), Some(24564.0));
    }

    #[test]
    fn parse_nvidia_smi_mib_returns_none_on_empty_or_na() {
        assert_eq!(parse_nvidia_smi_mib(""), None);
        assert_eq!(parse_nvidia_smi_mib("\n[N/A]\n"), None);
    }

    #[test]
    fn vram_gb_from_smi_stdout_converts_mib_to_gb() {
        // 24564 MiB / 1024 = 23.99 GiB
        let gb = vram_gb_from_smi_stdout("24564\n").unwrap();
        assert!((gb - 23.99).abs() < 0.05, "got {gb}");
    }

    #[test]
    fn vram_gb_from_smi_stdout_is_none_when_unparseable() {
        assert_eq!(vram_gb_from_smi_stdout("\n[N/A]\n"), None);
    }

    #[test]
    fn vram_gb_from_smi_stdout_emits_info_breadcrumb_on_success() {
        let logs = crate::test_support::capture(|| {
            let _ = vram_gb_from_smi_stdout("24564\n");
        });
        assert!(logs.contains("INFO"), "expected INFO level, got: {logs}");
        assert!(
            logs.contains("op=\"probe_vram\""),
            "expected probe_vram op, got: {logs}"
        );
        assert!(
            logs.contains("source=\"nvidia_smi\""),
            "expected source=nvidia_smi, got: {logs}"
        );
    }
}
