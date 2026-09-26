//! The tray UI: an egui window + system tray that is a client of the
//! daemon (`studio-worker run`).  See `docs/runtime/daemon-and-tray.md`.
//!
//! The UI never runs a job: a poller mirrors the daemon's state into a
//! [`Replica`] the pages render, and the operator's actions go back over the
//! local API.  When no daemon runs, the poller starts one.  One tray UI runs
//! per config directory (`single_instance`).
//!
//! Gated behind the `ui` cargo feature so headless installs and the
//! service path don't pull in egui / eframe / the tray backends.

pub mod actions;
pub mod app;
pub mod chrome;
pub mod format;
pub mod icons;
pub mod log_view;
pub mod notifier;
pub mod page;
pub mod pages;
pub mod prefs;
pub mod pulse;
pub mod single_instance;
pub mod theme;
pub mod tray;
pub mod tray_host;
pub mod widgets;

use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;

use crate::{
    config,
    daemon_link::{Action, Poller, ProcessStarter, Replica},
};

const TRACE_TARGET: &str = "studio_worker::ui";

/// Carries the display-retry attempt across the restart in place.
pub const DISPLAY_ATTEMPT_ENV: &str = "STUDIO_WORKER_UI_DISPLAY_ATTEMPT";

/// First wait before retrying the display, doubled per attempt.
pub const DISPLAY_RETRY_BASE: Duration = Duration::from_secs(2);
/// Longest wait between display attempts.
pub const DISPLAY_RETRY_MAX: Duration = Duration::from_secs(60);

/// How long to wait before display attempt `attempt + 1`.
pub fn display_retry_delay(attempt: u32) -> Duration {
    DISPLAY_RETRY_BASE
        .saturating_mul(2u32.saturating_pow(attempt.min(16)))
        .min(DISPLAY_RETRY_MAX)
}

