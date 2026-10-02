//! Tray icon state + menu factory (pure data).  The per-OS tray
//! construction lives in [`super::tray_host`] (`ksni` on Linux,
//! `tray-icon` on macOS / Windows); this module keeps the logic that
//! decides *what* the tray looks like (icon variant, menu labels,
//! ARGB byte order) free of any platform types so it stays
//! unit-testable.

use super::pulse::{Activity, Pulse};
use super::theme::Tone;

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

/// Convert an RGBA byte buffer (what [`TrayVariant::rgba_16`] produces,
/// the format `tray-icon` wants) into the ARGB32 network-byte-order
/// layout `ksni` expects for its `icon_pixmap`.  Each 4-byte
/// `[R, G, B, A]` group is rotated right by one to `[A, R, G, B]`.
/// Pure so the byte-order contract is unit-tested without a live tray.
pub fn rgba_to_argb32(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        px.rotate_right(1);
    }
    out
}

/// The tray colour for what the window's header says, so the two never disagree: red while
/// the daemon does not answer or the studio refuses the worker (auth failed, registration
/// rejected, a fatal session error), busy while a job runs, green otherwise. A worker waiting
/// on the studio (approval, connecting, reconnecting) still runs and serves its local API, so
/// it is green; the window says what it waits for.
pub fn variant_of(pulse: &Pulse) -> TrayVariant {
    match pulse.activity {
        Activity::Offline => TrayVariant::Disconnected,
        Activity::Busy | Activity::Running { .. } => TrayVariant::Busy,
        Activity::Idle | Activity::Paused if pulse.studio.tone == Tone::Bad => {
            TrayVariant::Disconnected
        }
        Activity::Idle | Activity::Paused => TrayVariant::Idle,
    }
}

/// Stable IDs used both as `MenuId`s on the muda side and to match
/// menu events back to actions.
pub mod menu_ids {
    pub const OPEN_WINDOW: &str = "studio-worker.open-window";
    pub const TOGGLE_AUTO: &str = "studio-worker.toggle-auto";
    pub const QUIT: &str = "studio-worker.quit";
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
    use crate::ui::pulse::Signal;

    fn pulse(activity: Activity, studio: Tone) -> Pulse {
        let signal = |tone| Signal {
            label: String::new(),
            tone,
            detail: String::new(),
        };
        Pulse {
            activity,
            daemon: signal(Tone::Good),
            studio: signal(studio),
            gpu: None,
            can_pause: true,
            paused: false,
        }
    }

    #[test]
    fn the_icon_is_red_while_the_daemon_does_not_answer() {
        assert_eq!(
            variant_of(&pulse(Activity::Offline, Tone::Neutral)),
            TrayVariant::Disconnected
        );
    }

    #[test]
    fn a_connected_idle_worker_is_green_with_no_heartbeat_at_all() {
        // The regression: the icon read a heartbeat nothing wrote, so a
        // connected, running worker showed red.
        assert_eq!(
            variant_of(&pulse(Activity::Idle, Tone::Good)),
            TrayVariant::Idle
        );
        assert_eq!(
            variant_of(&pulse(Activity::Paused, Tone::Good)),
            TrayVariant::Idle
        );
    }

    #[test]
    fn a_worker_waiting_on_the_studio_is_still_running_so_green() {
        // Awaiting approval, connecting, reconnecting: the worker runs and
        // serves its local API; the window says what it waits for.
        assert_eq!(
            variant_of(&pulse(Activity::Idle, Tone::Busy)),
            TrayVariant::Idle
        );
    }

    #[test]
    fn a_studio_that_refuses_the_worker_turns_it_red() {
        // Auth failed, registration rejected, a fatal session error.
        assert_eq!(
            variant_of(&pulse(Activity::Idle, Tone::Bad)),
            TrayVariant::Disconnected
        );
    }

    #[test]
    fn a_job_makes_it_busy_whatever_the_studio_says() {
        let running = Activity::Running {
            kind: "llm".into(),
            model: "m".into(),
            elapsed: "1s".into(),
            more: 0,
        };
        assert_eq!(variant_of(&pulse(running, Tone::Bad)), TrayVariant::Busy);
        assert_eq!(
            variant_of(&pulse(Activity::Busy, Tone::Good)),
            TrayVariant::Busy
        );
    }

    #[test]
    fn rgba_16_is_correct_size() {
        assert_eq!(TrayVariant::Idle.rgba_16().len(), 16 * 16 * 4);
        assert_eq!(TrayVariant::Busy.rgba_16().len(), 16 * 16 * 4);
        assert_eq!(TrayVariant::Disconnected.rgba_16().len(), 16 * 16 * 4);
    }

    #[test]
    fn rgba_to_argb32_rotates_each_pixel_and_preserves_length() {
        // One opaque pixel [R, G, B, A] -> [A, R, G, B].
        let rgba = vec![0x11, 0x22, 0x33, 0xFF];
        assert_eq!(rgba_to_argb32(&rgba), vec![0xFF, 0x11, 0x22, 0x33]);
        // A transparent pixel keeps its zero alpha at the front.
        let clear = vec![0x40, 0x50, 0x60, 0x00];
        assert_eq!(rgba_to_argb32(&clear), vec![0x00, 0x40, 0x50, 0x60]);
        // Length is preserved for the real 16x16 icon buffer.
        assert_eq!(
            rgba_to_argb32(&TrayVariant::Idle.rgba_16()).len(),
            16 * 16 * 4
        );
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
