//! Admission: does a load or transient job fit in device memory now, and
//! (for a job whose engine keeps its weights in host memory) in host memory?
//! (see `docs/runtime/model-lifecycle.md` and `docs/runtime/memory-ceiling.md`).
//!
//! Free memory is probed on the first GPU (where the engines place
//! models), not summed across GPUs: a model must fit on one device.

use crate::memory_ceiling::Limit;
use crate::types::ModelSource;

const TRACE_TARGET: &str = "studio_worker::lifecycle";

/// Device memory kept free for runtime overheads the catalogue estimates
/// do not cover (CUDA context, KV growth, allocator slack).
/// Measured: Nemotron streaming ran at 3.5 GiB on CUDA against ~2.6 GiB of weights.
/// Safe range 0.5..=2.0.
pub const ADMISSION_MARGIN_GIB: f32 = 1.0;

/// Host memory kept free beside a job's weights: what its runtime holds beyond them, and the
/// desktop's breathing room.  Evidence: an `sd-cli` offloading 13.85 GiB of weights held 21-27 GB.
/// A floor, not a forecast; the memory ceiling bounds the rest.  Safe range 2.0..=8.0.
pub const HOST_RAM_MARGIN_GIB: f32 = 4.0;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Reads the device's and the host's memory.  `SystemProbe` in production; faked in tests.
pub trait MemoryProbe {
    fn free_gib(&self) -> Option<f32>;
    fn total_gib(&self) -> Option<f32>;
    /// Host memory a job may still take: `MemAvailable`, capped by the memory ceiling's headroom.
    fn host_available_gib(&self) -> Option<f32>;
}

/// Which memory a job needs or was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Device,
    Host,
}

/// What a transient job needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryNeed {
    pub device_gib: f32,
    /// `Some` only for a job whose engine copies its weights into host memory.
    pub host_gib: Option<f32>,
}

impl MemoryNeed {
    /// Device memory only.
    pub fn device(gib: f32) -> Self {
        Self {
            device_gib: gib,
            host_gib: None,
        }
    }

    /// A job on `source` with the catalogue's (or the offer's) device estimate.
    pub fn of(device_gib: f32, source: &ModelSource) -> Self {
        Self {
            device_gib,
            host_gib: host_need_gib(source),
        }
    }
}

/// Host memory a job on `source` needs: the sum of its files' `approxBytes` when its engine
/// copies every weight into host memory (`offloadToCpu` without `mmap`); `None` otherwise.
/// A file without `approxBytes` counts nothing, so the need is a lower bound.
pub fn host_need_gib(source: &ModelSource) -> Option<f32> {
    let d = &source.cli_defaults;
    if d.offload_to_cpu != Some(true) || d.mmap == Some(true) {
        return None;
    }
    let bytes: u64 = source.files.iter().filter_map(|f| f.approx_bytes).sum();
    Some((bytes as f64 / GIB) as f32)
}

/// `MemAvailable` from `/proc/meminfo`, in GiB.
pub fn mem_available_gib(meminfo: &str) -> Option<f32> {
    let kib: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()?;
    Some((kib as f64 / (1024.0 * 1024.0)) as f32)
}

/// Host memory a job may take: the host's available memory, capped by what the worker's cgroup
/// limit leaves above its anonymous memory (file pages are reclaimable).
pub fn host_available_gib(mem_available: Option<f32>, cgroup: Option<(Limit, u64)>) -> Option<f32> {
    let headroom = match cgroup {
        Some((Limit::Bytes(max), anon)) => Some((max.saturating_sub(anon) as f64 / GIB) as f32),
        _ => None,
    };
    match (mem_available, headroom) {
        (Some(a), Some(h)) => Some(a.min(h)),
        (a, h) => a.or(h),
    }
}

/// Admit `needed_gib` of host memory if it fits in `available_gib` minus the reserve.
pub fn admit_host(needed_gib: f32, available_gib: f32) -> Result<(), Refused> {
    if needed_gib <= available_gib - HOST_RAM_MARGIN_GIB {
        Ok(())
    } else {
        Err(Refused {
            memory: MemoryKind::Host,
            needed_gib,
            free_gib: available_gib,
            margin_gib: HOST_RAM_MARGIN_GIB,
        })
    }
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Refused {
    pub memory: MemoryKind,
    pub needed_gib: f32,
    /// Free device memory, or available host memory.
    pub free_gib: f32,
    pub margin_gib: f32,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (memory, free) = match self.memory {
            MemoryKind::Device => ("device", "free"),
            MemoryKind::Host => ("host", "available"),
        };
        write!(
            f,
            "insufficient {memory} memory: needs {:.2} GiB, {:.2} GiB {free} ({:.2} GiB kept in \
             reserve)",
            self.needed_gib, self.free_gib, self.margin_gib
        )
    }
}

