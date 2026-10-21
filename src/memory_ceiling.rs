//! The worker's memory ceiling: on Linux the tray UI starts inside the transient systemd user scope
//! `studio-worker.scope` with `MemoryHigh` / `MemoryMax`, and the daemon and every engine child
//! inherit it.  See `docs/runtime/memory-ceiling.md`.
//!
//! The pure parts (limits, the launcher argv, cgroup parsing, the assessment of what is in
//! effect) are unit-tested; the probes that run `systemd-run` / `systemctl` or read `/proc` are
//! thin wrappers around them.

use std::ffi::OsString;
use std::path::Path;

use crate::config::Config;

const TRACE_TARGET: &str = "studio_worker::memory_ceiling";

/// The ceiling when the config does not say, in GB.  Half of a 64 GB workstation: room for the
/// largest offloading image model seen (an `sd-cli` holding 21-27 GB) and the daemon.
pub const DEFAULT_MEMORY_MAX_GB: u32 = 32;
/// The lowest ceiling accepted, in GB: the daemon plus the smallest engine.
pub const MIN_MEMORY_MAX_GB: u32 = 4;
/// The highest ceiling accepted, in GB.
pub const MAX_MEMORY_MAX_GB: u32 = 1024;
/// `MemoryHigh` as a fraction of `MemoryMax`: above it the kernel reclaims hard, so a job is
/// throttled before anything is killed.  Safe range 0.8..=0.95.
pub const MEMORY_HIGH_FRACTION: f64 = 0.9;
/// The scope's unit name, as `systemd-run --unit` takes it.
pub const SCOPE_NAME: &str = "studio-worker";
/// The scope's full unit name.
pub const SCOPE_UNIT: &str = "studio-worker.scope";

/// A memory ceiling within the accepted range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryCeiling {
    max_gb: u32,
}

/// A ceiling outside [`MIN_MEMORY_MAX_GB`]..=[`MAX_MEMORY_MAX_GB`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("must be a whole number of GB from {MIN_MEMORY_MAX_GB} to {MAX_MEMORY_MAX_GB} (got {got})")]
pub struct OutOfRange {
    pub got: u32,
}

impl Default for MemoryCeiling {
    fn default() -> Self {
        Self {
            max_gb: DEFAULT_MEMORY_MAX_GB,
        }
    }
}

impl MemoryCeiling {
    /// A ceiling of `max_gb`, if it is in range.
    pub fn new(max_gb: u32) -> Result<Self, OutOfRange> {
        if (MIN_MEMORY_MAX_GB..=MAX_MEMORY_MAX_GB).contains(&max_gb) {
            Ok(Self { max_gb })
        } else {
            Err(OutOfRange { got: max_gb })
        }
    }

    /// The config's ceiling; a hand-edited value out of range is logged and the default used.
    pub fn from_config(cfg: &Config) -> Self {
        Self::or_default(cfg.memory_max_gb)
    }

    /// A ceiling of `max_gb`; out of range, logged and the default used.
    pub fn or_default(max_gb: u32) -> Self {
        Self::new(max_gb).unwrap_or_else(|e| {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "memory_ceiling",
                memory_max_gb = max_gb,
                default_gb = DEFAULT_MEMORY_MAX_GB,
                error = %e,
                "memory_max_gb in config.toml is out of range; using the default"
            );
            Self::default()
        })
    }

    pub fn max_gb(self) -> u32 {
        self.max_gb
    }

    /// `MemoryMax`, as systemd reads it: `32G`.
    pub fn memory_max(self) -> String {
        format!("{}G", self.max_gb)
    }

    /// `MemoryHigh`, as systemd reads it, in whole MiB: `29491M` for 32 GB.
    pub fn memory_high(self) -> String {
        let mib = (f64::from(self.max_gb) * 1024.0 * MEMORY_HIGH_FRACTION).floor() as u64;
        format!("{mib}M")
    }

    /// The scope's properties: `MemoryHigh=…`, `MemoryMax=…`.
    pub fn properties(self) -> [String; 2] {
        [
            format!("MemoryHigh={}", self.memory_high()),
            format!("MemoryMax={}", self.memory_max()),
        ]
    }

    /// The scope launcher: what goes before the tray UI's own command line.
    pub fn launcher(self) -> Vec<String> {
        let [high, max] = self.properties();
        vec![
            "systemd-run".into(),
            "--user".into(),
            "--scope".into(),
            format!("--unit={SCOPE_NAME}"),
            "--collect".into(),
            "--quiet".into(),
            "-p".into(),
            high,
            "-p".into(),
            max,
        ]
    }
}

