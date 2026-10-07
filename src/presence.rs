//! User presence: whether the person is using this computer, read from the
//! operating system's input idle time.
//!
//! The daemon samples the idle time every [`SAMPLE_INTERVAL`] and keeps the
//! result in a [`PresenceSlot`]; the studio session reports it on every
//! heartbeat (`userPresence`) and, with the operator's `only_when_idle` on,
//! turns offers down while the person is active.
//!
//! Where the idle time comes from:
//!
//! - Linux, X11: the ScreenSaver extension (`x11rb`, pure Rust).
//! - Linux, Wayland (or X11 without the extension): the compositor's D-Bus idle
//!   monitor: GNOME's `org.gnome.Mutter.IdleMonitor`, then
//!   `org.freedesktop.ScreenSaver.GetSessionIdleTime` (KDE Plasma).
//! - Windows: `GetLastInputInfo` (through `system-idle-time`).
//! - macOS: `HIDIdleTime` of `IOHIDSystem`, read with `ioreg`.
//!
//! A probe that fails leaves the presence unknown (`None`); the studio treats
//! unknown like active for an `only_when_idle` worker.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const TRACE_TARGET: &str = "studio_worker::presence";

/// Input idle for this long means the person is away.
pub const IDLE_AFTER: Duration = Duration::from_secs(120);

/// How often the daemon samples the idle time.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Whether the person is using this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserPresence {
    Idle,
    Active,
}

impl UserPresence {
    pub fn as_str(self) -> &'static str {
        match self {
            UserPresence::Idle => "idle",
            UserPresence::Active => "active",
        }
    }
}

/// The latest presence; `None` while unknown.
pub type PresenceSlot = Arc<Mutex<Option<UserPresence>>>;

/// The presence an input idle time of `idle` means.
pub fn classify(idle: Duration) -> UserPresence {
    if idle >= IDLE_AFTER {
        UserPresence::Idle
    } else {
        UserPresence::Active
    }
}

/// Whether the worker holds back new work: the operator asked to use this
/// computer only while the person is away, and they are not known to be.
pub fn holds_back(only_when_idle: bool, presence: Option<UserPresence>) -> bool {
    only_when_idle && presence != Some(UserPresence::Idle)
}

/// Why the idle time could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    #[error("no idle-time source on this platform")]
    Unsupported,
    #[error("{source_name}: {detail}")]
    Failed {
        source_name: &'static str,
        detail: String,
    },
}

impl ProbeError {
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn failed(source_name: &'static str, detail: impl ToString) -> Self {
        ProbeError::Failed {
            source_name,
            detail: detail.to_string(),
        }
    }
}

/// What the monitor remembers between samples, so it logs changes only.
#[derive(Debug, Default)]
pub struct Tracker {
    last_error: Option<ProbeError>,
}

impl Tracker {
    /// Apply one sample to `slot`, logging a change of presence, a probe that
    /// starts failing (or fails differently) and a probe that recovers.
    pub fn record(&mut self, slot: &PresenceSlot, sample: Result<Duration, ProbeError>) {
        match sample {
            Ok(idle) => {
                if let Some(error) = self.last_error.take() {
                    tracing::info!(
                        target: TRACE_TARGET,
                        op = "presence",
                        previous_error = %error,
                        "user presence readable again"
                    );
                }
                let now = classify(idle);
                let before = slot.lock().replace(now);
                if before != Some(now) {
                    tracing::info!(
                        target: TRACE_TARGET,
                        op = "presence",
                        presence = now.as_str(),
                        idle_secs = idle.as_secs(),
                        "user presence changed"
                    );
                }
            }
            Err(error) => {
                let before = slot.lock().take();
                if self.last_error.as_ref() != Some(&error) {
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "presence",
                        error = %error,
                        was = before.map(UserPresence::as_str),
                        "user presence unknown: could not read the input idle time"
                    );
                    self.last_error = Some(error);
                }
            }
        }
    }
}

/// Sample the presence into `slot` until `stop` is set.
pub fn spawn_monitor(slot: PresenceSlot, stop: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracker = Tracker::default();
        let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !stop.load(Ordering::SeqCst) {
            tick.tick().await;
            tracker.record(&slot, idle_time().await);
        }
    })
}

