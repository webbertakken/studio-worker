//! Cross-platform system-tray host.
//!
//! The tray is best-effort on every OS: the window UI works without it.
//!
//! * **Linux** uses [`ksni`] — a pure-Rust StatusNotifierItem service
//!   over zbus — so the build pulls in no GTK / cairo / appindicator and
//!   `cargo install studio-worker` works on a bare box with no
//!   `pkg-config` / `-dev` packages.
//! * **macOS / Windows** use [`tray-icon`], which talks to the native
//!   tray APIs and needs no extra system libraries.
//!
//! Both backends expose the same [`TrayHandle`] surface: build it once
//! when the window opens, then call [`TrayHandle::set_variant`] whenever
//! the worker's health (idle / busy / disconnected) changes.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use eframe::egui;

use super::tray::{self, TrayVariant};

/// Tracing target for tray lifecycle + click events.  Stable so
/// operators can filter with `RUST_LOG=studio_worker::ui::tray=debug`.
const TRACE_TARGET: &str = "studio_worker::ui::tray";

/// Emit a structured breadcrumb when the native tray icon image fails to
/// build from its RGBA buffer.  Shared by both build sites — `install`
/// (startup) and `set_variant` (per health change) — so a failure is
/// never swallowed and the breadcrumb is unit-testable on the Linux CI
/// box even though the call sites are `#[cfg(not(target_os = "linux"))]`.
/// `op` names the lifecycle phase that hit the failure.
#[cfg(any(not(target_os = "linux"), test))]
fn log_icon_build_failure(op: &'static str, err: &str) {
    tracing::warn!(
        target: TRACE_TARGET,
        op,
        error = %err,
        "failed to build tray icon image"
    );
}

/// Handle the [`App`](crate::ui::app::App) keeps for the lifetime of the
/// window; dropping it tears the tray down.  `set_variant` pushes a new
/// health colour + tooltip when the worker's state changes.
pub struct TrayHandle {
    inner: Inner,
}

impl TrayHandle {
    /// Push a new health variant to the OS tray.  Best-effort: any
    /// failure is logged once, never panics.
    pub fn set_variant(&mut self, variant: TrayVariant) {
        self.inner.set_variant(variant);
    }
}

// ---------------------------------------------------------------------------
// Linux backend — ksni (pure Rust, no GTK).
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
struct Inner {
    tx: tokio::sync::mpsc::UnboundedSender<TrayVariant>,
    warned: bool,
}

#[cfg(target_os = "linux")]
impl Inner {
    fn set_variant(&mut self, variant: TrayVariant) {
        // The ksni service runs on the tokio runtime; forward the new
        // variant to it.  A send error means the service never started
        // (or already shut down) — warn once so the operator knows the
        // status icon is stale rather than silently swallowing it.
        if self.tx.send(variant).is_err() && !self.warned {
            self.warned = true;
            tracing::warn!(
                target: TRACE_TARGET,
                op = "set_variant",
                "linux tray service is not running; status icon will not update"
            );
        }
    }
}

/// The ksni `Tray` model.  Holds the shared runtime flags so menu
/// activations (and a left-click) drive the same actions the in-window
/// controls do.
#[cfg(target_os = "linux")]
struct KsniTray {
    variant: TrayVariant,
    paused: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    ctx: egui::Context,
}

#[cfg(target_os = "linux")]
impl KsniTray {
    fn show_window(&self) {
        tracing::info!(target: TRACE_TARGET, "open window requested from tray");
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Visible(true));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.ctx.request_repaint();
    }

    fn toggle_pause(&self) {
        let was_paused = self.paused.fetch_xor(true, Ordering::SeqCst);
        tracing::info!(
            target: TRACE_TARGET,
            paused = !was_paused,
            "pause toggled from tray menu"
        );
        self.ctx.request_repaint();
    }

    fn request_quit(&self) {
        tracing::info!(
            target: TRACE_TARGET,
            "quit requested from tray menu; stopping worker"
        );
        self.quit.store(true, Ordering::SeqCst);
        self.ctx.request_repaint();
    }
}

#[cfg(target_os = "linux")]
impl ksni::Tray for KsniTray {
    fn id(&self) -> String {
        "studio-worker".into()
    }