impl std::error::Error for Refused {}

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
            memory: MemoryKind::Device,
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
        let smi = crate::process::command("nvidia-smi")
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

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn host_available_gib(&self) -> Option<f32> {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok();
        host_available_gib(
            meminfo.as_deref().and_then(mem_available_gib),
            crate::memory_ceiling::own_cgroup_usage(),
        )
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
        fn host_available_gib(&self) -> Option<f32> {
            None
        }
    }

    fn source(offload: Option<bool>, mmap: Option<bool>, bytes: &[Option<u64>]) -> ModelSource {
        let mut source: ModelSource = serde_json::from_value(serde_json::json!({
            "engine": "sd-cpp",
            "files": [],
            "cliDefaults": { "cfgScale": 1.0, "steps": 1, "width": 64, "height": 64 }
        }))
        .unwrap();
        source.cli_defaults.offload_to_cpu = offload;
        source.cli_defaults.mmap = mmap;
        source.files = bytes
            .iter()
            .enumerate()
            .map(|(i, b)| crate::types::ModelFile {
                role: crate::types::ModelFileRole::DiffusionModel,
                url: format!("https://x/{i}"),
                filename: format!("f{i}"),
                approx_bytes: *b,
                sha256: None,
            })
            .collect();
        source
    }

    #[test]
    fn a_job_offloading_its_weights_to_the_cpu_needs_their_size_in_host_memory() {
        // qwen-image-2512, as the studio seeds it (offloadToCpu, no mmap).
        let qwen = source(
            Some(true),
            None,
            &[Some(9_932_896_800), Some(4_683_072_384), Some(253_806_246)],
        );
        let need = host_need_gib(&qwen).unwrap();
        assert!((need - 13.85).abs() < 0.01, "{need}");
        assert_eq!(MemoryNeed::of(12.0, &qwen).host_gib, Some(need));
        assert_eq!(
            host_need_gib(&source(Some(true), None, &[Some(GIB as u64), None])),
            Some(1.0),
            "a file without approxBytes counts nothing"
        );
    }

    #[test]
    fn mapped_or_device_resident_weights_need_no_host_check() {
        assert_eq!(
            host_need_gib(&source(Some(true), Some(true), &[Some(1)])),
            None
        );
        assert_eq!(host_need_gib(&source(None, None, &[Some(1)])), None);
        assert_eq!(host_need_gib(&source(Some(false), None, &[Some(1)])), None);
        assert_eq!(MemoryNeed::device(3.0).host_gib, None);
    }

    #[test]
    fn reads_mem_available_from_proc_meminfo() {
        let meminfo = "MemTotal:       65598464 kB\nMemFree:  1 kB\nMemAvailable:   47185920 kB\n";
        assert_eq!(mem_available_gib(meminfo), Some(45.0));
        assert_eq!(mem_available_gib("MemTotal: 1 kB\n"), None);
        assert_eq!(mem_available_gib("MemAvailable: lots\n"), None);
    }

    #[test]
    fn host_memory_is_the_smaller_of_the_host_and_the_ceiling_headroom() {
        let gib = GIB as u64;
        let capped = Some((Limit::Bytes(32 * gib), 10 * gib));
        assert_eq!(host_available_gib(Some(45.0), capped), Some(22.0));
        assert_eq!(host_available_gib(Some(15.0), capped), Some(15.0));
        assert_eq!(
            host_available_gib(Some(45.0), Some((Limit::Unlimited, gib))),
            Some(45.0)
        );
        assert_eq!(host_available_gib(None, capped), Some(22.0));
        assert_eq!(host_available_gib(Some(45.0), None), Some(45.0));
        assert_eq!(host_available_gib(None, None), None);
        assert_eq!(
            host_available_gib(None, Some((Limit::Bytes(gib), 2 * gib))),
            Some(0.0),
            "anon above the limit leaves no headroom"
        );
    }

    #[test]
    fn host_memory_is_admitted_inside_available_minus_its_margin() {
        assert_eq!(admit_host(13.85, 17.85), Ok(()));
        assert_eq!(
            admit_host(13.85, 17.0),
            Err(Refused {
                memory: MemoryKind::Host,
                needed_gib: 13.85,
                free_gib: 17.0,
                margin_gib: HOST_RAM_MARGIN_GIB,
            })
        );
        assert_eq!(
            admit_host(13.85, 17.0).unwrap_err().to_string(),
            "insufficient host memory: needs 13.85 GiB, 17.00 GiB available (4.00 GiB kept in \
             reserve)"
        );
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
                memory: MemoryKind::Device,
                needed_gib: 4.1,
                free_gib: 5.0,
                margin_gib: ADMISSION_MARGIN_GIB,
            })
        );
    }

    #[test]
    fn refusal_says_what_was_needed_and_what_was_free() {
        let r = Refused {
            memory: MemoryKind::Device,
            needed_gib: 3.5,
            free_gib: 2.25,
            margin_gib: 1.0,
        };
        assert_eq!(
            r.to_string(),
            "insufficient device memory: needs 3.50 GiB, 2.25 GiB free (1.00 GiB kept in reserve)"
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
