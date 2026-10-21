//! The daemon brings the tray UI back when it vanished (`docs/runtime/daemon-and-tray.md#the-daemon-relaunches-the-tray`).
//!
//! The tray UI polls the daemon every second, marking its requests as the tray's. Once a tray has
//! been seen, a silence longer than [`ABSENT_AFTER`] means it is gone (crashed, killed, or lost with
//! its session bus), and the daemon starts it again, backing off between attempts. A user who quit
//! from the menu leaves a `ui.quit` marker: then the daemon never relaunches. A daemon that never
//! saw a tray (a headless `run`) never starts one.
//!
//! [`TrayWatchdog::step`] is pure: the daemon calls it with the time, the tray's last poll and the
//! quit marker, and acts on the decision.

use std::time::Duration;

/// A tray silent this long is gone (it polls every second).
pub const ABSENT_AFTER: Duration = Duration::from_secs(30);
/// How often the daemon checks.
pub const CHECK_EVERY: Duration = Duration::from_secs(5);
/// The wait after the first relaunch; it doubles with every attempt that brings no tray back.
pub const FIRST_BACKOFF: Duration = Duration::from_secs(30);
/// The longest wait between attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Milliseconds since the Unix epoch: the clock the tray's polls are noted on.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// What the daemon does after a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do: a tray polls, none was ever seen, or a relaunch is backing off.
    Nothing,
    /// The tray has gone: start it again. `attempt` counts from 1 since it was last seen.
    Relaunch { attempt: u32, absent_for: Duration },
    /// The tray has gone because the user quit from the menu: no relaunch (reported once).
    UserQuit,
}

/// The wait after `attempt` relaunches that brought no tray back.
pub fn backoff(attempt: u32) -> Duration {
    let doubled = FIRST_BACKOFF.saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    doubled.min(MAX_BACKOFF)
}

/// The watchdog's memory between checks. Times are milliseconds since any fixed origin.
#[derive(Debug, Default)]
pub struct TrayWatchdog {
    attempts: u32,
    next_allowed_ms: u64,
    reported_quit: bool,
}

impl TrayWatchdog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide at `now_ms`, given the tray's last poll (`None` when no tray was ever seen) and
    /// whether the user's quit marker is present.
    pub fn step(&mut self, now_ms: u64, last_seen_ms: Option<u64>, user_quit: bool) -> Decision {
        let Some(last_seen) = last_seen_ms else {
            return Decision::Nothing;
        };
        let absent_for = Duration::from_millis(now_ms.saturating_sub(last_seen));
        if absent_for < ABSENT_AFTER {
            // A tray polls: forget earlier attempts and any quit.
            self.attempts = 0;
            self.next_allowed_ms = 0;
            self.reported_quit = false;
            return Decision::Nothing;
        }
        if user_quit {
            if self.reported_quit {
                return Decision::Nothing;
            }
            self.reported_quit = true;
            return Decision::UserQuit;
        }
        if now_ms < self.next_allowed_ms {
            return Decision::Nothing;
        }
        self.attempts += 1;
        self.next_allowed_ms = now_ms + backoff(self.attempts).as_millis() as u64;
        Decision::Relaunch {
            attempt: self.attempts,
            absent_for,
        }
    }
}

/// `<config dir>/ui.quit`: the tray UI leaves it when the user quits from its menu, and removes
/// it when it starts.
pub fn quit_marker_path(config_path: &std::path::Path) -> std::path::PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("ui.quit")
}

#[cfg(feature = "ui")]
const TRACE_TARGET: &str = "studio_worker::tray_watchdog";

