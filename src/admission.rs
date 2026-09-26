//! Admission: does a load or transient job fit in device memory now?
//! (see `docs/runtime/model-lifecycle.md`).
//!
//! Free memory is probed on the first GPU (where the engines place
//! models), not summed across GPUs: a model must fit on one device.

const TRACE_TARGET: &str = "studio_worker::lifecycle";

/// Device memory kept free for runtime overheads the catalogue estimates
/// do not cover (CUDA context, KV growth, allocator slack).  Measured:
/// Measured: Nemotron streaming ran at 3.5 GiB on CUDA against ~2.6 GiB of weights.
/// Safe range 0.5..=2.0.
pub const ADMISSION_MARGIN_GIB: f32 = 1.0;

/// Reads the device's memory.  `SystemProbe` in production; faked in tests.
pub trait MemoryProbe {
    fn free_gib(&self) -> Option<f32>;
    fn total_gib(&self) -> Option<f32>;
}

/// How much device memory is free, and how we know.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FreeMemory {
    /// Measured on the device now.
    Probed { gib: f32 },
    /// No live probe: total minus what the worker itself has loaded.
    Accounted { total_gib: f32, loaded_gib: f32 },
    /// Neither probe nor total: nothing can be admitted safely.
    Unknown,
}

impl FreeMemory {
    pub fn gib(&self) -> f32 {
        match *self {
            Self::Probed { gib } => gib,
            Self::Accounted {
                total_gib,
                loaded_gib,
            } => (total_gib - loaded_gib).max(0.0),
            Self::Unknown => 0.0,
        }
    }
}

/// A load or job that does not fit.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error(
    "insufficient memory: needs {needed_gib:.2} GiB, {free_gib:.2} GiB free \
     ({margin_gib:.2} GiB kept in reserve)"
)]
pub struct Refused {
    pub needed_gib: f32,
    pub free_gib: f32,
    pub margin_gib: f32,
}

/// Measure free memory now, falling back to accounting when the device
/// cannot be probed.  `loaded_gib` is the sum of loaded estimates.
pub fn free_now(probe: &dyn MemoryProbe, loaded_gib: f32) -> FreeMemory {
    if let Some(gib) = probe.free_gib() {
        return FreeMemory::Probed { gib };
    }
    match probe.total_gib() {
        Some(total_gib) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "admit",
                total_gib,
                loaded_gib,
                "free memory probe unavailable; admitting against total minus loaded estimates"
            );
            FreeMemory::Accounted {
                total_gib,
                loaded_gib,
            }
        }
        None => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "admit",
                "no device memory probe available; nothing can be admitted"
            );
            FreeMemory::Unknown
        }
    }
}

/// Admit `needed_gib` if it fits in `free` minus the reserve.
pub fn admit(needed_gib: f32, free: &FreeMemory) -> Result<(), Refused> {
    let free_gib = free.gib();
    if *free != FreeMemory::Unknown && needed_gib <= free_gib - ADMISSION_MARGIN_GIB {
        Ok(())
    } else {
        Err(Refused {
            needed_gib,
            free_gib,
            margin_gib: ADMISSION_MARGIN_GIB,
        })
    }
}

/// First GPU's free memory from `nvidia-smi --query-gpu=memory.free
/// --format=csv,noheader,nounits` (MiB per line), in GiB.
pub fn first_gpu_free_gib(stdout: &str) -> Option<f32> {
    let mib: f32 = stdout.lines().next()?.trim().parse().ok()?;
    Some(mib / 1024.0)
}

/// AMD DRM sysfs: free = `mem_info_vram_total` - `mem_info_vram_used`.
pub fn amd_free_gib(total: &str, used: &str) -> Option<f32> {
    let total: u64 = total.trim().parse().ok()?;
    let used: u64 = used.trim().parse().ok()?;
    Some((total.saturating_sub(used) as f64 / (1024.0 * 1024.0 * 1024.0)) as f32)
}

/// The host's real probe: `nvidia-smi` first, then AMD DRM sysfs; total
/// from the existing VRAM detection.
pub struct SystemProbe;

