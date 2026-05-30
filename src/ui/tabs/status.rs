//! Status tab — surfaces who the worker is, who it's talking to, and
//! how recently it last successfully heartbeat.  When the worker
//! hasn't registered yet, this tab shows the in-window Register form
//! (fork #2 of plans/native-ui.md, default A).

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use chrono::{DateTime, Utc};
use eframe::egui;

use crate::{
    auto_register::RegistrationState,
    config::Config,
    runtime::{HeartbeatOutcome, HeartbeatStatus},
};

/// Pure-data view of the Status tab.  Constructed each frame from
/// the live shared state; no egui types in scope so it's
/// unit-testable.  Maps onto `auto_register::RegistrationState` for
/// the pre-registration phases.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusView {
    /// Worker is auto-registering but hasn't received a request_id
    /// from the studio yet.  Transient — normally only seen for a
    /// frame or two on first launch.
    Initialising { api_base_url: String },
    /// Studio has a Pending Workers row for this install; UI shows
    /// the request id + a copy button so the operator can match it
    /// in the dashboard.
    Pending {
        api_base_url: String,
        request_id: String,
        since: DateTime<Utc>,
    },
    /// Operator rejected the request.  Worker stops trying; the
    /// hint instructs the user to run `studio-worker register --reset`
    /// to clear state and try again.
    Rejected {
        api_base_url: String,
        reason: String,
    },
    Registered {
        worker_id: String,
        api_base_url: String,
        vram_total_gb: f32,
        vram_threshold_gb: f32,
        paused: bool,
        busy: bool,
        last_heartbeat: Option<HeartbeatSummary>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatSummary {
    pub when: DateTime<Utc>,
    pub ok: bool,
    pub reason: Option<String>,
}

impl HeartbeatSummary {
    pub fn from(status: &HeartbeatStatus) -> Self {
        match &status.outcome {
            HeartbeatOutcome::Ok => Self {
                when: status.last_attempt_at,
                ok: true,
                reason: None,
            },
            HeartbeatOutcome::Err { reason } => Self {
                when: status.last_attempt_at,
                ok: false,
                reason: Some(reason.clone()),
            },
        }
    }
}

impl StatusView {
    pub fn build(
        cfg: &Config,
        registration: &RegistrationState,
        busy: bool,
        paused: bool,
        last_heartbeat: Option<&HeartbeatStatus>,
        vram_total_gb: f32,
    ) -> Self {
        let registered = cfg.worker_id.is_some() && cfg.auth_token.is_some();
        if registered {
            return Self::Registered {
                worker_id: cfg.worker_id.clone().unwrap_or_default(),
                api_base_url: cfg.api_base_url.clone(),
                vram_total_gb,
                vram_threshold_gb: cfg.vram_threshold_gb,
                paused,
                busy,
                last_heartbeat: last_heartbeat.map(HeartbeatSummary::from),
            };
        }
        match registration {
            RegistrationState::Pending { request_id, since } => Self::Pending {
                api_base_url: cfg.api_base_url.clone(),
                request_id: request_id.clone(),
                since: *since,
            },
            RegistrationState::Rejected { reason } => Self::Rejected {
                api_base_url: cfg.api_base_url.clone(),
                reason: reason.clone(),
            },
            // Pristine — cold start, between requests, or operator
            // bootstrap path that hasn't completed yet.
            RegistrationState::Pristine | RegistrationState::Approved => Self::Initialising {
                api_base_url: cfg.api_base_url.clone(),
            },
        }
    }
}

/// Human-friendly "5s ago" formatting for a heartbeat timestamp.
pub fn format_age(now: DateTime<Utc>, when: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(when);
    let secs = delta.num_seconds();
    if secs < 0 {
        return "just now".into();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        let rem = secs % 60;
        return format!("{mins}m {rem:02}s ago");
    }
    let hours = mins / 60;
    let rem_min = mins % 60;
    format!("{hours}h {rem_min:02}m ago")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render(ui: &mut egui::Ui, view: &StatusView, paused_flag: &Arc<AtomicBool>) {
    match view {
        StatusView::Initialising { api_base_url } => render_initialising(ui, api_base_url),
        StatusView::Pending {
            api_base_url,
            request_id,
            since,
        } => render_pending(ui, api_base_url, request_id, *since),
        StatusView::Rejected {
            api_base_url,
            reason,
        } => render_rejected(ui, api_base_url, reason),
        StatusView::Registered { .. } => render_registered(ui, view, paused_flag),
    }
}

fn render_initialising(ui: &mut egui::Ui, api_base_url: &str) {
    ui.heading("Initialising");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(format!(
            "Asking {api_base_url} for a registration slot\u{2026}"
        ));
    });
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(
            "No action needed.  The worker will keep retrying until it gets through.",
        )
        .italics()
        .color(egui::Color32::from_gray(160)),
    );
}

