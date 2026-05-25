//! Native egui desktop UI.  See `plans/native-ui.md` for the full
//! design (tabs, tray icon, notifications, autostart).
//!
//! This module is gated behind the `ui` cargo feature so headless
//! installs and the systemd / launchd service path don't pull in
//! egui / eframe / tray-icon / notify-rust + their system libs.

pub mod app;
pub mod autostart;
pub mod notifier;
pub mod register;
pub mod tab;
pub mod tabs;
pub mod tray;

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
        cfg: cfg.clone(),
        logs: logs.clone(),
        busy: busy.clone(),
        observers: observers.clone(),
        stop: stop.clone(),
        config_path: path,
        tokio: handle,
    };

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0])
            .with_min_inner_size([640.0, 480.0])
            .with_title("studio-worker"),
        ..Default::default()
    };

    let auto_enabled = { cfg.lock().auto_enabled };

    eframe::run_native(
        "studio-worker",
        native_options,
        Box::new(move |cc| {
            // Dark mode by default (project design rule).
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
            let app = app::App::new(app_state);
            let quit_handle = app.quit_requested_handle();

            // Best-effort tray icon.  On Linux without a running
            // app-indicator host (e.g. an X session with no system
            // tray) `build()` returns Err; we keep the UI usable
            // without a tray rather than aborting startup.
            let tray_state =
                install_tray(cc.egui_ctx.clone(), cfg.clone(), quit_handle, auto_enabled);
            // Stash the tray inside the App so it lives as long as the
            // event loop (dropping it removes the icon).
            let mut app = app;
            app.attach_tray(tray_state);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))?;

    // Signal loops to wind down once the window closes.
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// State the App keeps for the lifetime of the tray.  The tray icon
/// must be retained — dropping it removes the icon from the system
/// bar.
pub struct TrayState {
    pub icon: Option<tray_icon::TrayIcon>,
    pub current_variant: tray::TrayVariant,
    pub menu_ids: TrayMenuIds,
}

#[derive(Debug, Clone)]
pub struct TrayMenuIds {
    pub open_window: tray_icon::menu::MenuId,
    pub toggle_auto: tray_icon::menu::MenuId,
    pub quit: tray_icon::menu::MenuId,
}

fn install_tray(
    ctx: eframe::egui::Context,
    cfg: crate::config::SharedConfig,
    quit_requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    auto_enabled: bool,
) -> TrayState {
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    let labels = tray::menu_labels(auto_enabled);
    let open_id = MenuId::new(tray::menu_ids::OPEN_WINDOW);
    let toggle_id = MenuId::new(tray::menu_ids::TOGGLE_AUTO);
    let quit_id = MenuId::new(tray::menu_ids::QUIT);
    let menu = Menu::new();
    let open_item = MenuItem::with_id(open_id.clone(), labels.open_window, true, None);
    let toggle_item = MenuItem::with_id(toggle_id.clone(), labels.toggle_auto, true, None);
    let quit_item = MenuItem::with_id(quit_id.clone(), labels.quit, true, None);
    let _ = menu.append(&open_item);
    let _ = menu.append(&toggle_item);
    let _ = menu.append(&quit_item);

    let variant = tray::TrayVariant::Disconnected;
    let icon = Icon::from_rgba(variant.rgba_16(), 16, 16).ok();
    let mut builder = TrayIconBuilder::new()
        .with_tooltip(variant.tooltip())
        .with_menu(Box::new(menu));
    if let Some(i) = icon {
        builder = builder.with_icon(i);
    }
    let tray = builder.build().ok();

    // Forward muda menu events to the app via the channels the tray
    // icon crate exposes globally.  We poll on a background thread to
    // request a repaint and route the click.
    let ctx_clone = ctx.clone();
    let cfg_clone = cfg.clone();
    let open_for_thread = open_id.clone();
    let toggle_for_thread = toggle_id.clone();
    let quit_for_thread = quit_id.clone();
    std::thread::spawn(move || {
        let rx = MenuEvent::receiver();
        while let Ok(event) = rx.recv() {
            if event.id == open_for_thread {
                ctx_clone.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
                ctx_clone.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
            } else if event.id == toggle_for_thread {
                let mut guard = cfg_clone.lock();
                guard.auto_enabled = !guard.auto_enabled;
                let snapshot = guard.clone();
                drop(guard);
                // Best-effort persistence — the default config path
                // is the one we loaded from.  Re-resolution happens
                // via `config::resolve_path(None)`.
                if let Ok(p) = crate::config::resolve_path(None) {
                    let _ = crate::config::save(&snapshot, &p);
                }
            } else if event.id == quit_for_thread {
                quit_requested.store(true, std::sync::atomic::Ordering::SeqCst);
                ctx_clone.request_repaint();
            }
            ctx_clone.request_repaint();
        }
    });

    TrayState {
        icon: tray,
        current_variant: variant,
        menu_ids: TrayMenuIds {
            open_window: open_id,
            toggle_auto: toggle_id,
            quit: quit_id,
        },
    }
}
