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
        tokio: handle,
    };

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0])
            .with_min_inner_size([640.0, 480.0])
            .with_title("studio-worker"),
        ..Default::default()
    };

    // The tray menu label flips between "Pause" / "Resume" based on
    // the current paused state; start with the live value so the
    // first render is correct.
    let initial_paused = paused.load(std::sync::atomic::Ordering::SeqCst);

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

            // Best-effort tray icon.  On Linux without a running
            // app-indicator host (e.g. an X session with no system
            // tray) `build()` returns Err; we keep the UI usable
            // without a tray rather than aborting startup.
            let tray_state = install_tray(
                cc.egui_ctx.clone(),
                paused.clone(),
                quit_handle,
                initial_paused,
            );
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
    paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    quit_requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    initial_paused: bool,
) -> TrayState {
    use tray_icon::menu::{MenuEvent, MenuId};

    let open_id = MenuId::new(tray::menu_ids::OPEN_WINDOW);
    let toggle_id = MenuId::new(tray::menu_ids::TOGGLE_AUTO);
    let quit_id = MenuId::new(tray::menu_ids::QUIT);

    // Linux: tray-icon's AppIndicator backend needs GTK initialised on
    // the same thread that runs its main loop.  We spawn a dedicated
    // thread for that, build the TrayIcon there, and let it run
    // `gtk::main()` until process shutdown.  The TrayIcon handle
    // stays in that thread's stack; updating the icon variant from
    // the main thread would race so we skip that for now (variant
    // changes still drive the in-window Status badge).
    //
    // macOS / Windows: tray-icon must be built on the main thread
    // that owns the winit event loop.  We do that inline.
    let labels = tray::menu_labels(initial_paused);
    let open_label = labels.open_window;
    let toggle_label = labels.toggle_auto.clone();
    let quit_label = labels.quit;

    let tray_holder = spawn_tray_thread(
        open_id.clone(),
        toggle_id.clone(),
        quit_id.clone(),
        open_label,
        toggle_label,
        quit_label,
    );

    // Forward muda menu events to the app via the channels the tray
    // icon crate exposes globally.  Poll on a background thread to
    // request a repaint and route the click.
    let ctx_clone = ctx.clone();
    let paused_clone = paused.clone();
    let open_for_thread = open_id.clone();
    let toggle_for_thread = toggle_id.clone();
    let quit_for_thread = quit_id.clone();
    std::thread::spawn(move || {
        let rx = MenuEvent::receiver();
        while let Ok(event) = rx.recv() {
            if event.id == open_for_thread {
                tracing::info!(
                    target: "studio_worker::ui::tray",
                    "open window requested from tray menu"
                );
                ctx_clone.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
                ctx_clone.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
            } else if event.id == toggle_for_thread {
                let was_paused = paused_clone.fetch_xor(true, std::sync::atomic::Ordering::SeqCst);
                tracing::info!(
                    target: "studio_worker::ui::tray",
                    paused = !was_paused,
                    "pause toggled from tray menu"
                );
            } else if event.id == quit_for_thread {
                tracing::info!(
                    target: "studio_worker::ui::tray",
                    "quit requested from tray menu; stopping worker"
                );
                quit_requested.store(true, std::sync::atomic::Ordering::SeqCst);
                ctx_clone.request_repaint();
            }
            ctx_clone.request_repaint();
        }
    });

    TrayState {
        icon: tray_holder,
        current_variant: tray::TrayVariant::Disconnected,
        menu_ids: TrayMenuIds {
            open_window: open_id,
            toggle_auto: toggle_id,
            quit: quit_id,
        },
    }
}

#[cfg(target_os = "linux")]
fn spawn_tray_thread(
    open_id: tray_icon::menu::MenuId,
    toggle_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
    open_label: &'static str,
    toggle_label: String,
    quit_label: &'static str,
) -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    std::thread::spawn(move || {
        if let Err(e) = gtk::init() {
            tracing::warn!(
                target: "studio_worker::ui::tray",
                "gtk init failed: {e}"
            );
            return;
        }
        let menu = Menu::new();
        let open_item = MenuItem::with_id(open_id, open_label, true, None);
        let toggle_item = MenuItem::with_id(toggle_id, &toggle_label, true, None);
        let quit_item = MenuItem::with_id(quit_id, quit_label, true, None);
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
        let _tray = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    target: "studio_worker::ui::tray",
                    "tray build failed: {e}"
                );
                return;
            }
        };
        // Block this thread on gtk's main loop so the tray stays
        // alive + AppIndicator events get serviced.
        gtk::main();
    });
    // Linux: handle stays on the gtk thread; the main thread can't
    // mutate the icon (would race).  We track variant changes for
    // the in-window status badge instead.
    None
}

#[cfg(not(target_os = "linux"))]
fn spawn_tray_thread(
    open_id: tray_icon::menu::MenuId,
    toggle_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
    open_label: &'static str,
    toggle_label: String,
    quit_label: &'static str,
) -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    let menu = Menu::new();
    let open_item = MenuItem::with_id(open_id, open_label, true, None);
    let toggle_item = MenuItem::with_id(toggle_id, &toggle_label, true, None);
    let quit_item = MenuItem::with_id(quit_id, quit_label, true, None);
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
    builder.build().ok()
}