/// The argv that starts the tray UI: `exe args`, behind the scope launcher when `ceiling` is set.
pub fn tray_argv(ceiling: Option<MemoryCeiling>, exe: &Path, args: &[OsString]) -> Vec<OsString> {
    let mut argv: Vec<OsString> = ceiling
        .map(|c| c.launcher().into_iter().map(OsString::from).collect())
        .unwrap_or_default();
    argv.push(exe.as_os_str().to_owned());
    argv.extend(args.iter().cloned());
    argv
}

// ---------------------------------------------------------------------------
// cgroup v2 parsing
// ---------------------------------------------------------------------------

/// A cgroup memory limit file's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// `max`: no limit.
    Unlimited,
    Bytes(u64),
}

impl Limit {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "max" => Some(Self::Unlimited),
            n => n.parse().ok().map(Self::Bytes),
        }
    }

    /// For the logs: `max`, or the bytes.
    pub fn describe(self) -> String {
        match self {
            Self::Unlimited => "max".into(),
            Self::Bytes(b) => b.to_string(),
        }
    }
}

/// The cgroup v2 path in `/proc/<pid>/cgroup` (`0::/user.slice/…`).
pub fn cgroup_path(proc_cgroup: &str) -> Option<&str> {
    proc_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
        .filter(|p| p.starts_with('/'))
}

/// Whether `cgroup` is the worker's scope.
pub fn is_worker_scope(cgroup: &str) -> bool {
    cgroup.rsplit('/').next() == Some(SCOPE_UNIT)
}

/// `anon` from a cgroup's `memory.stat`, in bytes.
pub fn anon_bytes(memory_stat: &str) -> Option<u64> {
    memory_stat
        .lines()
        .find_map(|line| line.strip_prefix("anon "))
        .and_then(|n| n.trim().parse().ok())
}

/// What the daemon's own cgroup says about memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub cgroup: String,
    /// `memory.max`; `None` when the file is missing (memory controller not enabled).
    pub max: Option<Limit>,
    pub high: Option<Limit>,
}

/// Whether a ceiling holds this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    InEffect {
        cgroup: String,
        max: Limit,
        high: Option<Limit>,
    },
    NotInScope {
        cgroup: String,
    },
    NoLimit {
        cgroup: String,
    },
    /// The cgroup could not be read.
    Unknown,
}

/// Judge `effective`.
pub fn assess(effective: Option<Effective>) -> Assessment {
    let Some(e) = effective else {
        return Assessment::Unknown;
    };
    if !is_worker_scope(&e.cgroup) {
        return Assessment::NotInScope { cgroup: e.cgroup };
    }
    match e.max {
        Some(max @ Limit::Bytes(_)) => Assessment::InEffect {
            cgroup: e.cgroup,
            max,
            high: e.high,
        },
        _ => Assessment::NoLimit { cgroup: e.cgroup },
    }
}

/// Log what [`assess`] found, beside the configured ceiling.
pub fn log_assessment(assessment: &Assessment, configured: MemoryCeiling) {
    match assessment {
        Assessment::InEffect { cgroup, max, high } => tracing::info!(
            target: TRACE_TARGET,
            op = "memory_ceiling",
            cgroup = %cgroup,
            memory_max = %max.describe(),
            memory_high = %high.map(Limit::describe).unwrap_or_else(|| "unknown".into()),
            configured_gb = configured.max_gb(),
            "memory ceiling in effect"
        ),
        Assessment::NotInScope { cgroup } => tracing::warn!(
            target: TRACE_TARGET,
            op = "memory_ceiling",
            cgroup = %cgroup,
            configured_gb = configured.max_gb(),
            reason = "not in studio-worker.scope: started outside the login entry, or without systemd",
            "no memory ceiling in effect"
        ),
        Assessment::NoLimit { cgroup } => tracing::warn!(
            target: TRACE_TARGET,
            op = "memory_ceiling",
            cgroup = %cgroup,
            configured_gb = configured.max_gb(),
            reason = "the scope has no memory limit: is the memory controller delegated to the user manager?",
            "no memory ceiling in effect"
        ),
        Assessment::Unknown => tracing::warn!(
            target: TRACE_TARGET,
            op = "memory_ceiling",
            configured_gb = configured.max_gb(),
            reason = "/proc/self/cgroup is unreadable",
            "no memory ceiling in effect"
        ),
    }
}

