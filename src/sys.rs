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
    username_from_probe(whoami::username())
}

/// Resolve the OS-user probe into a username, logging the outcome so a
/// silent fallback can't hide a failing probe.  `whoami::username`
/// became fallible in whoami 2.x; on the error path we emit a `warn`
/// breadcrumb naming the underlying error and fall back to
/// `unknown-user`, mirroring `machine_name`'s `unknown-host` default.
fn username_from_probe<E: std::fmt::Display>(probe: std::result::Result<String, E>) -> String {
    let user = match probe {
        Ok(user) => user,
        Err(e) => {
            tracing::warn!(
                target: "studio_worker::sys",
                op = "username",
                error = %e,
                "failed to resolve OS user; falling back to unknown-user"
            );
            "unknown-user".to_string()
        }
    };
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
    // Linux exposes cheap, dependency-free sysfs probes; try them first
    // so the common case never spawns a subprocess.
    #[cfg(target_os = "linux")]
    {
        let from_sysfs = detect_vram_gb_from_sysfs(Path::new("/proc/driver/nvidia/gpus"));
        if from_sysfs > 0.0 {
            return from_sysfs;
        }
        // AMD (and Intel discrete) expose VRAM via the DRM sysfs tree.
        let from_amd = detect_vram_gb_from_amd_sysfs(Path::new("/sys/class/drm"));
        if from_amd > 0.0 {
            return from_amd;
        }
    }
    // Apple Silicon / Intel Macs: unified memory, sized via sysctl.
    #[cfg(target_os = "macos")]
    {
        if let Some(gb) = detect_vram_gb_via_sysctl() {
            return gb;
        }
    }
    // NVIDIA on any platform: `nvidia-smi`.  Absent on a non-NVIDIA
    // host, where the command simply fails to spawn.
    if let Some(gb) = detect_vram_gb_via_nvidia_smi() {
        return gb;
    }
    // Windows non-NVIDIA (AMD / Intel): CIM `Win32_VideoController`.
    #[cfg(target_os = "windows")]
    {
        if let Some(gb) = detect_vram_gb_via_wmic() {
            return gb;
        }
    }
    0.0
}

/// Bytes → GiB (matching the NVIDIA MiB/1024 path, so every vendor
/// reports the same unit).
fn bytes_to_gib(bytes: u64) -> f32 {
    (bytes as f64 / (1024.0 * 1024.0 * 1024.0)) as f32
}

/// True for a DRM primary-node dir name (`card0`, `card1`, …) but not
/// the connector (`card0-DP-1`) or render (`renderD128`) siblings.
fn is_drm_card_dir(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// Sum VRAM across every AMD/Intel-discrete GPU exposed under the DRM
/// sysfs tree (`<root>/card*/device/mem_info_vram_total`, a byte
/// count).  Integrated GPUs and drivers that don't publish the file
/// simply contribute nothing, so an iGPU-only box returns 0 and the
/// caller keeps the configured threshold as its only signal.
pub fn detect_vram_gb_from_amd_sysfs(root: &Path) -> f32 {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return 0.0,
    };
    let mut total_bytes: u64 = 0;
    let mut cards: u32 = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_drm_card_dir(&name) {
            continue;
        }
        let vram_file = entry.path().join("device").join("mem_info_vram_total");
        if let Ok(content) = std::fs::read_to_string(&vram_file) {
            if let Some(bytes) = parse_amd_vram_total_bytes(&content) {
                total_bytes = total_bytes.saturating_add(bytes);
                cards += 1;
            }
        }
    }
    let gib = bytes_to_gib(total_bytes);
    if cards > 0 {
        tracing::info!(
            target: "studio_worker::sys",
            op = "probe_vram",
            source = "amd_drm_sysfs",
            vram_gb = gib,
            cards,
            "detected VRAM via AMD/DRM sysfs"
        );
    }
    gib
}

/// Parse an AMD `mem_info_vram_total` file: a single decimal byte
/// count, possibly with trailing whitespace.
pub fn parse_amd_vram_total_bytes(content: &str) -> Option<u64> {
    content.trim().parse::<u64>().ok().filter(|b| *b > 0)
}