impl MemoryProbe for SystemProbe {
    // Host-dependent IO (CI has no GPU); the parsers it delegates to are
    // unit-tested above.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn free_gib(&self) -> Option<f32> {
        let smi = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| first_gpu_free_gib(&String::from_utf8_lossy(&o.stdout)));
        smi.or_else(|| {
            let dev = std::path::Path::new("/sys/class/drm/card0/device");
            amd_free_gib(
                &std::fs::read_to_string(dev.join("mem_info_vram_total")).ok()?,
                &std::fs::read_to_string(dev.join("mem_info_vram_used")).ok()?,
            )
        })
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn total_gib(&self) -> Option<f32> {
        crate::sys::detect_vram_gb().ok().filter(|g| *g > 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProbe {
        free: Option<f32>,
        total: Option<f32>,
    }

    impl MemoryProbe for FakeProbe {
        fn free_gib(&self) -> Option<f32> {
            self.free
        }
        fn total_gib(&self) -> Option<f32> {
            self.total
        }
    }

    #[test]
    fn admits_when_the_estimate_fits_inside_free_minus_margin() {
        let free = FreeMemory::Probed { gib: 5.0 };
        assert_eq!(admit(3.9, &free), Ok(()));
        assert_eq!(admit(4.0, &free), Ok(()));
    }

    #[test]
    fn refuses_when_the_margin_would_be_eaten() {
        let free = FreeMemory::Probed { gib: 5.0 };
        assert_eq!(
            admit(4.1, &free),
            Err(Refused {
                needed_gib: 4.1,
                free_gib: 5.0,
                margin_gib: ADMISSION_MARGIN_GIB,
            })
        );
    }

    #[test]
    fn refusal_says_what_was_needed_and_what_was_free() {
        let r = Refused {
            needed_gib: 3.5,
            free_gib: 2.25,
            margin_gib: 1.0,
        };
        assert_eq!(
            r.to_string(),
            "insufficient memory: needs 3.50 GiB, 2.25 GiB free (1.00 GiB kept in reserve)"
        );
    }

    #[test]
    fn prefers_the_probed_free_memory() {
        let probe = FakeProbe {
            free: Some(7.5),
            total: Some(24.0),
        };
        assert_eq!(free_now(&probe, 10.0), FreeMemory::Probed { gib: 7.5 });
    }

    #[test]
    fn falls_back_to_accounting_and_logs_it() {
        let logs = crate::test_support::capture(|| {
            let probe = FakeProbe {
                free: None,
                total: Some(24.0),
            };
            assert_eq!(
                free_now(&probe, 10.0),
                FreeMemory::Accounted {
                    total_gib: 24.0,
                    loaded_gib: 10.0
                }
            );
        });
        assert!(logs.contains("free memory probe unavailable"), "{logs}");
    }

    #[test]
    fn with_no_probe_at_all_nothing_is_admitted() {
        let probe = FakeProbe {
            free: None,
            total: None,
        };
        let free = free_now(&probe, 0.0);
        assert_eq!(free, FreeMemory::Unknown);
        assert!(admit(0.1, &free).is_err());
    }

    #[test]
    fn accounted_free_is_total_minus_loaded_never_negative() {
        assert_eq!(
            FreeMemory::Accounted {
                total_gib: 24.0,
                loaded_gib: 10.0
            }
            .gib(),
            14.0
        );
        assert_eq!(
            FreeMemory::Accounted {
                total_gib: 8.0,
                loaded_gib: 10.0
            }
            .gib(),
            0.0
        );
        assert_eq!(FreeMemory::Unknown.gib(), 0.0);
    }

    #[test]
    fn parses_the_first_gpu_free_mib_from_nvidia_smi() {
        assert_eq!(first_gpu_free_gib("3703\n12000\n"), Some(3703.0 / 1024.0));
        assert_eq!(first_gpu_free_gib(" 1024 \n"), Some(1.0));
        assert_eq!(first_gpu_free_gib(""), None);
        assert_eq!(first_gpu_free_gib("[N/A]\n"), None);
    }

    #[test]
    fn parses_amd_free_from_total_and_used_bytes() {
        let gib = 1024.0 * 1024.0 * 1024.0;
        assert_eq!(
            amd_free_gib("17179869184\n", "4294967296\n"),
            Some((17179869184.0 - 4294967296.0) / gib)
        );
        assert_eq!(amd_free_gib("garbage", "1"), None);
        assert_eq!(amd_free_gib("1", "2"), Some(0.0));
    }
}