    fn title(&self) -> String {
        "studio-worker".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![ksni::Icon {
            width: 16,
            height: 16,
            data: tray::rgba_to_argb32(&self.variant.rgba_16()),
        }]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.variant.tooltip().to_string(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.show_window();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        // `menu_labels` flips Pause/Resume on `auto_enabled` (= not paused).
        let labels = tray::menu_labels(!self.paused.load(Ordering::SeqCst));
        vec![
            StandardItem {
                label: labels.open_window.to_string(),
                activate: Box::new(|t: &mut Self| t.show_window()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: labels.toggle_auto.clone(),
                activate: Box::new(|t: &mut Self| t.toggle_pause()),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: labels.quit.to_string(),
                activate: Box::new(|t: &mut Self| t.request_quit()),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawn the ksni tray on the tokio runtime and return a handle that
/// forwards variant updates to it.  The service is set up
/// asynchronously; variant updates sent before it is ready are buffered
/// and applied once it starts.  Returns `Some` immediately (the channel
/// always exists); the icon itself appears only if a StatusNotifier host
/// is present (KDE, GNOME-with-AppIndicator, etc.).
#[cfg(target_os = "linux")]
pub fn install(
    ctx: egui::Context,
    paused: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    tokio: tokio::runtime::Handle,
    _initial_paused: bool,
) -> Option<TrayHandle> {
    use ksni::TrayMethods;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TrayVariant>();
    tokio.spawn(async move {
        let tray = KsniTray {
            variant: TrayVariant::Disconnected,
            paused,
            quit,
            ctx,
        };
        let handle = match tray.spawn().await {
            Ok(h) => {
                tracing::info!(target: TRACE_TARGET, "linux tray (ksni) started");
                h
            }
            Err(e) => {
                tracing::warn!(
                    target: TRACE_TARGET,
                    error = %e,
                    "linux tray (ksni) failed to start; running without a tray"
                );
                return;
            }
        };
        while let Some(variant) = rx.recv().await {
            handle
                .update(move |t: &mut KsniTray| t.variant = variant)
                .await;
        }
        // All senders dropped (window closed) — tear the tray down.
        handle.shutdown().await;
    });
    Some(TrayHandle {
        inner: Inner { tx, warned: false },
    })
}

// ---------------------------------------------------------------------------
// macOS / Windows backend — tray-icon (native).
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
struct Inner {
    icon: Option<tray_icon::TrayIcon>,
}

#[cfg(not(target_os = "linux"))]
impl Inner {
    fn set_variant(&mut self, variant: TrayVariant) {
        let Some(icon) = self.icon.as_ref() else {
            return;
        };
        match tray_icon::Icon::from_rgba(variant.rgba_16(), 16, 16) {
            Ok(new_icon) => {
                if let Err(e) = icon.set_icon(Some(new_icon)) {
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "set_variant",
                        error = %e,
                        "failed to update tray icon"
                    );
                }
            }
            Err(e) => log_icon_build_failure("set_variant", &e.to_string()),
        }
        if let Err(e) = icon.set_tooltip(Some(variant.tooltip())) {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "set_variant",
                error = %e,
                "failed to update tray tooltip"
            );
        }
    }
}

/// Build the native tray icon + menu (on the current — main — thread,
/// as tray-icon requires) and spawn a thread that routes menu clicks to
/// the shared runtime flags.  Returns `None` only when the platform tray
/// host rejects the icon; the window UI keeps working regardless.
#[cfg(not(target_os = "linux"))]
pub fn install(
    ctx: egui::Context,
    paused: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    _tokio: tokio::runtime::Handle,
    initial_paused: bool,
) -> Option<TrayHandle> {
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    let open_id = MenuId::new(tray::menu_ids::OPEN_WINDOW);
    let toggle_id = MenuId::new(tray::menu_ids::TOGGLE_AUTO);
    let quit_id = MenuId::new(tray::menu_ids::QUIT);

    // `menu_labels` flips Pause/Resume on `auto_enabled` (= not paused).
    let labels = tray::menu_labels(!initial_paused);
    let menu = Menu::new();
    let _ = menu.append(&MenuItem::with_id(
        open_id.clone(),
        labels.open_window,
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        toggle_id.clone(),
        &labels.toggle_auto,
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(quit_id.clone(), labels.quit, true, None));

    let variant = TrayVariant::Disconnected;
    let icon = match Icon::from_rgba(variant.rgba_16(), 16, 16) {
        Ok(i) => Some(i),
        Err(e) => {
            log_icon_build_failure("install", &e.to_string());
            None
        }
    };
    let mut builder = TrayIconBuilder::new()
        .with_tooltip(variant.tooltip())
        .with_menu(Box::new(menu));
    if let Some(i) = icon {
        builder = builder.with_icon(i);
    }
    let tray_icon = match builder.build() {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(
                target: TRACE_TARGET,
                error = %e,
                "tray build failed; running without a tray"
            );
            None
        }
    };

    // Route muda menu events to the shared flags on a background thread.
    std::thread::spawn(move || {
        let rx = MenuEvent::receiver();
        while let Ok(event) = rx.recv() {
            if event.id == open_id {
                tracing::info!(target: TRACE_TARGET, "open window requested from tray menu");
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if event.id == toggle_id {
                let was_paused = paused.fetch_xor(true, Ordering::SeqCst);
                tracing::info!(
                    target: TRACE_TARGET,
                    paused = !was_paused,
                    "pause toggled from tray menu"
                );
            } else if event.id == quit_id {
                tracing::info!(
                    target: TRACE_TARGET,
                    "quit requested from tray menu; stopping worker"
                );
                quit.store(true, Ordering::SeqCst);
            }
            ctx.request_repaint();
        }
    });

    Some(TrayHandle {
        inner: Inner { icon: tray_icon },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The mac/win tray builds an icon image from a static RGBA buffer in
    // two places (`install` at startup, `set_variant` on every health
    // change).  Both must surface a build failure rather than swallow it,
    // so the breadcrumb is exercised here on the Linux CI box even though
    // the call sites themselves are `#[cfg(not(target_os = "linux"))]`.
    #[test]
    fn icon_build_failure_emits_structured_warn() {
        let logs = crate::test_support::capture(|| {
            log_icon_build_failure("install", "bad rgba length");
        });
        assert!(logs.contains("WARN"), "expected WARN level, got: {logs}");
        assert!(
            logs.contains("studio_worker::ui::tray"),
            "expected tray target, got: {logs}"
        );
        assert!(logs.contains("op=\"install\""), "expected op field: {logs}");
        assert!(
            logs.contains("error=bad rgba length"),
            "expected structured error field, got: {logs}"
        );
        assert!(
            logs.contains("failed to build tray icon image"),
            "expected build-failure message: {logs}"
        );
    }

    #[test]
    fn icon_build_failure_op_field_tracks_the_call_site() {
        let logs = crate::test_support::capture(|| {
            log_icon_build_failure("set_variant", "oops");
        });
        assert!(
            logs.contains("op=\"set_variant\""),
            "expected set_variant op field: {logs}"
        );
    }
}