/// Parse `sysctl -n hw.memsize` (total RAM in bytes) into a usable VRAM
/// figure for Apple unified memory.  `fraction` is the share of unified
/// memory we treat as GPU-addressable (macOS lets the GPU use most of
/// it; 0.75 is a conservative, widely-cited figure).
pub fn parse_sysctl_memsize(stdout: &str, fraction: f64) -> Option<f32> {
    let bytes = stdout.trim().parse::<u64>().ok().filter(|b| *b > 0)?;
    Some((bytes_to_gib(bytes) as f64 * fraction) as f32)
}

/// Sum the `AdapterRAM` values from a Windows CIM/WMIC
/// `Win32_VideoController` dump (one integer per adapter, bytes).
/// Ignores non-numeric lines (headers, blanks) so it tolerates both
/// `wmic` and PowerShell `Get-CimInstance` output shapes.
pub fn parse_wmic_adapter_ram(stdout: &str) -> Option<f32> {
    let mut total: u64 = 0;
    let mut found = false;
    for line in stdout.lines() {
        if let Some(bytes) = line.trim().parse::<u64>().ok().filter(|b| *b > 0) {
            total = total.saturating_add(bytes);
            found = true;
        }
    }
    found.then(|| bytes_to_gib(total))
}

/// Probe VRAM via `nvidia-smi --query-gpu=memory.total`.  Returns `None`
/// when the binary is absent (no driver / non-NVIDIA host) or exits
/// non-zero, in which cases the caller defaults to 0 GB.
///
/// Coverage-off: spawning a real `nvidia-smi` is host-dependent (CI has
/// none), so its success / non-zero-exit arms can't be exercised
/// deterministically.  The parse + GB conversion + logging it delegates
/// to ([`vram_gb_from_smi_stdout`], [`parse_nvidia_smi_mib`]) are
/// unit-tested directly.
#[cfg_attr(coverage_nightly, coverage(off))]
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

/// Live Apple unified-memory probe via `sysctl -n hw.memsize`.
/// Coverage-off: host-dependent; the parse is unit-tested via
/// [`parse_sysctl_memsize`].
#[cfg(target_os = "macos")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn detect_vram_gb_via_sysctl() -> Option<f32> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_sysctl_memsize(&String::from_utf8_lossy(&output.stdout), 0.75)
}

/// Live Windows non-NVIDIA probe via PowerShell CIM
/// (`Win32_VideoController.AdapterRAM`).  Coverage-off: host-dependent;
/// the parse is unit-tested via [`parse_wmic_adapter_ram`].  Note
/// `AdapterRAM` is a UInt32 and saturates at ~4 GiB on larger cards — a
/// documented Windows limitation, but a conservative floor beats 0.
#[cfg(target_os = "windows")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn detect_vram_gb_via_wmic() -> Option<f32> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_VideoController).AdapterRAM",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_wmic_adapter_ram(&String::from_utf8_lossy(&output.stdout))
}

/// Summed VRAM (MiB) from an `nvidia-smi` memory query plus the count of
/// GPU lines that were dropped from that total.
///
/// `dropped` is the number of non-empty lines whose leading token wasn't
/// a number — nvidia-smi emits `[N/A]` for `memory.total` when a card has
/// fallen off the bus, hit an ECC fault, or sits in a MIG state with no
/// resolvable total.  Carrying the count (rather than silently summing
/// the survivors) means a multi-GPU box that under-reports its VRAM — and
/// then refuses jobs it could actually run — leaves a breadcrumb instead
/// of vanishing the card without a trace.
struct SmiMemTotal {
    mib: f64,
    dropped: u32,
}

