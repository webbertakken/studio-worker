//! Native egui desktop UI.  See `plans/native-ui.md` for the full
//! design (tabs, tray icon, notifications, autostart).
//!
//! This module is gated behind the `ui` cargo feature so headless
//! installs and the systemd / launchd service path don't pull in
//! egui / eframe / tray-icon / notify-rust + their system libs.

pub mod app;
pub mod notifier;
pub mod tab;
pub mod tabs;
pub mod tray;
pub mod tray_host;

use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;

use crate::{
    auto_register::{self, RegistrationState},
    config,
    runtime::{self, LoopSchedule, WorkerObservers},
    types::LogEntry,
};

/// Entry point for `studio-worker ui`.  Loads config, spawns the four
/// background loops on the calling tokio runtime, then hands the main
/// thread to eframe (which it owns for the lifetime of the window).
pub fn run(config_path: Option<&str>) -> Result<()> {
    let (cfg, path) = config::load(config_path)?;
    runtime::log_startup_banner(&cfg, &path);

    // Honour `auto_start`: make the tray UI come back on login without
    // the operator having to toggle anything.  Best-effort and
    // idempotent — a failure is logged, never fatal.
    sync_autostart_on_launch(cfg.auto_start);

    let cfg = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));
    // Operator pause toggle.  Runtime-only: never persisted so the
    // worker comes up unpaused on every launch.
    let paused = Arc::new(AtomicBool::new(false));
    let logs: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let observers = WorkerObservers::default();

    let registration = auto_register::shared_initial();

    // Spawn the loops on the tokio runtime that's already driving
    // `run_cli` (multi-threaded — main.rs builds `Runtime::new()`).
    // `eframe::run_native` blocks the main thread; the loops keep
    // ticking on worker threads.
    let handle = tokio::runtime::Handle::current();

    // Auto-register loop: polls every 30s until Approved or Rejected.
    // Then the WS session takes over.
    let cfg_autoreg = cfg.clone();
    let path_autoreg = path.clone();
    let registration_autoreg = registration.clone();
    let stop_autoreg = stop.clone();
    handle.spawn(async move {
        loop {
            if stop_autoreg.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let state =
                auto_register::tick(&cfg_autoreg, &path_autoreg, &registration_autoreg).await;
            if matches!(
                state,
                RegistrationState::Approved | RegistrationState::Rejected { .. }
            ) {
                return;
            }
            for _ in 0..30 {
                if stop_autoreg.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });

    let cfg_loops = cfg.clone();
    let stop_loops = stop.clone();
    let logs_loops = logs.clone();
    let busy_loops = busy.clone();
    let paused_loops = paused.clone();
    let observers_loops = observers.clone();
    handle.spawn(async move {
        if let Err(e) = runtime::run_loops(
            cfg_loops,
            stop_loops,
            logs_loops,
            busy_loops,
            paused_loops,
            observers_loops,
            LoopSchedule::default(),
        )
        .await
        {
            tracing::error!(target: "studio_worker::ui", error = %e, "run_loops exited");
        }
    });

    let app_state = app::AppDeps {
        cfg: cfg.clone(),
        logs: logs.clone(),
        busy: busy.clone(),
        paused: paused.clone(),
        observers: observers.clone(),
        stop: stop.clone(),
        config_path: path,
        tokio: handle.clone(),
    };

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([960.0, 720.0])
        .with_min_inner_size([640.0, 480.0])
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

    // The tray menu label flips between "Pause" / "Resume" based on
    // the current paused state; start with the live value so the
    // first render is correct.
    let initial_paused = paused.load(std::sync::atomic::Ordering::SeqCst);
    // The Linux (ksni) tray backend runs on the tokio runtime; hand it
    // a runtime handle so it can spawn its zbus service.
    let tokio_for_tray = handle.clone();

    eframe::run_native(
        "studio-worker",
        native_options,
        Box::new(move |cc| {
            // Dark mode by default (project design rule).
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
            let app = app::App::with_notifier_and_registration(
                app_state,
                app::App::default_notifier_box(),
                registration,
            );
            let quit_handle = app.quit_requested_handle();

            // Best-effort tray.  Linux uses ksni (pure Rust); macOS /
            // Windows use tray-icon.  Either may be unavailable (no
            // StatusNotifier host, no system tray) — the window UI keeps
            // working without it rather than aborting startup.
            let tray_handle = tray_host::install(
                cc.egui_ctx.clone(),
                paused.clone(),
                quit_handle,
                tokio_for_tray,
                initial_paused,
            );
            // Stash the tray inside the App so it lives as long as the
            // event loop (dropping it removes the icon).
            let mut app = app;
            if let Some(tray) = tray_handle {
                app.attach_tray(tray);
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))?;

    // Signal loops to wind down once the window closes.
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// Reconcile the on-login autostart entry with the configured
/// `auto_start` at UI launch.  The decision is the pure
/// [`autostart::launch_sync_action`]; this only performs the chosen
/// side effect and logs the outcome.
fn sync_autostart_on_launch(auto_start: bool) {
    use crate::autostart::{self, AutostartSync};
    match autostart::launch_sync_action(auto_start, autostart::is_enabled()) {
        AutostartSync::Enable => match std::env::current_exe() {
            Ok(exe) => {
                if let Err(e) = autostart::enable(&exe) {
                    tracing::warn!(
                        target: "studio_worker::ui",
                        error = %e,
                        "could not enable autostart-on-login"
                    );
                }
            }
            Err(e) => tracing::warn!(
                target: "studio_worker::ui",
                error = %e,
                "could not resolve current exe to enable autostart-on-login"
            ),
        },
        AutostartSync::Disable => {
            if let Err(e) = autostart::disable() {
                tracing::warn!(
                    target: "studio_worker::ui",
                    error = %e,
                    "could not disable stale autostart-on-login"
                );
            }
        }
        AutostartSync::Noop => {}
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
    use super::dev_window_position;

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
