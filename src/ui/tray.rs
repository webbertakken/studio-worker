//! Tray icon state + menu factory.  The actual `tray_icon::TrayIcon`
//! construction is wired up by `App::install_tray` so the logic that
//! decides *what* the tray looks like (icon variant, menu labels)
//! stays in pure data and is unit-testable.

use std::time::Duration;

use chrono::Utc;

use crate::runtime::HeartbeatStatus;

#[cfg(feature = "ui")]
use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};
#[cfg(feature = "ui")]
use tray_icon::{Icon, TrayIconBuilder};

/// What the tray icon currently advertises about the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayVariant {
    Idle,
    Busy,
    Disconnected,
}

impl TrayVariant {
    /// 16x16 RGBA bytes for the icon.  Each variant is a solid
    /// coloured disk so users distinguish state at a glance without
    /// shipping bespoke art for v1.
    pub fn rgba_16(self) -> Vec<u8> {
        const SIZE: usize = 16;
        let (r, g, b) = match self {
            TrayVariant::Idle => (0x6B, 0xCE, 0x6B),         // green
            TrayVariant::Busy => (0xE8, 0xA8, 0x38),         // amber
            TrayVariant::Disconnected => (0xD0, 0x60, 0x60), // red
        };
        let cx = (SIZE as f32 - 1.0) / 2.0;
        let cy = cx;
        let radius = 6.5;
        let mut buf = vec![0u8; SIZE * SIZE * 4];
        for y in 0..SIZE {
            for x in 0..SIZE {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                let i = (y * SIZE + x) * 4;
                if dist <= radius {
                    buf[i] = r;
                    buf[i + 1] = g;
                    buf[i + 2] = b;
                    buf[i + 3] = 0xFF;
                }
            }
        }
        buf
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            TrayVariant::Idle => "studio-worker — idle",
            TrayVariant::Busy => "studio-worker — running a job",
            TrayVariant::Disconnected => "studio-worker — disconnected",
        }
    }
}

/// Derive the tray variant from live state.  A heartbeat that's
/// missing or older than `disconnect_threshold` flips the variant
/// to `Disconnected`.
pub fn derive_variant(
    busy: bool,
    last_heartbeat: Option<&HeartbeatStatus>,
    heartbeat_interval: Duration,
) -> TrayVariant {
    if busy {
        return TrayVariant::Busy;
    }
    match last_heartbeat {
        None => TrayVariant::Disconnected,
        Some(hb) => {
            let age = Utc::now().signed_duration_since(hb.last_attempt_at);
            let stale =
                age.num_milliseconds() as u128 > (heartbeat_interval.as_millis().saturating_mul(3));
            if stale || !matches!(hb.outcome, crate::runtime::HeartbeatOutcome::Ok) {
                TrayVariant::Disconnected
            } else {
                TrayVariant::Idle
            }
        }
    }
}

/// Stable IDs used both as `MenuId`s on the muda side and to match
/// menu events back to actions.
pub mod menu_ids {
    pub const OPEN_WINDOW: &str = "studio-worker.open-window";
    pub const TOGGLE_AUTO: &str = "studio-worker.toggle-auto";
    pub const QUIT: &str = "studio-worker.quit";
}

#[cfg(feature = "ui")]
pub struct TrayHandle {
    /// Keep the TrayIcon alive for the duration of the session.
    pub _icon: tray_icon::TrayIcon,
}

#[cfg(feature = "ui")]
pub fn install(variant: TrayVariant, labels: &MenuLabels) -> anyhow::Result<TrayHandle> {
    let menu = Menu::new();
    let open = MenuItem::with_id(
        MenuId::new(menu_ids::OPEN_WINDOW),
        labels.open_window,
        true,
        None,
    );
    let toggle = MenuItem::with_id(
        MenuId::new(menu_ids::TOGGLE_AUTO),
        &labels.toggle_auto,
        true,
        None,
    );
    let quit = MenuItem::with_id(MenuId::new(menu_ids::QUIT), labels.quit, true, None);
    menu.append(&open)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&toggle)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit)?;

    let icon = Icon::from_rgba(variant.rgba_16(), 16, 16)?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(variant.tooltip())
        .with_icon(icon)
        .build()?;
    Ok(TrayHandle { _icon: tray })
}