/// Convert the stdout of an `nvidia-smi` memory query to GB and emit the
/// probe breadcrumb.  Split out from the subprocess plumbing so the
/// parse + conversion + logging are unit-testable without a real
/// `nvidia-smi` on the box (CI has none).
fn vram_gb_from_smi_stdout(stdout: &str) -> Option<f32> {
    let SmiMemTotal { mib, dropped } = parse_nvidia_smi_mib(stdout)?;
    let vram_gb = (mib / 1024.0) as f32;
    tracing::info!(
        target: "studio_worker::sys",
        op = "probe_vram",
        source = "nvidia_smi",
        vram_gb = vram_gb,
        dropped = dropped,
        "detected NVIDIA VRAM via nvidia-smi fallback"
    );
    Some(vram_gb)
}

/// Sum the per-GPU MiB totals from
/// `nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits`.
/// One line per GPU, each a bare MiB integer (e.g. `24564`).  Tolerates
/// a trailing unit token (if `nounits` is ever dropped) and ignores
/// blank lines.  Every non-empty line that fails to parse (e.g. `[N/A]`)
/// is warn-logged and counted in [`SmiMemTotal::dropped`] before being
/// left out of the total.  Returns `None` when no line yielded a number.
fn parse_nvidia_smi_mib(stdout: &str) -> Option<SmiMemTotal> {
    let mut total: f64 = 0.0;
    let mut any = false;
    let mut dropped: u32 = 0;
    for (idx, line) in stdout.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match trimmed
            .split_whitespace()
            .next()
            .and_then(|tok| tok.parse::<f64>().ok())
        {
            Some(mib) => {
                total += mib;
                any = true;
            }
            None => {
                dropped += 1;
                tracing::warn!(
                    target: "studio_worker::sys",
                    op = "probe_vram",
                    source = "nvidia_smi",
                    line = idx,
                    content = trimmed,
                    "nvidia-smi VRAM line did not parse as MiB — dropping this GPU from the total"
                );
            }
        }
    }
    any.then_some(SmiMemTotal {
        mib: total,
        dropped,
    })
}