/// The `HIDIdleTime` (nanoseconds) in `ioreg -c IOHIDSystem` output.
pub fn parse_ioreg_idle(output: &str) -> Option<Duration> {
    output.lines().find_map(|line| {
        let (_, value) = line.split_once("\"HIDIdleTime\" = ")?;
        value.trim().parse::<u64>().ok().map(Duration::from_nanos)
    })
}

/// The input idle time of the person at this computer.
#[cfg_attr(coverage_nightly, coverage(off))]
pub async fn idle_time() -> Result<Duration, ProbeError> {
    platform::idle_time().await
}

#[cfg(target_os = "linux")]
mod platform {
    use super::ProbeError;
    use std::time::Duration;

    pub async fn idle_time() -> Result<Duration, ProbeError> {
        let wayland = std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland");
        let x11 = std::env::var_os("DISPLAY").is_some();
        if x11 && !wayland {
            let x11_result = tokio::task::spawn_blocking(x11_idle)
                .await
                .map_err(|e| ProbeError::failed("x11", e))?;
            match x11_result {
                Ok(idle) => return Ok(idle),
                Err(x11_error) => {
                    return dbus_idle().await.map_err(|dbus_error| {
                        ProbeError::failed("x11, d-bus", format!("{x11_error}; {dbus_error}"))
                    })
                }
            }
        }
        dbus_idle().await
    }