/// Watch the tray UI from the daemon until `control.stop`, relaunching it when it vanished.
#[cfg(feature = "ui")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn spawn(control: crate::control::DaemonControl) {
    let spawned = std::thread::Builder::new()
        .name("tray-watchdog".into())
        .spawn(move || {
            let mut watchdog = TrayWatchdog::new();
            let quit_marker = quit_marker_path(&control.config_path);
            while !control.stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(CHECK_EVERY);
                let decision =
                    watchdog.step(now_ms(), control.tray_last_seen(), quit_marker.exists());
                match decision {
                    Decision::Nothing => {}
                    Decision::UserQuit => tracing::info!(
                        target: TRACE_TARGET,
                        op = "tray_relaunch",
                        marker = %quit_marker.display(),
                        "the tray UI is gone because the user quit it; not relaunching"
                    ),
                    Decision::Relaunch {
                        attempt,
                        absent_for,
                    } => relaunch(&crate::setup::RealOps, &control, attempt, absent_for),
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!(
            target: TRACE_TARGET,
            op = "tray_relaunch",
            error = %e,
            "could not start the tray watchdog; a vanished tray UI comes back only at the next login"
        );
    }
}

/// Start the tray UI again, detached, logging the attempt and its outcome.
#[cfg(feature = "ui")]
pub fn relaunch(
    ops: &dyn crate::setup::SetupOps,
    control: &crate::control::DaemonControl,
    attempt: u32,
    absent_for: Duration,
) {
    let config = control.config_path.to_string_lossy().into_owned();
    let args = crate::setup::ui_args(Some(&config));
    let log = crate::daemon_link::ui_log_path(&control.config_path);
    tracing::warn!(
        target: TRACE_TARGET,
        op = "tray_relaunch",
        attempt,
        absent_secs = absent_for.as_secs(),
        next_wait_secs = backoff(attempt).as_secs(),
        "the tray UI has not polled the daemon; starting it again"
    );
    let started = ops.exe().and_then(|exe| {
        // A child of the daemon: inside the daemon's scope (and its memory ceiling) already.
        ops.start_ui(&crate::memory_ceiling::tray_argv(None, &exe, &args), &log)
            .map(|pid| (exe, pid))
            .map_err(anyhow::Error::from)
    });
    match started {
        Ok((exe, pid)) => tracing::info!(
            target: TRACE_TARGET,
            op = "tray_relaunch",
            attempt,
            pid,
            exe = %exe.display(),
            log = %log.display(),
            "started the tray UI again"
        ),
        Err(e) => tracing::error!(
            target: TRACE_TARGET,
            op = "tray_relaunch",
            attempt,
            error = %e,
            "could not start the tray UI again; retrying after the backoff"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000;

    #[test]
    fn never_starts_a_tray_it_never_saw() {
        let mut watchdog = TrayWatchdog::new();
        assert_eq!(watchdog.step(10_000 * S, None, false), Decision::Nothing);
    }

    #[test]
    fn leaves_a_polling_tray_alone() {
        let mut watchdog = TrayWatchdog::new();
        assert_eq!(
            watchdog.step(100 * S, Some(99 * S), false),
            Decision::Nothing
        );
        assert_eq!(
            watchdog.step(129 * S, Some(100 * S), false),
            Decision::Nothing
        );
    }

    #[test]
    fn relaunches_a_tray_silent_past_the_threshold() {
        let mut watchdog = TrayWatchdog::new();
        assert_eq!(
            watchdog.step(131 * S, Some(100 * S), false),
            Decision::Relaunch {
                attempt: 1,
                absent_for: Duration::from_secs(31)
            }
        );
    }

    #[test]
    fn backs_off_between_attempts_that_bring_no_tray_back() {
        let mut watchdog = TrayWatchdog::new();
        let seen = Some(0);
        assert!(matches!(
            watchdog.step(31 * S, seen, false),
            Decision::Relaunch { attempt: 1, .. }
        ));
        // 30 s must pass before the second attempt.
        assert_eq!(watchdog.step(60 * S, seen, false), Decision::Nothing);
        assert!(matches!(
            watchdog.step(61 * S, seen, false),
            Decision::Relaunch { attempt: 2, .. }
        ));
        // Then 60 s.
        assert_eq!(watchdog.step(120 * S, seen, false), Decision::Nothing);
        assert!(matches!(
            watchdog.step(121 * S, seen, false),
            Decision::Relaunch { attempt: 3, .. }
        ));
    }

    #[test]
    fn starts_over_once_the_tray_polls_again() {
        let mut watchdog = TrayWatchdog::new();
        assert!(matches!(
            watchdog.step(31 * S, Some(0), false),
            Decision::Relaunch { attempt: 1, .. }
        ));
        assert_eq!(
            watchdog.step(40 * S, Some(39 * S), false),
            Decision::Nothing
        );
        assert!(matches!(
            watchdog.step(80 * S, Some(39 * S), false),
            Decision::Relaunch { attempt: 1, .. }
        ));
    }

    #[test]
    fn never_relaunches_after_the_user_quit_and_says_so_once() {
        let mut watchdog = TrayWatchdog::new();
        assert_eq!(watchdog.step(31 * S, Some(0), true), Decision::UserQuit);
        assert_eq!(watchdog.step(3_600 * S, Some(0), true), Decision::Nothing);
    }

    #[test]
    fn backoff_doubles_up_to_its_cap() {
        assert_eq!(backoff(1), Duration::from_secs(30));
        assert_eq!(backoff(2), Duration::from_secs(60));
        assert_eq!(backoff(3), Duration::from_secs(120));
        assert_eq!(backoff(10), MAX_BACKOFF);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
    }
}

#[cfg(all(test, feature = "ui"))]
mod relaunch_tests {
    use super::*;
    use crate::test_support::capture;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    type Started = Arc<Mutex<Vec<(Vec<OsString>, PathBuf)>>>;

    struct FakeOps {
        started: Started,
        fails: bool,
    }

    impl crate::setup::SetupOps for FakeOps {
        fn exe(&self) -> anyhow::Result<PathBuf> {
            Ok(PathBuf::from("/opt/sw/studio-worker"))
        }
        fn sync_login_entry(
            &self,
            _exe: &Path,
            _enabled: bool,
            _ceiling: Option<crate::memory_ceiling::MemoryCeiling>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn launch_ceiling(
            &self,
            _ceiling: crate::memory_ceiling::MemoryCeiling,
        ) -> Option<crate::memory_ceiling::MemoryCeiling> {
            unreachable!("the watchdog starts the tray inside the daemon's scope")
        }
        fn scope_active(&self) -> bool {
            unreachable!("the watchdog starts the tray inside the daemon's scope")
        }
        fn start_ui(&self, argv: &[OsString], log: &Path) -> std::io::Result<u32> {
            if self.fails {
                return Err(std::io::Error::other("no display"));
            }
            self.started
                .lock()
                .unwrap()
                .push((argv.to_vec(), log.to_path_buf()));
            Ok(4242)
        }
    }

    fn control_at(dir: &Path) -> crate::control::DaemonControl {
        crate::control::DaemonControl::new(
            crate::config::shared(crate::config::Config::default()),
            dir.join("config.toml"),
            0.0,
        )
    }

    #[test]
    fn starts_the_tray_for_this_config_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let control = control_at(dir.path());
        let started: Started = Arc::default();
        let ops = FakeOps {
            started: started.clone(),
            fails: false,
        };
        let logs = capture(move || relaunch(&ops, &control, 2, Duration::from_secs(45)));

        let started = started.lock().unwrap();
        let (argv, log) = &started[0];
        let config = dir
            .path()
            .join("config.toml")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            argv,
            &[
                OsString::from("/opt/sw/studio-worker"),
                "--config".into(),
                config.into(),
                "ui".into()
            ]
        );
        assert_eq!(log, &dir.path().join("ui.log"));
        assert!(logs.contains("op=\"tray_relaunch\""), "{logs}");
        assert!(logs.contains("attempt=2"), "{logs}");
        assert!(logs.contains("absent_secs=45"), "{logs}");
        assert!(logs.contains("started the tray UI again"), "{logs}");
        assert!(logs.contains("pid=4242"), "{logs}");
    }

    #[test]
    fn a_failed_start_is_an_error_and_waits_for_the_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let control = control_at(dir.path());
        let ops = FakeOps {
            started: Arc::default(),
            fails: true,
        };
        let logs = capture(move || relaunch(&ops, &control, 1, Duration::from_secs(31)));

        assert!(logs.contains("ERROR"), "{logs}");
        assert!(logs.contains("could not start the tray UI again"), "{logs}");
        assert!(logs.contains("no display"), "{logs}");
    }

    #[test]
    fn the_quit_marker_sits_beside_the_config() {
        assert_eq!(
            quit_marker_path(Path::new("/home/a/.config/sw/config.toml")),
            PathBuf::from("/home/a/.config/sw/ui.quit")
        );
    }
}