fn render_pending(ui: &mut egui::Ui, api_base_url: &str, request_id: &str, since: DateTime<Utc>) {
    ui.heading("Waiting for approval");
    ui.add_space(4.0);
    ui.label(format!(
        "This worker has registered with {api_base_url} and is waiting for the \
         studio operator to approve it.  You can keep this window open or close \
         it \u{2014} the worker keeps polling in the background."
    ));
    ui.add_space(12.0);
    egui::Grid::new("pending_grid")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Request ID");
            ui.horizontal(|ui| {
                ui.monospace(request_id);
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(request_id.to_string());
                }
            });
            ui.end_row();

            ui.label("Waiting");
            ui.label(format_age(Utc::now(), since));
            ui.end_row();
        });
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(
            "Share the Request ID with the studio operator if you want them to \
             find your pending row quickly.",
        )
        .italics()
        .color(egui::Color32::from_gray(160)),
    );
}

fn render_rejected(ui: &mut egui::Ui, api_base_url: &str, reason: &str) {
    ui.heading("Registration rejected");
    ui.add_space(4.0);
    ui.colored_label(
        egui::Color32::LIGHT_RED,
        if reason.is_empty() {
            "The studio operator rejected this worker's registration.".to_string()
        } else {
            format!("The studio operator rejected this worker's registration: {reason}")
        },
    );
    ui.add_space(12.0);
    ui.label(format!(
        "To try again, contact the operator of {api_base_url} to understand why, then run:"
    ));
    ui.add_space(4.0);
    ui.monospace("studio-worker register --reset");
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(
            "This clears the local request state and submits a fresh request on \
             the next launch.",
        )
        .italics()
        .color(egui::Color32::from_gray(160)),
    );
}

fn render_registered(ui: &mut egui::Ui, view: &StatusView, paused_flag: &Arc<AtomicBool>) {
    let StatusView::Registered {
        worker_id,
        api_base_url,
        vram_total_gb,
        vram_threshold_gb,
        paused,
        busy,
        last_heartbeat,
    } = view
    else {
        unreachable!();
    };

    ui.heading("Worker status");
    ui.add_space(4.0);

    let badge = if *busy {
        ("BUSY", egui::Color32::from_rgb(232, 168, 56))
    } else if *paused {
        ("PAUSED", egui::Color32::LIGHT_GRAY)
    } else {
        ("IDLE", egui::Color32::LIGHT_GREEN)
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(badge.0).color(badge.1).strong());
        ui.label("\u{2014}");
        ui.label(if *busy {
            "running a job"
        } else if *paused {
            "claiming paused by operator"
        } else {
            "waiting for work"
        });
    });
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        let (label, hint) = if *paused {
            ("Resume", "start accepting new job offers again")
        } else {
            (
                "Pause",
                "stop accepting new job offers (in-flight job, if any, will finish)",
            )
        };
        if ui.button(label).on_hover_text(hint).clicked() {
            toggle_pause(paused_flag);
        }
    });
    ui.add_space(8.0);

    egui::Grid::new("status_grid")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Worker ID");
            ui.monospace(worker_id);
            ui.end_row();

            ui.label("API base URL");
            ui.monospace(api_base_url);
            ui.end_row();

            ui.label("VRAM total");
            ui.label(format!("{vram_total_gb:.1} GB"));
            ui.end_row();

            ui.label("VRAM threshold");
            ui.label(format!("{vram_threshold_gb:.1} GB per claim"));
            ui.end_row();

            ui.label("Last heartbeat");
            match last_heartbeat {
                None => ui.label("never"),
                Some(h) => {
                    let when = format_age(Utc::now(), h.when);
                    if h.ok {
                        ui.colored_label(egui::Color32::LIGHT_GREEN, format!("ok \u{00b7} {when}"))
                    } else {
                        let reason = h.reason.as_deref().unwrap_or("unknown");
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            format!("error \u{00b7} {when} \u{00b7} {reason}"),
                        )
                    }
                }
            };
            ui.end_row();
        });
}

