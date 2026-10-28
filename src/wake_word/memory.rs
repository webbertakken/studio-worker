//! Memory admission for a training's stages (`docs/engines/wake-word.md`, "Memory"): a stage starts
//! only when the machine and the worker's scope have room for what it was measured to need, and
//! waits (stage `waiting_for_memory`) until they do.

use std::time::{Duration, Instant};

use crate::memory_ceiling::{anon_bytes, Limit};

use super::stage::Phase;

const GIB: u64 = 1024 * 1024 * 1024;

/// What a stage needs, in bytes.  Train: its anonymous peak, 7.0 GiB with validation in chunks
/// (measured 2026-10-28 on a 2-language, 8-near-miss request), plus 5 GiB for the feature pages it
/// maps and keeps hot.  The others: under 2 GiB at smoke scale (measured), with room to spare.
/// Safe range: the measured peak and up.
pub fn need(phase: Phase) -> u64 {
    match phase {
        Phase::Train => 12 * GIB,
        _ => 4 * GIB,
    }
}

/// Memory kept free beyond a stage's need: earlyoom kills at 10 % of RAM available (its `-m`
/// default), so a stage never plans to take the machine below that, plus 2 GiB for the rest of the
/// machine's moments.
pub const RESERVE_SHARE: f64 = 0.10;
pub const RESERVE_MARGIN: u64 = 2 * GIB;
/// How long a stage waits for memory before the training fails `memory_unavailable`.
/// Safe range 10 to 240 minutes.
pub const WAIT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// How often a waiting stage looks again.  Safe range 5 to 120 s.
pub const POLL: Duration = Duration::from_secs(15);
/// How often a waiting stage says so in the log.
pub const LOG_EVERY: Duration = Duration::from_secs(60);

/// The machine's and the scope's memory now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// `MemTotal`.
    pub total: u64,
    /// `MemAvailable`: what the kernel can hand out without swapping.
    pub available: u64,
    /// What the worker's scope may still hold before its throttle (`memory.high`, else
    /// `memory.max`), counting its anonymous memory only (its page cache is reclaimable); `None`
    /// without a limit.
    pub scope_headroom: Option<u64>,
}

/// Whether a stage may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Admit,
    Wait(String),
}

/// A `kB` field of `/proc/meminfo`, in bytes.
fn meminfo_field(meminfo: &str, field: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix(field)?.strip_prefix(':')?;
        let kib: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
        Some(kib * 1024)
    })
}

/// The scope's headroom from its `memory.high`, `memory.max` and `memory.stat`.
pub fn scope_headroom(high: Option<&str>, max: Option<&str>, stat: &str) -> Option<u64> {
    let limit = |raw: Option<&str>| match raw.and_then(Limit::parse) {
        Some(Limit::Bytes(bytes)) => Some(bytes),
        _ => None,
    };
    let limit = match (limit(high), limit(max)) {
        (Some(h), Some(m)) => h.min(m),
        (one, other) => one.or(other)?,
    };
    Some(limit.saturating_sub(anon_bytes(stat).unwrap_or(0)))
}

/// A snapshot from `/proc/meminfo` and, when the worker runs in a scope, its cgroup's files.
pub fn snapshot(meminfo: &str, scope_headroom: Option<u64>) -> Option<Snapshot> {
    Some(Snapshot {
        total: meminfo_field(meminfo, "MemTotal")?,
        available: meminfo_field(meminfo, "MemAvailable")?,
        scope_headroom,
    })
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB as f64
}

/// Whether a stage needing `need` bytes may start now.
pub fn verdict(need: u64, now: &Snapshot) -> Verdict {
    let reserve = (now.total as f64 * RESERVE_SHARE) as u64 + RESERVE_MARGIN;
    if now.available < need + reserve {
        return Verdict::Wait(format!(
            "{:.1} GiB available; the stage needs {:.1} GiB and {:.1} GiB stays free",
            gib(now.available),
            gib(need),
            gib(reserve)
        ));
    }
    if let Some(headroom) = now.scope_headroom {
        if headroom < need {
            return Verdict::Wait(format!(
                "the worker's scope has {:.1} GiB left before its limit; the stage needs {:.1} GiB",
                gib(headroom),
                gib(need)
            ));
        }
    }
    Verdict::Admit
}

/// How waiting ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited {
    Admitted {
        waited: Duration,
    },
    /// Unreadable memory: the stage starts, and the log says so.
    Unknown,
    TimedOut {
        last: String,
    },
    Cancelled,
}