/// Menu labels the tray exposes given current state.  Pure data so
/// it's snapshot-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuLabels {
    pub open_window: &'static str,
    pub toggle_auto: String,
    pub quit: &'static str,
}

pub fn menu_labels(auto_enabled: bool) -> MenuLabels {
    MenuLabels {
        open_window: "Open Window",
        toggle_auto: if auto_enabled {
            "Pause claiming".into()
        } else {
            "Resume claiming".into()
        },
        quit: "Quit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::HeartbeatOutcome;
    use chrono::Duration as ChronoDuration;

    #[test]
    fn variant_is_busy_when_busy_regardless_of_heartbeat() {
        let hb = HeartbeatStatus {
            last_attempt_at: Utc::now(),
            outcome: HeartbeatOutcome::Err { reason: "x".into() },
        };
        assert_eq!(
            derive_variant(true, Some(&hb), Duration::from_secs(5)),
            TrayVariant::Busy
        );
    }

    #[test]
    fn variant_is_disconnected_without_any_heartbeat() {
        assert_eq!(
            derive_variant(false, None, Duration::from_secs(5)),
            TrayVariant::Disconnected
        );
    }

    #[test]
    fn variant_is_idle_when_heartbeat_recent_and_ok() {
        let hb = HeartbeatStatus {
            last_attempt_at: Utc::now() - ChronoDuration::seconds(1),
            outcome: HeartbeatOutcome::Ok,
        };
        assert_eq!(
            derive_variant(false, Some(&hb), Duration::from_secs(5)),
            TrayVariant::Idle
        );
    }

    #[test]
    fn variant_is_disconnected_when_heartbeat_failed() {
        let hb = HeartbeatStatus {
            last_attempt_at: Utc::now(),
            outcome: HeartbeatOutcome::Err {
                reason: "5xx".into(),
            },
        };
        assert_eq!(
            derive_variant(false, Some(&hb), Duration::from_secs(5)),
            TrayVariant::Disconnected
        );
    }

    #[test]
    fn variant_is_disconnected_when_heartbeat_stale() {
        // Heartbeat 30s ago with 5s interval → 6x older than interval.
        let hb = HeartbeatStatus {
            last_attempt_at: Utc::now() - ChronoDuration::seconds(30),
            outcome: HeartbeatOutcome::Ok,
        };
        assert_eq!(
            derive_variant(false, Some(&hb), Duration::from_secs(5)),
            TrayVariant::Disconnected
        );
    }

    #[test]
    fn rgba_16_is_correct_size() {
        assert_eq!(TrayVariant::Idle.rgba_16().len(), 16 * 16 * 4);
        assert_eq!(TrayVariant::Busy.rgba_16().len(), 16 * 16 * 4);
        assert_eq!(TrayVariant::Disconnected.rgba_16().len(), 16 * 16 * 4);
    }

    #[test]
    fn rgba_16_disk_has_opaque_centre_and_clear_corners() {
        let buf = TrayVariant::Idle.rgba_16();
        // Centre pixel (8, 8): alpha = 0xFF
        let centre = (8 * 16 + 8) * 4;
        assert_eq!(buf[centre + 3], 0xFF);
        // Corner pixel (0, 0): alpha = 0
        assert_eq!(buf[3], 0);
    }

    #[test]
    fn menu_labels_flip_with_auto_enabled() {
        assert_eq!(menu_labels(true).toggle_auto, "Pause claiming");
        assert_eq!(menu_labels(false).toggle_auto, "Resume claiming");
        assert_eq!(menu_labels(true).open_window, "Open Window");
        assert_eq!(menu_labels(true).quit, "Quit");
    }
}