/// Flip the operator pause flag and emit a structured breadcrumb, so a
/// pause/resume from the Status tab's button is as visible in the logs
/// (and the studio's shipped-log view) as the identical toggle from the
/// tray menu (`ui::mod`).  Without it, pausing from the window left no
/// trace while pausing from the tray did.  `fetch_xor` returns the
/// previous value, so the new paused state is its negation.  Extracted
/// from the button handler so the logging is unit-testable without an
/// egui context.  Returns the new paused state.
fn toggle_pause(paused_flag: &Arc<AtomicBool>) -> bool {
    let now_paused = !paused_flag.fetch_xor(true, Ordering::SeqCst);
    tracing::info!(
        target: "studio_worker::ui::status",
        paused = now_paused,
        "pause toggled from status tab"
    );
    now_paused
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::runtime::HeartbeatStatus;
    use chrono::TimeZone;

    fn registered_cfg() -> Config {
        Config {
            worker_id: Some("w-abc".into()),
            auth_token: Some("tok-xyz".into()),
            api_base_url: "https://studio.example".into(),
            vram_threshold_gb: 12.0,
            ..Config::default()
        }
    }

    #[test]
    fn build_initialising_when_pristine_and_unregistered() {
        let cfg = Config::default();
        let view = StatusView::build(&cfg, &RegistrationState::Pristine, false, false, None, 0.0);
        match view {
            StatusView::Initialising { api_base_url } => {
                assert_eq!(api_base_url, cfg.api_base_url);
            }
            other => panic!("expected Initialising, got {other:?}"),
        }
    }

    #[test]
    fn build_pending_when_state_pending() {
        let cfg = Config::default();
        let since = Utc::now();
        let view = StatusView::build(
            &cfg,
            &RegistrationState::Pending {
                request_id: "rr-42".into(),
                since,
            },
            false,
            false,
            None,
            0.0,
        );
        match view {
            StatusView::Pending {
                request_id,
                since: s,
                ..
            } => {
                assert_eq!(request_id, "rr-42");
                assert_eq!(s, since);
            }
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn build_rejected_when_state_rejected() {
        let cfg = Config::default();
        let view = StatusView::build(
            &cfg,
            &RegistrationState::Rejected {
                reason: "unknown contributor".into(),
            },
            false,
            false,
            None,
            0.0,
        );
        match view {
            StatusView::Rejected { reason, .. } => assert_eq!(reason, "unknown contributor"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn build_registered_takes_precedence_over_registration_state() {
        // If worker_id + auth_token are set, the registration state
        // is irrelevant — we're operational.
        let cfg = registered_cfg();
        let view = StatusView::build(
            &cfg,
            &RegistrationState::Pending {
                request_id: "rr-stale".into(),
                since: Utc::now(),
            },
            false,
            false,
            None,
            24.0,
        );
        assert!(matches!(view, StatusView::Registered { .. }));
    }

    #[test]
    fn build_registered_when_worker_id_and_token_present() {
        let cfg = registered_cfg();
        let view = StatusView::build(&cfg, &RegistrationState::Approved, false, false, None, 24.0);
        match view {
            StatusView::Registered {
                worker_id,
                api_base_url,
                vram_total_gb,
                vram_threshold_gb,
                paused,
                busy,
                last_heartbeat,
            } => {
                assert_eq!(worker_id, "w-abc");
                assert_eq!(api_base_url, "https://studio.example");
                assert!((vram_total_gb - 24.0).abs() < f32::EPSILON);
                assert!((vram_threshold_gb - 12.0).abs() < f32::EPSILON);
                assert!(!paused);
                assert!(!busy);
                assert!(last_heartbeat.is_none());
            }
            _ => panic!("expected Registered"),
        }
    }

    #[test]
    fn build_registered_propagates_paused() {
        let cfg = registered_cfg();
        let view = StatusView::build(&cfg, &RegistrationState::Approved, false, true, None, 24.0);
        match view {
            StatusView::Registered { paused, .. } => assert!(paused),
            _ => panic!("expected Registered"),
        }
    }

    #[test]
    fn build_propagates_heartbeat_ok() {
        let cfg = registered_cfg();
        let hb = HeartbeatStatus {
            last_attempt_at: Utc::now(),
            outcome: HeartbeatOutcome::Ok,
        };
        let view = StatusView::build(
            &cfg,
            &RegistrationState::Approved,
            false,
            false,
            Some(&hb),
            24.0,
        );
        match view {
            StatusView::Registered {
                last_heartbeat: Some(s),
                ..
            } => {
                assert!(s.ok);
                assert!(s.reason.is_none());
            }
            _ => panic!("expected Registered with heartbeat"),
        }
    }

    #[test]
    fn build_propagates_heartbeat_err() {
        let cfg = registered_cfg();
        let hb = HeartbeatStatus {
            last_attempt_at: Utc::now(),
            outcome: HeartbeatOutcome::Err {
                reason: "5xx".into(),
            },
        };
        let view = StatusView::build(
            &cfg,
            &RegistrationState::Approved,
            true,
            false,
            Some(&hb),
            24.0,
        );
        match view {
            StatusView::Registered {
                busy,
                last_heartbeat: Some(s),
                ..
            } => {
                assert!(busy);
                assert!(!s.ok);
                assert_eq!(s.reason.as_deref(), Some("5xx"));
            }
            _ => panic!("expected Registered with err heartbeat"),
        }
    }

    #[test]
    fn format_age_sub_minute() {
        let now = Utc.with_ymd_and_hms(2026, 5, 25, 12, 0, 30).unwrap();
        let then = Utc.with_ymd_and_hms(2026, 5, 25, 12, 0, 18).unwrap();
        assert_eq!(format_age(now, then), "12s ago");
    }

    #[test]
    fn format_age_sub_hour() {
        let now = Utc.with_ymd_and_hms(2026, 5, 25, 12, 5, 30).unwrap();
        let then = Utc.with_ymd_and_hms(2026, 5, 25, 12, 0, 18).unwrap();
        assert_eq!(format_age(now, then), "5m 12s ago");
    }

    #[test]
    fn format_age_multi_hour() {
        let now = Utc.with_ymd_and_hms(2026, 5, 25, 14, 5, 0).unwrap();
        let then = Utc.with_ymd_and_hms(2026, 5, 25, 12, 0, 0).unwrap();
        assert_eq!(format_age(now, then), "2h 05m ago");
    }

    #[test]
    fn format_age_future_clamps_to_just_now() {
        let now = Utc.with_ymd_and_hms(2026, 5, 25, 12, 0, 0).unwrap();
        let then = Utc.with_ymd_and_hms(2026, 5, 25, 12, 0, 5).unwrap();
        assert_eq!(format_age(now, then), "just now");
    }

    #[test]
    fn toggle_pause_flips_flag_and_logs_both_directions() {
        // The Status-tab Pause/Resume button must leave the same
        // breadcrumb the tray-menu toggle does (`ui::mod`), otherwise a
        // pause from the window is invisible in the shipped logs while
        // the identical action from the tray is not.
        let flag = Arc::new(AtomicBool::new(false));

        let out = crate::test_support::capture({
            let flag = flag.clone();
            move || assert!(toggle_pause(&flag), "first toggle must pause")
        });
        assert!(
            flag.load(Ordering::SeqCst),
            "flag is paused after first toggle"
        );
        assert!(out.contains("INFO"), "expected INFO level, got: {out}");
        assert!(
            out.contains("studio_worker::ui::status"),
            "expected the status target, got: {out}"
        );
        assert!(
            out.contains("pause toggled from status tab"),
            "expected the toggle message, got: {out}"
        );
        assert!(
            out.contains("paused=true"),
            "expected paused=true, got: {out}"
        );

        let out = crate::test_support::capture({
            let flag = flag.clone();
            move || assert!(!toggle_pause(&flag), "second toggle must resume")
        });
        assert!(
            !flag.load(Ordering::SeqCst),
            "flag is resumed after second toggle"
        );
        assert!(
            out.contains("paused=false"),
            "expected paused=false, got: {out}"
        );
    }
}