/// Wait until a stage needing `need` bytes may start, `timeout` at most.  `probe` reads the
/// machine; `on_wait` is told every time the stage still waits, with why.
pub fn wait_for(
    need: u64,
    probe: &dyn Fn() -> Option<Snapshot>,
    cancelled: &dyn Fn() -> bool,
    timeout: Duration,
    poll: Duration,
    on_wait: &mut dyn FnMut(&str),
) -> Waited {
    let started = Instant::now();
    loop {
        if cancelled() {
            return Waited::Cancelled;
        }
        let Some(now) = probe() else {
            return Waited::Unknown;
        };
        match verdict(need, &now) {
            Verdict::Admit => {
                return Waited::Admitted {
                    waited: started.elapsed(),
                }
            }
            Verdict::Wait(why) => {
                if started.elapsed() >= timeout {
                    return Waited::TimedOut { last: why };
                }
                on_wait(&why);
            }
        }
        // Sleep in small steps, so a cancel is seen at once.
        let until = Instant::now() + poll;
        while Instant::now() < until && !cancelled() {
            std::thread::sleep(poll.min(Duration::from_millis(50)));
        }
    }
}

/// This machine now: `/proc/meminfo`, and the daemon's own cgroup when it has limits.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn probe_system() -> Option<Snapshot> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let headroom = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| crate::memory_ceiling::cgroup_path(&text).map(str::to_string))
        .and_then(|path| {
            let dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
            let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
            scope_headroom(
                read("memory.high").as_deref(),
                read("memory.max").as_deref(),
                &read("memory.stat").unwrap_or_default(),
            )
        });
    snapshot(&meminfo, headroom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const MEMINFO: &str = "MemTotal:       65598464 kB\nMemFree:        12000000 kB\nMemAvailable:   34000000 kB\nSwapTotal: 16776188 kB\n";

    fn at(available_gib: u64, headroom_gib: Option<u64>) -> Snapshot {
        Snapshot {
            total: 64 * GIB,
            available: available_gib * GIB,
            scope_headroom: headroom_gib.map(|g| g * GIB),
        }
    }

    #[test]
    fn meminfo_and_the_scope_read_into_a_snapshot() {
        let s = snapshot(MEMINFO, Some(5)).unwrap();
        assert_eq!(
            (s.total, s.available, s.scope_headroom),
            (65_598_464 * 1024, 34_000_000 * 1024, Some(5))
        );
        assert_eq!(snapshot("MemTotal: 1 kB\n", None), None);
        let stat = "anon 4294967296\nfile 999999999999\n";
        assert_eq!(
            scope_headroom(Some("30923554816\n"), Some("34359738368\n"), stat),
            Some(30_923_554_816 - 4_294_967_296)
        );
        assert_eq!(
            scope_headroom(Some("max"), Some("34359738368"), stat),
            Some(34_359_738_368 - 4_294_967_296)
        );
        assert_eq!(scope_headroom(Some("max"), Some("max"), stat), None);
        assert_eq!(scope_headroom(None, None, ""), None);
        assert_eq!(scope_headroom(Some("100"), None, "anon 500\n"), Some(0));
    }

    #[test]
    fn a_stage_starts_only_with_its_need_and_the_reserve_free() {
        // Train needs 12 GiB; the reserve on 64 GiB is 6.4 + 2 GiB.
        assert_eq!(verdict(need(Phase::Train), &at(21, None)), Verdict::Admit);
        let Verdict::Wait(why) = verdict(need(Phase::Train), &at(20, None)) else {
            panic!("waits")
        };
        assert!(
            why.contains("20.0 GiB available; the stage needs 12.0 GiB and 8.4 GiB stays free"),
            "{why}"
        );
        assert_eq!(verdict(need(Phase::Samples), &at(13, None)), Verdict::Admit);
        let Verdict::Wait(why) = verdict(need(Phase::Train), &at(40, Some(11))) else {
            panic!("waits")
        };
        assert!(why.contains("scope has 11.0 GiB left"), "{why}");
        assert_eq!(
            verdict(need(Phase::Train), &at(40, Some(12))),
            Verdict::Admit
        );
    }

    #[test]
    fn a_stage_waits_until_memory_frees_up() {
        let calls = Cell::new(0);
        let probe = || {
            calls.set(calls.get() + 1);
            Some(at(if calls.get() < 3 { 10 } else { 30 }, None))
        };
        let mut said = Vec::new();
        let waited = wait_for(
            need(Phase::Train),
            &probe,
            &|| false,
            Duration::from_secs(5),
            Duration::from_millis(1),
            &mut |why| said.push(why.to_string()),
        );
        assert!(matches!(waited, Waited::Admitted { .. }), "{waited:?}");
        assert_eq!(said.len(), 2);
    }

    #[test]
    fn waiting_ends_in_a_timeout_a_cancel_or_an_unreadable_machine() {
        let starved = || Some(at(10, None));
        match wait_for(
            need(Phase::Train),
            &starved,
            &|| false,
            Duration::from_millis(20),
            Duration::from_millis(5),
            &mut |_| {},
        ) {
            Waited::TimedOut { last } => assert!(last.contains("10.0 GiB available"), "{last}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            wait_for(
                need(Phase::Train),
                &starved,
                &|| true,
                Duration::from_secs(5),
                Duration::from_millis(5),
                &mut |_| {}
            ),
            Waited::Cancelled
        );
        assert_eq!(
            wait_for(
                need(Phase::Train),
                &|| None,
                &|| false,
                Duration::from_secs(5),
                Duration::from_millis(5),
                &mut |_| {}
            ),
            Waited::Unknown
        );
    }
}