// ---------------------------------------------------------------------------
// Host IO (Linux)
// ---------------------------------------------------------------------------

/// The daemon's own cgroup and its memory limits.
#[cfg(target_os = "linux")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn read_effective() -> Option<Effective> {
    let proc_cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let cgroup = cgroup_path(&proc_cgroup)?.to_string();
    let dir = Path::new("/sys/fs/cgroup").join(cgroup.trim_start_matches('/'));
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .and_then(|raw| Limit::parse(&raw))
    };
    Some(Effective {
        max: read("memory.max"),
        high: read("memory.high"),
        cgroup,
    })
}

/// Anonymous memory and limit of the daemon's own cgroup, for host-memory admission.
#[cfg(target_os = "linux")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn own_cgroup_usage() -> Option<(Limit, u64)> {
    let proc_cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let dir = Path::new("/sys/fs/cgroup").join(cgroup_path(&proc_cgroup)?.trim_start_matches('/'));
    let max = Limit::parse(&std::fs::read_to_string(dir.join("memory.max")).ok()?)?;
    let anon = anon_bytes(&std::fs::read_to_string(dir.join("memory.stat")).ok()?)?;
    Some((max, anon))
}

#[cfg(not(target_os = "linux"))]
pub fn own_cgroup_usage() -> Option<(Limit, u64)> {
    None
}

/// Log the ceiling in effect for the daemon (at start).
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn log_effective(configured: MemoryCeiling) {
    #[cfg(target_os = "linux")]
    log_assessment(&assess(read_effective()), configured);
    #[cfg(not(target_os = "linux"))]
    tracing::info!(
        target: TRACE_TARGET,
        op = "memory_ceiling",
        configured_gb = configured.max_gb(),
        "no memory ceiling on this platform"
    );
}

