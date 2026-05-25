//! Native egui desktop UI.  See `plans/native-ui.md` for the full
//! design (tabs, tray icon, notifications, autostart).
//!
//! This module is gated behind the `ui` cargo feature so headless
//! installs and the systemd / launchd service path don't pull in
//! egui / eframe / tray-icon / notify-rust + their system libs.

pub mod app;
pub mod tab;

use std::sync::{atomic::AtomicBool, Arc};

use anyhow::{anyhow, Result};
use parking_lot::Mutex;

use crate::{
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

    let cfg = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));
    let logs: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let observers = WorkerObservers::default();

    // Spawn the loops on the tokio runtime that's already driving
    // `run_cli` (multi-threaded — main.rs builds `Runtime::new()`).
    // `eframe::run_native` blocks the main thread; the loops keep
    // ticking on worker threads.
    let handle = tokio::runtime::Handle::current();
    let cfg_loops = cfg.clone();
    let stop_loops = stop.clone();
    let logs_loops = logs.clone();
    let busy_loops = busy.clone();
    let observers_loops = observers.clone();
    handle.spawn(async move {
        runtime::run_loops(
            cfg_loops,
            stop_loops,
            logs_loops,
            busy_loops,
            observers_loops,
            LoopSchedule::default(),
        )
        .await;
    });

    let app_state = app::AppDeps {
        cfg,
        logs,
        busy,
        observers,
        stop: stop.clone(),
        config_path: path,
    };

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0])
            .with_min_inner_size([640.0, 480.0])
            .with_title("studio-worker"),
        ..Default::default()
    };

    eframe::run_native(
        "studio-worker",
        native_options,
        Box::new(move |cc| {
            // Dark mode by default (project design rule).
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
            Ok(Box::new(app::App::new(app_state)))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))?;

    // Signal loops to wind down once the window closes.
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}
