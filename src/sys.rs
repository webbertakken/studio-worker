//! Host-system probes: hostname, OS user, VRAM.
use anyhow::Result;

pub fn machine_name() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "unknown-host".to_string())
}

pub fn username() -> String {
    whoami::username()
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
    {
        if let Ok(entries) = std::fs::read_dir("/proc/driver/nvidia/gpus") {
            let mut total_mib: f64 = 0.0;
            for entry in entries.flatten() {
                let info_path = entry.path().join("information");
                if let Ok(content) = std::fs::read_to_string(&info_path) {
                    for line in content.lines() {
                        if let Some(rest) = line.trim().strip_prefix("Video Memory:") {
                            if let Some(mib) = parse_mib(rest) {
                                total_mib += mib;
                            }
                        }
                    }
                }
            }
            if total_mib > 0.0 {
                return Ok((total_mib / 1024.0) as f32);
            }
        }
    }
    Ok(0.0)
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
}