/// The display attempt this process is, from [`DISPLAY_ATTEMPT_ENV`].
pub fn display_attempt(env_value: Option<&str>) -> u32 {
    env_value.and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Log a failed display attempt and answer how long to wait.
pub fn log_display_wait(attempt: u32, error: &str) -> Duration {
    let delay = display_retry_delay(attempt);
    tracing::warn!(
        target: TRACE_TARGET,
        op = "display_wait",
        attempt = attempt + 1,
        retry_in_secs = delay.as_secs(),
        error = %error,
        "no usable display yet; the tray UI will retry"
    );
    delay
}

/// Entry point for `studio-worker ui`.
pub fn run(config_path: Option<&str>) -> Result<()> {
    let path = config::resolve_path(config_path)?;
    let attempt = display_attempt(std::env::var(DISPLAY_ATTEMPT_ENV).ok().as_deref());
    tracing::info!(
        target: TRACE_TARGET,
        op = "startup",
        config_path = %path.display(),
        display_attempt = attempt,
        "tray UI starting as a client of the daemon"
    );
    let _ui_lock = match take_ui_lock(&path, attempt) {
        UiLockOutcome::Held(lock) => lock,
        UiLockOutcome::HandedOver => return Ok(()),
    };
    ensure_autostart();

    // The poller runs whether or not the window can open: it starts the
    // daemon when none runs, even while the UI waits for a display.
    let replica = Replica::default();
    let stop = Arc::new(AtomicBool::new(false));
    let repaint: Arc<Mutex<Option<eframe::egui::Context>>> = Arc::default();
    let exe = std::env::current_exe()?;
    let poller = Poller::new(
        replica.clone(),
        path.clone(),
        Box::new(ProcessStarter {
            exe,
            config_path: path.clone(),
        }),
    );
    std::thread::spawn({
        let stop = stop.clone();
        let repaint = repaint.clone();
        move || {
            poller.run(stop, || {
                if let Some(ctx) = repaint.lock().as_ref() {
                    ctx.request_repaint();
                }
            })
        }
    });

    std::thread::spawn({
        let stop = stop.clone();
        let repaint = repaint.clone();
        let path = path.clone();
        move || {
            single_instance::watch(path, stop, || match repaint.lock().as_ref() {
                Some(ctx) => {
                    raise_window(ctx);
                    true
                }
                None => false,
            })
        }
    });

    let actions = actions::ActionRunner::new(path.clone(), replica.clone());
    let deps = app::AppDeps {
        replica: replica.clone(),
        start_minimised: config::peek(&path).start_minimised,
        actions: actions.clone(),
        config_path: path,
        tokio: tokio::runtime::Handle::current(),
    };

    // Start-minimised is requested by the App on its first frame via
    // `ViewportCommand::Minimized` — egui 0.34's ViewportBuilder has
    // no `with_minimized`.
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1240.0, 820.0])
        .with_min_inner_size([960.0, 600.0])
        .with_title("studio-worker");
    // In development, open on the left monitor instead of the
    // primary screen.  Override with STUDIO_WORKER_WINDOW_POS="x,y".
    if let Some([x, y]) =
        dev_window_position(std::env::var("STUDIO_WORKER_WINDOW_POS").ok().as_deref())
    {
        viewport = viewport.with_position([x, y]);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let initial_paused = replica.paused.load(std::sync::atomic::Ordering::SeqCst);
    // The Linux (ksni) tray backend runs on the tokio runtime.
    let tokio_for_tray = tokio::runtime::Handle::current();
    let set_paused: tray_host::SetPaused = {
        let actions = actions.clone();
        Arc::new(move |paused| actions.run(Action::SetPaused(paused)))
    };

    let app_theme = prefs::load(&prefs::path_for(&deps.config_path)).theme;
    let outcome = eframe::run_native(
        "studio-worker",
        native_options,
        Box::new(move |cc| {
            // Dark by default (project design rule); the operator's choice
            // from ui.toml otherwise.
            theme::apply(&cc.egui_ctx, app_theme);
            *repaint.lock() = Some(cc.egui_ctx.clone());
            actions.attach(cc.egui_ctx.clone());
            let mut app = app::App::with_notifier(deps, app::App::default_notifier_box());
            // Best-effort tray: the window works without one.
            if let Some(tray) = tray_host::install(
                cc.egui_ctx.clone(),
                replica.paused.clone(),
                set_paused,
                app.quit_requested_handle(),
                tokio_for_tray,
                initial_paused,
            ) {
                app.attach_tray(tray);
            }
            Ok(Box::new(app))
        }),
    );
    match outcome {
        Ok(()) => {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        Err(err) => {
            let delay = log_display_wait(attempt, &err.to_string());
            std::thread::sleep(delay);
            restart_for_display(attempt + 1)
        }
    }
}

/// What [`take_ui_lock`] decided.
enum UiLockOutcome {
    /// This process is the tray UI; `None` when the lock could not be
    /// opened and the UI runs without the guard.
    Held(Option<single_instance::UiLock>),
    /// Another tray UI runs for this config and was asked to show itself.
    HandedOver,
}

/// Take the UI lock, or hand over to the tray UI that holds it.  A UI
/// restarting itself for the display waits for its predecessor's lock.
fn take_ui_lock(path: &std::path::Path, display_attempt: u32) -> UiLockOutcome {
    let (attempts, pause) = if display_attempt > 0 {
        (
            single_instance::RESTART_ATTEMPTS,
            single_instance::RESTART_PAUSE,
        )
    } else {
        (1, Duration::ZERO)
    };
    match single_instance::acquire(path, attempts, pause) {
        Ok(single_instance::Instance::Primary(lock)) => UiLockOutcome::Held(Some(lock)),
        Ok(single_instance::Instance::Secondary) => {
            // Best-effort: the failure is logged, and exiting is right either
            // way (the running UI already has a tray icon).
            let _ = single_instance::hand_over(path);
            UiLockOutcome::HandedOver
        }
        Err(e) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "single_instance",
                error = %e,
                "could not open the ui lock; running without the single-instance guard"
            );
            UiLockOutcome::Held(None)
        }
    }
}

/// Show, un-minimise and focus the window (from any thread).
pub fn raise_window(ctx: &eframe::egui::Context) {
    use eframe::egui::ViewportCommand;
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
    ctx.request_repaint();
}

/// Start this UI again in place with the next display attempt.  The
/// windowing library allows one event loop per process and caches a
/// failed display connection, so a retry needs a fresh process.
#[cfg_attr(coverage_nightly, coverage(off))]
fn restart_for_display(attempt: u32) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env(DISPLAY_ATTEMPT_ENV, attempt.to_string());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let err = cmd.exec();
        Err(anyhow!(
            "restarting the tray UI for the display failed: {err}"
        ))
    }
    #[cfg(not(unix))]
    {
        cmd.spawn()
            .map_err(|e| anyhow!("restarting the tray UI for the display failed: {e}"))?;
        std::process::exit(0);
    }
}

/// Keep the tray UI's login entry installed and pointing at this
/// executable.  Best-effort: a failure is logged, never fatal.
fn ensure_autostart() {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = crate::autostart::ensure(&exe) {
                tracing::warn!(
                    target: "studio_worker::ui",
                    op = "autostart",
                    error = %e,
                    "could not install the login entry for the tray UI"
                );
            }
        }
        Err(e) => tracing::warn!(
            target: "studio_worker::ui",
            op = "autostart",
            error = %e,
            "could not resolve the current executable for the login entry"
        ),
    }
}

