//! Whether this machine can run the torch engine: Linux x86_64 with an NVIDIA GPU new enough for
//! the extensions' kernels.

use std::fmt;

use super::TRACE_TARGET;

/// The oldest GPU generation the extensions build for (Ampere): bf16 and NATTEN's kernels.
/// Safe range: 8.0 and up.
pub const MIN_COMPUTE_CAPABILITY: ComputeCapability = ComputeCapability { major: 8, minor: 0 };

/// An NVIDIA GPU's compute capability, as `nvidia-smi --query-gpu=compute_cap` prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ComputeCapability {
    pub major: u32,
    pub minor: u32,
}

impl ComputeCapability {
    /// Parse `8.9`.
    pub fn parse(text: &str) -> Option<Self> {
        let (major, minor) = text.trim().split_once('.')?;
        Some(Self {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        })
    }
}

impl fmt::Display for ComputeCapability {
    /// `8.9`: the spelling of `TORCH_CUDA_ARCH_LIST` and `NATTEN_CUDA_ARCH` too.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// What the engine can do here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Platform {
    Supported(ComputeCapability),
    Unsupported(String),
}

impl Platform {
    pub fn capability(&self) -> Option<ComputeCapability> {
        match self {
            Platform::Supported(capability) => Some(*capability),
            Platform::Unsupported(_) => None,
        }
    }
}

/// Decide from the target and `nvidia-smi`'s answer (its stdout, or `None` when it did not run).
pub fn classify(linux_x86_64: bool, nvidia_smi: Option<&str>) -> Platform {
    if !linux_x86_64 {
        return Platform::Unsupported("the torch engine runs on Linux x86_64 only".into());
    }
    let Some(stdout) = nvidia_smi else {
        return Platform::Unsupported("no NVIDIA GPU: nvidia-smi did not answer".into());
    };
    let Some(capability) = stdout.lines().next().and_then(ComputeCapability::parse) else {
        return Platform::Unsupported(format!(
            "nvidia-smi gave no compute capability ({:?})",
            stdout.trim()
        ));
    };
    if capability < MIN_COMPUTE_CAPABILITY {
        return Platform::Unsupported(format!(
            "the GPU's compute capability {capability} is below {MIN_COMPUTE_CAPABILITY}"
        ));
    }
    Platform::Supported(capability)
}

/// This machine, probed once per process (the engine roster is built more than once), and logged
/// when it is.
pub fn probe() -> Platform {
    static PROBED: std::sync::OnceLock<Platform> = std::sync::OnceLock::new();
    PROBED.get_or_init(probe_now).clone()
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn probe_now() -> Platform {
    let linux_x86_64 = cfg!(all(target_os = "linux", target_arch = "x86_64"));
    let nvidia_smi = linux_x86_64
        .then(|| {
            crate::process::command("nvidia-smi")
                .args(["--query-gpu=compute_cap", "--format=csv,noheader"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        })
        .flatten();
    let platform = classify(linux_x86_64, nvidia_smi.as_deref());
    log(&platform);
    platform
}

pub fn log(platform: &Platform) {
    match platform {
        Platform::Supported(capability) => tracing::info!(
            target: TRACE_TARGET,
            op = "platform",
            compute_capability = %capability,
            "mesh jobs advertised: an NVIDIA GPU the extensions build for"
        ),
        Platform::Unsupported(reason) => tracing::info!(
            target: TRACE_TARGET,
            op = "platform",
            reason = %reason,
            "mesh jobs not advertised"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_4090_is_supported() {
        assert_eq!(
            classify(true, Some("8.9\n")),
            Platform::Supported(ComputeCapability { major: 8, minor: 9 })
        );
    }

    #[test]
    fn the_first_gpu_decides() {
        assert_eq!(
            classify(true, Some("12.0\n7.5\n")).capability(),
            Some(ComputeCapability {
                major: 12,
                minor: 0
            })
        );
    }

    #[test]
    fn every_other_machine_says_why_not() {
        let reason = |p: Platform| match p {
            Platform::Unsupported(reason) => reason,
            Platform::Supported(_) => panic!("expected unsupported"),
        };
        assert!(reason(classify(false, Some("8.9"))).contains("Linux x86_64"));
        assert!(reason(classify(true, None)).contains("no NVIDIA GPU"));
        assert!(reason(classify(true, Some("[N/A]"))).contains("no compute capability"));
        assert!(reason(classify(true, Some("7.5"))).contains("below 8.0"));
    }

    #[test]
    fn a_capability_prints_as_the_arch_list_spells_it() {
        assert_eq!(
            ComputeCapability::parse(" 8.6 ").unwrap().to_string(),
            "8.6"
        );
        assert_eq!(ComputeCapability::parse("8"), None);
    }

    #[test]
    fn the_verdict_is_logged() {
        let logs = crate::test_support::capture(|| {
            log(&classify(true, Some("8.9")));
            log(&classify(true, None));
        });
        assert!(logs.contains("op=\"platform\""), "{logs}");
        assert!(logs.contains("mesh jobs advertised"), "{logs}");
        assert!(logs.contains("mesh jobs not advertised"), "{logs}");
    }
}