/// Make the scope's limits the configured ones, when the daemon runs inside the scope.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn reconcile(ceiling: MemoryCeiling) {
    #[cfg(target_os = "linux")]
    {
        let in_scope = read_effective().is_some_and(|e| is_worker_scope(&e.cgroup));
        if !in_scope {
            tracing::info!(
                target: TRACE_TARGET,
                op = "memory_ceiling",
                configured_gb = ceiling.max_gb(),
                "not in studio-worker.scope; the ceiling applies from the next start through the login entry"
            );
            return;
        }
        let [high, max] = ceiling.properties();
        let out = crate::process::command("systemctl")
            .args([
                "--user",
                "set-property",
                "--runtime",
                SCOPE_UNIT,
                &high,
                &max,
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => tracing::info!(
                target: TRACE_TARGET,
                op = "memory_ceiling",
                unit = SCOPE_UNIT,
                memory_high = %ceiling.memory_high(),
                memory_max = %ceiling.memory_max(),
                "scope memory limits set"
            ),
            Ok(o) => tracing::warn!(
                target: TRACE_TARGET,
                op = "memory_ceiling",
                unit = SCOPE_UNIT,
                status = %o.status,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "could not set the scope's memory limits"
            ),
            Err(e) => tracing::warn!(
                target: TRACE_TARGET,
                op = "memory_ceiling",
                unit = SCOPE_UNIT,
                error = %e,
                "could not run systemctl to set the scope's memory limits"
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = ceiling;
}

/// The ceiling to start the tray UI under: `Some` on Linux when the scope launcher works
/// (`systemd-run --user --scope … true` exits 0), `None` otherwise.  Logs the outcome.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn launch_ceiling(ceiling: MemoryCeiling) -> Option<MemoryCeiling> {
    #[cfg(target_os = "linux")]
    {
        let [_, max] = ceiling.properties();
        let probe = crate::process::command("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "-p",
                &max,
                "true",
            ])
            .output();
        match probe {
            Ok(o) if o.status.success() => {
                tracing::debug!(
                    target: TRACE_TARGET,
                    op = "memory_ceiling",
                    memory_max = %ceiling.memory_max(),
                    "systemd-run --user works; the tray UI starts in studio-worker.scope"
                );
                Some(ceiling)
            }
            Ok(o) => {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "memory_ceiling",
                    status = %o.status,
                    stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                    "systemd-run --user does not work; the tray UI starts without a memory ceiling"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "memory_ceiling",
                    error = %e,
                    "systemd-run is not available; the tray UI starts without a memory ceiling"
                );
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ceiling;
        None
    }
}

/// Whether `studio-worker.scope` is active: a worker already runs in it.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn scope_active() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::process::command("systemctl")
            .args(["--user", "is-active", "--quiet", SCOPE_UNIT])
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(not(target_os = "linux"))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;

    #[test]
    fn a_ceiling_is_accepted_only_inside_its_range() {
        assert_eq!(MemoryCeiling::new(4).unwrap().max_gb(), 4);
        assert_eq!(MemoryCeiling::new(1024).unwrap().max_gb(), 1024);
        assert_eq!(MemoryCeiling::new(3), Err(OutOfRange { got: 3 }));
        assert_eq!(MemoryCeiling::new(0), Err(OutOfRange { got: 0 }));
        assert_eq!(MemoryCeiling::new(1025), Err(OutOfRange { got: 1025 }));
        assert_eq!(
            OutOfRange { got: 2 }.to_string(),
            "must be a whole number of GB from 4 to 1024 (got 2)"
        );
    }

    #[test]
    fn the_default_ceiling_is_32_gb() {
        assert_eq!(MemoryCeiling::default().max_gb(), 32);
    }

    #[test]
    fn memory_high_sits_a_little_below_memory_max() {
        let c = MemoryCeiling::new(32).unwrap();
        assert_eq!(c.memory_max(), "32G");
        assert_eq!(c.memory_high(), "29491M");
        assert_eq!(c.properties(), ["MemoryHigh=29491M", "MemoryMax=32G"]);
        assert_eq!(MemoryCeiling::new(4).unwrap().memory_high(), "3686M");
    }

    #[test]
    fn an_out_of_range_config_value_falls_back_to_the_default_and_says_so() {
        let cfg = Config {
            memory_max_gb: 1,
            ..Config::default()
        };
        let logs = capture(move || assert_eq!(MemoryCeiling::from_config(&cfg).max_gb(), 32));
        assert!(logs.contains("out of range; using the default"), "{logs}");
        let cfg = Config {
            memory_max_gb: 48,
            ..Config::default()
        };
        assert_eq!(MemoryCeiling::from_config(&cfg).max_gb(), 48);
    }

    #[test]
    fn the_launcher_starts_a_named_collected_scope_with_both_limits() {
        assert_eq!(
            MemoryCeiling::new(32).unwrap().launcher(),
            [
                "systemd-run",
                "--user",
                "--scope",
                "--unit=studio-worker",
                "--collect",
                "--quiet",
                "-p",
                "MemoryHigh=29491M",
                "-p",
                "MemoryMax=32G",
            ]
        );
    }

    #[test]
    fn the_tray_argv_puts_the_launcher_before_the_exe_only_with_a_ceiling() {
        let exe = Path::new("/opt/sw/studio-worker");
        let args = [OsString::from("ui")];
        assert_eq!(
            tray_argv(None, exe, &args),
            [OsString::from("/opt/sw/studio-worker"), "ui".into()]
        );
        let argv = tray_argv(Some(MemoryCeiling::default()), exe, &args);
        assert_eq!(argv[0], "systemd-run");
        assert_eq!(argv[argv.len() - 2], "/opt/sw/studio-worker");
        assert_eq!(argv[argv.len() - 1], "ui");
        assert!(argv.contains(&OsString::from("MemoryMax=32G")));
    }

    #[test]
    fn parses_cgroup_limits() {
        assert_eq!(Limit::parse("max\n"), Some(Limit::Unlimited));
        assert_eq!(
            Limit::parse("34359738368\n"),
            Some(Limit::Bytes(34_359_738_368))
        );
        assert_eq!(Limit::parse("garbage"), None);
        assert_eq!(Limit::Unlimited.describe(), "max");
        assert_eq!(Limit::Bytes(5).describe(), "5");
    }

    #[test]
    fn finds_the_cgroup_v2_path_and_recognises_the_worker_scope() {
        let raw =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/studio-worker.scope\n";
        let path = cgroup_path(raw).unwrap();
        assert!(is_worker_scope(path));
        assert_eq!(
            cgroup_path("12:memory:/x\n0::/user.slice/user-1000.slice/session-c1.scope\n"),
            Some("/user.slice/user-1000.slice/session-c1.scope")
        );
        assert!(!is_worker_scope(
            "/user.slice/user-1000.slice/session-c1.scope"
        ));
        assert!(!is_worker_scope("/app.slice/studio-worker.scope/child"));
        assert_eq!(cgroup_path("12:memory:/x\n"), None);
    }

    #[test]
    fn reads_anonymous_memory_from_memory_stat() {
        let stat = "anon_thp 0\nanon 1073741824\nfile 42\n";
        assert_eq!(anon_bytes(stat), Some(1_073_741_824));
        assert_eq!(anon_bytes("file 1\n"), None);
    }

    fn effective(cgroup: &str, max: Option<Limit>) -> Option<Effective> {
        Some(Effective {
            cgroup: cgroup.into(),
            max,
            high: Some(Limit::Bytes(1)),
        })
    }

    #[test]
    fn a_ceiling_is_in_effect_only_inside_the_scope_with_a_limit() {
        let scope = "/user.slice/app.slice/studio-worker.scope";
        assert_eq!(
            assess(effective(scope, Some(Limit::Bytes(8)))),
            Assessment::InEffect {
                cgroup: scope.into(),
                max: Limit::Bytes(8),
                high: Some(Limit::Bytes(1)),
            }
        );
        assert_eq!(
            assess(effective(scope, Some(Limit::Unlimited))),
            Assessment::NoLimit {
                cgroup: scope.into()
            }
        );
        assert_eq!(
            assess(effective(scope, None)),
            Assessment::NoLimit {
                cgroup: scope.into()
            }
        );
        assert_eq!(
            assess(effective("/session-c1.scope", Some(Limit::Bytes(8)))),
            Assessment::NotInScope {
                cgroup: "/session-c1.scope".into()
            }
        );
        assert_eq!(assess(None), Assessment::Unknown);
    }

    #[test]
    fn every_assessment_is_logged_with_its_fingerprint() {
        let c = MemoryCeiling::default();
        let cases = [
            (
                Assessment::InEffect {
                    cgroup: "/a/studio-worker.scope".into(),
                    max: Limit::Bytes(34_359_738_368),
                    high: Some(Limit::Bytes(30_922_964_992)),
                },
                "memory ceiling in effect",
                "memory_max=34359738368",
            ),
            (
                Assessment::NotInScope {
                    cgroup: "/s.scope".into(),
                },
                "no memory ceiling in effect",
                "not in studio-worker.scope",
            ),
            (
                Assessment::NoLimit {
                    cgroup: "/a/studio-worker.scope".into(),
                },
                "no memory ceiling in effect",
                "memory controller",
            ),
            (
                Assessment::Unknown,
                "no memory ceiling in effect",
                "unreadable",
            ),
        ];
        for (assessment, message, detail) in cases {
            let logs = capture(move || log_assessment(&assessment, c));
            assert!(logs.contains("op=\"memory_ceiling\""), "{logs}");
            assert!(logs.contains(message), "{logs}");
            assert!(logs.contains(detail), "{logs}");
            assert!(logs.contains("configured_gb=32"), "{logs}");
        }
    }
}