/// VRAM probe driven by a configurable sysfs root.  Public-in-crate so
/// the integration tests can exercise both the "missing root" and
/// "populated root" branches without a real `/proc/driver/nvidia` tree.
///
/// Emits a summary tracing event per call, plus a `WARN` for every GPU
/// dropped from the total so a multi-GPU box never under-reports its
/// VRAM silently:
///
/// - `INFO source="no_nvidia_sysfs"` — `root` is not a directory.  This
///   is the normal case on CI runners / non-GPU hosts.
/// - `INFO source="nvidia_sysfs"` — at least one GPU's `information`
///   file was parseable.  `gpu_count` is how many contributed; `dropped`
///   is how many were present but unreadable / had no parseable `Video
///   Memory` line (each of those also gets its own `WARN` naming it).
/// - `WARN source="sysfs_unparseable"` — directories were present but
///   none parseable (current 5xx drivers dropped the `Video Memory`
///   line).  The caller then falls back to `nvidia-smi`; the warn is the
///   breadcrumb that the cheap sysfs path no longer works on this host.
/// - `WARN source="nvidia_sysfs" reason="no_video_memory_line"|"video_memory_unparseable"|"info_unreadable"`
///   — a specific GPU was dropped from the total while others survived.
///   `video_memory_unparseable` means the `Video Memory` line was
///   present but its value didn't parse (the warn echoes the offending
///   `content`); `no_video_memory_line` means no such line at all.
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
        let gpu_path = entry.path();
        let info_path = gpu_path.join("information");
        match std::fs::read_to_string(&info_path) {
            Ok(content) => {
                let mut found = false;
                // A `Video Memory:` line that's present but whose value
                // can't be parsed (e.g. `N/A` on a driver that stubbed
                // the field) must be surfaced differently from a GPU
                // with no such line at all — otherwise the operator is
                // told the line is missing when it's right there.  Keep
                // the first offending value to echo in the warn.
                let mut unparseable: Option<String> = None;
                for line in content.lines() {
                    if let Some(rest) = line.trim().strip_prefix("Video Memory:") {
                        if let Some(mib) = parse_mib(rest) {
                            total_mib += mib;
                            found = true;
                        } else if unparseable.is_none() {
                            unparseable = Some(rest.trim().to_string());
                        }
                    }
                }
                if found {
                    parseable += 1;
                } else if let Some(content) = unparseable {
                    tracing::warn!(
                        target: "studio_worker::sys",
                        op = "probe_vram",
                        source = "nvidia_sysfs",
                        reason = "video_memory_unparseable",
                        gpu = %gpu_path.display(),
                        content = content.as_str(),
                        "sysfs GPU Video Memory line did not parse as MiB — dropping it from the total"
                    );
                } else {
                    tracing::warn!(
                        target: "studio_worker::sys",
                        op = "probe_vram",
                        source = "nvidia_sysfs",
                        reason = "no_video_memory_line",
                        gpu = %gpu_path.display(),
                        "sysfs GPU has no parseable Video Memory line — dropping it from the total"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "studio_worker::sys",
                    op = "probe_vram",
                    source = "nvidia_sysfs",
                    reason = "info_unreadable",
                    gpu = %gpu_path.display(),
                    error = %e,
                    "could not read a sysfs GPU information file — dropping it from the total"
                );
            }
        }
    }

    let vram_gb = (total_mib / 1024.0) as f32;
    let dropped = gpu_count.saturating_sub(parseable);
    if parseable > 0 {
        tracing::info!(
            target: "studio_worker::sys",
            op = "probe_vram",
            source = "nvidia_sysfs",
            vram_gb = vram_gb,
            gpu_count = parseable,
            dropped = dropped,
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
        assert_eq!(parse_mib("8 GB"), Some(8192.0));
    }

    #[test]
    fn parse_mib_defaults_to_mib_when_the_unit_is_omitted() {
        // A bare value (no unit token) is assumed to already be in MiB,
        // matching nvidia-smi's `--units` output where the suffix is
        // sometimes stripped.
        assert_eq!(parse_mib("4096"), Some(4096.0));
    }

    #[test]
    fn parse_mib_treats_an_unknown_unit_as_raw_mib() {
        // An unrecognised suffix must not silently zero the GPU out of
        // the VRAM total: the worker claims jobs by VRAM, so dropping a
        // card to 0 would make it refuse work it can actually run. We
        // keep the numeric value as-is (best-effort MiB) rather than
        // returning `None`.
        assert_eq!(parse_mib("2048 KiB"), Some(2048.0));
        assert_eq!(parse_mib("4 TB"), Some(4.0));
    }

    #[test]
    fn parse_mib_rejects_unparseable_or_empty_values() {
        // A non-numeric leading token (e.g. an `[N/A]` placeholder) or
        // an empty / whitespace-only line yields `None` so the caller
        // skips it instead of polluting the total with a bogus number.
        assert_eq!(parse_mib("N/A MiB"), None);
        assert_eq!(parse_mib(""), None);
        assert_eq!(parse_mib("   "), None);
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
    fn username_from_probe_returns_the_resolved_value() {
        let user = username_from_probe(Ok::<_, std::io::Error>("alice".to_string()));
        assert_eq!(user, "alice");
    }

    #[test]
    fn username_from_probe_falls_back_to_unknown_user_on_error() {
        let user =
            username_from_probe(Err::<String, _>(std::io::Error::other("no entropy source")));
        assert_eq!(user, "unknown-user");
    }

    #[test]
    fn username_from_probe_warns_with_the_error_on_failure() {
        // whoami 2.x made the probe fallible; a failure must leave an
        // operator-visible breadcrumb naming the error rather than a
        // silent fallback that hides why the user came back unknown.
        let logs = crate::test_support::capture(|| {
            let _ =
                username_from_probe(Err::<String, _>(std::io::Error::other("permission denied")));
        });
        assert!(logs.contains("WARN"), "expected WARN level, got: {logs}");
        assert!(
            logs.contains("op=\"username\""),
            "expected username op, got: {logs}"
        );
        assert!(
            logs.contains("permission denied"),
            "expected underlying error, got: {logs}"
        );
    }

    #[test]
    fn username_from_probe_emits_debug_value_on_success() {
        let logs = crate::test_support::capture(|| {
            let _ = username_from_probe(Ok::<_, std::io::Error>("bob".to_string()));
        });
        assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
        assert!(
            logs.contains("value=bob"),
            "expected resolved value, got: {logs}"
        );
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

    #[test]
    fn detect_vram_gb_from_sysfs_sums_only_survivors_when_one_gpu_is_unreadable() {
        // A healthy card next to one whose `information` can't be read
        // (here a *directory* named `information`, so `read_to_string`
        // fails on every platform): the survivor still totals, the bad
        // card is dropped from the sum rather than zeroing the host out.
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("0000:01:00.0");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("information"), "Video Memory: 12288 MiB\n").unwrap();
        let bad = dir.path().join("0000:02:00.0");
        std::fs::create_dir_all(bad.join("information")).unwrap();
        // Only the healthy card's 12288 MiB / 1024 = 12 GiB counts.
        let gb = detect_vram_gb_from_sysfs(dir.path());
        assert!((gb - 12.0).abs() < 1e-3, "got {gb}");
    }

    // -----------------------------------------------------------------
    // AMD / Intel-discrete VRAM via the DRM sysfs tree, and the Apple /
    // Windows non-NVIDIA parsers.  These give the threshold sanity
    // check + studio matching a real number on non-NVIDIA GPUs, which
    // all reported 0 before.
    // -----------------------------------------------------------------

    #[test]
    fn is_drm_card_dir_matches_only_primary_nodes() {
        assert!(is_drm_card_dir("card0"));
        assert!(is_drm_card_dir("card12"));
        assert!(!is_drm_card_dir("card0-DP-1"));
        assert!(!is_drm_card_dir("renderD128"));
        assert!(!is_drm_card_dir("card"));
        assert!(!is_drm_card_dir("controlD64"));
    }

    #[test]
    fn parse_amd_vram_total_bytes_reads_a_byte_count() {
        assert_eq!(
            parse_amd_vram_total_bytes("17163091968\n"),
            Some(17163091968)
        );
        assert_eq!(
            parse_amd_vram_total_bytes("0"),
            None,
            "zero = no VRAM file value"
        );
        assert_eq!(parse_amd_vram_total_bytes("N/A"), None);
        assert_eq!(parse_amd_vram_total_bytes(""), None);
    }

    #[test]
    fn detect_vram_gb_from_amd_sysfs_sums_cards_and_ignores_siblings() {
        let dir = tempfile::tempdir().unwrap();
        // card0 = 16 GiB, card1 = 8 GiB.
        for (card, bytes) in [
            ("card0", 16u64 * 1024 * 1024 * 1024),
            ("card1", 8 * 1024 * 1024 * 1024),
        ] {
            let dev = dir.path().join(card).join("device");
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::write(dev.join("mem_info_vram_total"), bytes.to_string()).unwrap();
        }
        // A connector node + a render node must be ignored.
        std::fs::create_dir_all(dir.path().join("card0-DP-1")).unwrap();
        std::fs::create_dir_all(dir.path().join("renderD128")).unwrap();
        // An iGPU card with no VRAM file contributes nothing.
        std::fs::create_dir_all(dir.path().join("card2").join("device")).unwrap();

        let gb = detect_vram_gb_from_amd_sysfs(dir.path());
        assert!((gb - 24.0).abs() < 1e-3, "expected 24 GiB, got {gb}");
    }

    #[test]
    fn detect_vram_gb_from_amd_sysfs_returns_zero_without_a_tree() {
        let missing = std::path::Path::new("/definitely/no/drm/here");
        assert_eq!(detect_vram_gb_from_amd_sysfs(missing), 0.0);
    }

    #[test]
    fn parse_sysctl_memsize_scales_unified_memory() {
        // 32 GiB unified memory, 75% GPU-addressable = 24 GiB.
        let bytes = (32u64 * 1024 * 1024 * 1024).to_string();
        let gb = parse_sysctl_memsize(&bytes, 0.75).unwrap();
        assert!((gb - 24.0).abs() < 1e-2, "got {gb}");
        assert_eq!(parse_sysctl_memsize("0", 0.75), None);
        assert_eq!(parse_sysctl_memsize("garbage", 0.75), None);
    }

    #[test]
    fn parse_wmic_adapter_ram_sums_adapters_and_ignores_noise() {
        // Two adapters: 8 GiB + 4 GiB, with a header + blank lines.
        let out = format!(
            "AdapterRAM\n\n{}\n{}\n",
            8u64 * 1024 * 1024 * 1024,
            4u64 * 1024 * 1024 * 1024
        );
        let gb = parse_wmic_adapter_ram(&out).unwrap();
        assert!((gb - 12.0).abs() < 1e-2, "got {gb}");
        assert_eq!(parse_wmic_adapter_ram("AdapterRAM\n\n"), None, "no numbers");
        assert_eq!(parse_wmic_adapter_ram(""), None);
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
        let total = parse_nvidia_smi_mib("24564\n").unwrap();
        assert_eq!(total.mib, 24564.0);
        assert_eq!(total.dropped, 0);
    }

    #[test]
    fn parse_nvidia_smi_mib_sums_multiple_gpus() {
        let total = parse_nvidia_smi_mib("24564\n24564\n").unwrap();
        assert_eq!(total.mib, 49128.0);
        assert_eq!(total.dropped, 0);
    }

    #[test]
    fn parse_nvidia_smi_mib_tolerates_units_and_crlf_whitespace() {
        // If `nounits` is ever dropped the value arrives as "24564 MiB".
        let total = parse_nvidia_smi_mib("  24564 MiB \r\n").unwrap();
        assert_eq!(total.mib, 24564.0);
        assert_eq!(total.dropped, 0);
    }

    #[test]
    fn parse_nvidia_smi_mib_returns_none_on_empty_or_na() {
        assert!(parse_nvidia_smi_mib("").is_none());
        assert!(parse_nvidia_smi_mib("\n[N/A]\n").is_none());
    }

    #[test]
    fn parse_nvidia_smi_mib_sums_survivors_and_counts_a_dropped_gpu() {
        // A healthy 24 GiB card next to one nvidia-smi reports `[N/A]`
        // for (fell off the bus / ECC fault): the survivor's VRAM still
        // totals, but the dropped card is counted, not silently lost.
        let total = parse_nvidia_smi_mib("24564\n[N/A]\n24564\n").unwrap();
        assert_eq!(total.mib, 49128.0);
        assert_eq!(total.dropped, 1);
    }

    #[test]
    fn parse_nvidia_smi_mib_warns_on_each_dropped_gpu_line() {
        // A multi-GPU box that under-reports its VRAM (and then refuses
        // jobs it can run) must leave a per-line breadcrumb naming the
        // offending value, not vanish the card without a trace.
        let logs = crate::test_support::capture(|| {
            let _ = parse_nvidia_smi_mib("24564\n[N/A]\n");
        });
        assert!(logs.contains("WARN"), "expected WARN level, got: {logs}");
        assert!(
            logs.contains("op=\"probe_vram\""),
            "expected probe_vram op, got: {logs}"
        );
        assert!(
            logs.contains("source=\"nvidia_smi\""),
            "expected source=nvidia_smi, got: {logs}"
        );
        assert!(
            logs.contains("[N/A]"),
            "the warning must name the unparseable value, got: {logs}"
        );
        assert!(
            logs.contains("dropping this GPU"),
            "the warning must explain the drop, got: {logs}"
        );
    }

    #[test]
    fn vram_gb_from_smi_stdout_reports_dropped_count_in_breadcrumb() {
        // The success breadcrumb must surface how many GPUs were dropped
        // so a truncated VRAM total can't pass for a complete one.
        let logs = crate::test_support::capture(|| {
            let gb = vram_gb_from_smi_stdout("24564\n[N/A]\n").unwrap();
            assert!((gb - 23.99).abs() < 0.05, "survivor still totals: {gb}");
        });
        assert!(
            logs.contains("dropped=1"),
            "the breadcrumb must report the dropped count, got: {logs}"
        );
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