    fn x11_idle() -> Result<Duration, ProbeError> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::screensaver::ConnectionExt as _;
        let fail = |e: &dyn std::fmt::Display| ProbeError::failed("x11", e);
        let (conn, screen) =
            x11rb::rust_connection::RustConnection::connect(None).map_err(|e| fail(&e))?;
        let root = conn.setup().roots[screen].root;
        conn.screensaver_query_version(1, 0)
            .map_err(|e| fail(&e))?
            .reply()
            .map_err(|e| fail(&e))?;
        let info = conn
            .screensaver_query_info(root)
            .map_err(|e| fail(&e))?
            .reply()
            .map_err(|e| fail(&e))?;
        Ok(Duration::from_millis(info.ms_since_user_input.into()))
    }

    async fn dbus_idle() -> Result<Duration, ProbeError> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| ProbeError::failed("d-bus", e))?;
        let mutter = conn
            .call_method(
                Some("org.gnome.Mutter.IdleMonitor"),
                "/org/gnome/Mutter/IdleMonitor/Core",
                Some("org.gnome.Mutter.IdleMonitor"),
                "GetIdletime",
                &(),
            )
            .await
            .and_then(|reply| reply.body().deserialize::<u64>());
        let mutter_error = match mutter {
            Ok(ms) => return Ok(Duration::from_millis(ms)),
            Err(e) => e,
        };
        conn.call_method(
            Some("org.freedesktop.ScreenSaver"),
            "/org/freedesktop/ScreenSaver",
            Some("org.freedesktop.ScreenSaver"),
            "GetSessionIdleTime",
            &(),
        )
        .await
        .and_then(|reply| reply.body().deserialize::<u32>())
        .map(|ms| Duration::from_millis(ms.into()))
        .map_err(|e| {
            ProbeError::failed(
                "d-bus",
                format!("mutter idle monitor: {mutter_error}; freedesktop screensaver: {e}"),
            )
        })
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::ProbeError;
    use std::time::Duration;

    pub async fn idle_time() -> Result<Duration, ProbeError> {
        tokio::task::spawn_blocking(|| {
            system_idle_time::get_idle_time().map_err(|e| ProbeError::Failed {
                source_name: "GetLastInputInfo",
                detail: e.to_string(),
            })
        })
        .await
        .map_err(|e| ProbeError::Failed {
            source_name: "GetLastInputInfo",
            detail: e.to_string(),
        })?
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::ProbeError;
    use std::time::Duration;

    pub async fn idle_time() -> Result<Duration, ProbeError> {
        tokio::task::spawn_blocking(|| {
            let out = std::process::Command::new("ioreg")
                .args(["-c", "IOHIDSystem", "-r", "-d", "1", "-k", "HIDIdleTime"])
                .output()
                .map_err(|e| ProbeError::failed("ioreg", e))?;
            if !out.status.success() {
                return Err(ProbeError::failed("ioreg", out.status));
            }
            super::parse_ioreg_idle(&String::from_utf8_lossy(&out.stdout))
                .ok_or_else(|| ProbeError::failed("ioreg", "no HIDIdleTime in its output"))
        })
        .await
        .map_err(|e| ProbeError::failed("ioreg", e))?
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod platform {
    use super::ProbeError;
    use std::time::Duration;

    pub async fn idle_time() -> Result<Duration, ProbeError> {
        Err(ProbeError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;

    #[test]
    fn two_minutes_without_input_is_idle() {
        assert_eq!(classify(Duration::ZERO), UserPresence::Active);
        assert_eq!(classify(Duration::from_secs(119)), UserPresence::Active);
        assert_eq!(classify(IDLE_AFTER), UserPresence::Idle);
        assert_eq!(classify(Duration::from_secs(3600)), UserPresence::Idle);
    }

    #[test]
    fn only_when_idle_holds_back_unless_the_person_is_known_to_be_away() {
        assert!(!holds_back(false, Some(UserPresence::Active)));
        assert!(!holds_back(false, None));
        assert!(holds_back(true, Some(UserPresence::Active)));
        assert!(holds_back(true, None));
        assert!(!holds_back(true, Some(UserPresence::Idle)));
    }

    #[test]
    fn presence_is_lowercase_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&UserPresence::Idle).unwrap(),
            "\"idle\""
        );
        assert_eq!(
            serde_json::from_str::<UserPresence>("\"active\"").unwrap(),
            UserPresence::Active
        );
    }

    #[test]
    fn the_tracker_logs_changes_only() {
        let slot = PresenceSlot::default();
        let in_capture = slot.clone();
        let logs = capture(move || {
            let mut tracker = Tracker::default();
            tracker.record(&in_capture, Ok(Duration::from_secs(1)));
            tracker.record(&in_capture, Ok(Duration::from_secs(2)));
            tracker.record(&in_capture, Ok(Duration::from_secs(300)));
        });
        assert_eq!(*slot.lock(), Some(UserPresence::Idle));
        assert_eq!(logs.matches("user presence changed").count(), 2, "{logs}");
        assert!(logs.contains("presence=\"active\""), "{logs}");
        assert!(logs.contains("presence=\"idle\""), "{logs}");
    }

    #[test]
    fn a_failing_probe_makes_presence_unknown_and_warns_once_per_error() {
        let slot: PresenceSlot = Arc::new(Mutex::new(Some(UserPresence::Idle)));
        let in_capture = slot.clone();
        let logs = capture(move || {
            let mut tracker = Tracker::default();
            let gone = || ProbeError::failed("x11", "no display");
            tracker.record(&in_capture, Err(gone()));
            tracker.record(&in_capture, Err(gone()));
            tracker.record(&in_capture, Err(ProbeError::Unsupported));
            tracker.record(&in_capture, Ok(Duration::from_secs(1)));
        });
        assert_eq!(*slot.lock(), Some(UserPresence::Active));
        assert_eq!(logs.matches("user presence unknown").count(), 2, "{logs}");
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("x11: no display"), "{logs}");
        assert!(logs.contains("user presence readable again"), "{logs}");
    }

    #[test]
    fn a_failing_probe_clears_a_known_presence() {
        let slot: PresenceSlot = Arc::new(Mutex::new(Some(UserPresence::Idle)));
        Tracker::default().record(&slot, Err(ProbeError::Unsupported));
        assert_eq!(*slot.lock(), None);
    }

    #[test]
    fn ioreg_output_yields_the_hid_idle_time() {
        let out = "+-o IOHIDSystem  <class IOHIDSystem>\n    {\n      \"HIDIdleTime\" = 2500000000\n    }\n";
        assert_eq!(parse_ioreg_idle(out), Some(Duration::from_millis(2500)));
        assert_eq!(parse_ioreg_idle("nothing here"), None);
        assert_eq!(parse_ioreg_idle("\"HIDIdleTime\" = soon"), None);
    }

    #[tokio::test]
    async fn the_monitor_stops_when_asked() {
        let stop = Arc::new(AtomicBool::new(true));
        let handle = spawn_monitor(PresenceSlot::default(), stop);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("monitor stops")
            .unwrap();
    }
}