/// Decide where to place the window on launch.
///
/// - An explicit `STUDIO_WORKER_WINDOW_POS="x,y"` always wins (any build).
/// - Otherwise, debug builds default to the left monitor's top-left so
///   the window opens on the left screen during development.
/// - Release builds return `None`, letting the window manager decide.
fn dev_window_position(env: Option<&str>) -> Option<[f32; 2]> {
    if let Some(raw) = env {
        let mut parts = raw.split(',').map(str::trim);
        if let (Some(x), Some(y), None) = (parts.next(), parts.next(), parts.next()) {
            if let (Ok(x), Ok(y)) = (x.parse::<f32>(), y.parse::<f32>()) {
                return Some([x, y]);
            }
        }
        return None;
    }
    // The left monitor sits at the X11 root origin; a small inset keeps
    // the title bar clear of the screen edge.  Release builds defer to
    // the window manager.
    #[cfg(debug_assertions)]
    let default = Some([48.0, 48.0]);
    #[cfg(not(debug_assertions))]
    let default = None;
    default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_retry_doubles_up_to_a_minute() {
        assert_eq!(display_retry_delay(0), Duration::from_secs(2));
        assert_eq!(display_retry_delay(1), Duration::from_secs(4));
        assert_eq!(display_retry_delay(4), Duration::from_secs(32));
        assert_eq!(display_retry_delay(5), DISPLAY_RETRY_MAX);
        assert_eq!(display_retry_delay(u32::MAX), DISPLAY_RETRY_MAX);
    }

    #[test]
    fn the_display_attempt_comes_from_the_environment() {
        assert_eq!(display_attempt(None), 0);
        assert_eq!(display_attempt(Some("3")), 3);
        assert_eq!(display_attempt(Some("junk")), 0);
    }

    #[test]
    fn a_display_wait_is_logged_with_its_attempt() {
        let logs = crate::test_support::capture(|| {
            let delay = log_display_wait(1, "Invalid MIT-MAGIC-COOKIE-1 key");
            assert_eq!(delay, Duration::from_secs(4));
        });
        assert!(logs.contains("op=\"display_wait\""), "{logs}");
        assert!(logs.contains("attempt=2"), "{logs}");
        assert!(logs.contains("retry_in_secs=4"), "{logs}");
        assert!(logs.contains("MIT-MAGIC-COOKIE"), "{logs}");
    }

    #[test]
    fn a_second_ui_hands_over_to_the_first_and_leaves_a_raise_request() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let first = take_ui_lock(&config, 0);
        assert!(matches!(first, UiLockOutcome::Held(Some(_))));
        assert!(matches!(
            take_ui_lock(&config, 0),
            UiLockOutcome::HandedOver
        ));
        assert!(single_instance::take_raise_request(&config));
    }

    #[test]
    fn a_ui_restarting_for_the_display_waits_for_its_predecessors_lock() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let predecessor = take_ui_lock(&config, 0);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(predecessor);
        });
        assert!(matches!(
            take_ui_lock(&config, 1),
            UiLockOutcome::Held(Some(_))
        ));
        release.join().unwrap();
    }

    #[test]
    fn an_unopenable_ui_lock_runs_without_the_guard_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the config directory should be: the lock cannot open.
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        let config = blocker.join("config.toml");
        let logs = crate::test_support::capture(move || {
            assert!(matches!(
                take_ui_lock(&config, 0),
                UiLockOutcome::Held(None)
            ));
        });
        assert!(logs.contains("without the single-instance guard"), "{logs}");
    }

    #[test]
    fn parses_explicit_position_override() {
        assert_eq!(dev_window_position(Some("100,200")), Some([100.0, 200.0]));
    }

    #[test]
    fn trims_whitespace_around_coords() {
        assert_eq!(dev_window_position(Some(" 10 , 20 ")), Some([10.0, 20.0]));
    }

    #[test]
    fn rejects_malformed_override() {
        assert_eq!(dev_window_position(Some("not-a-pos")), None);
        assert_eq!(dev_window_position(Some("1,2,3")), None);
        assert_eq!(dev_window_position(Some("1")), None);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn defaults_to_left_screen_in_debug() {
        assert_eq!(dev_window_position(None), Some([48.0, 48.0]));
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn defers_to_wm_in_release() {
        assert_eq!(dev_window_position(None), None);
    }
}
