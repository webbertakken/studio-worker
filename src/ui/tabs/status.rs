//! Status tab — surfaces who the worker is, who it's talking to, and
//! how recently it last successfully heartbeat.  When the worker
//! hasn't registered yet, this tab shows the in-window Register form
//! (fork #2 of plans/native-ui.md, default A).

use chrono::{DateTime, Utc};
use eframe::egui;

use crate::{
    config::Config,
    runtime::{HeartbeatOutcome, HeartbeatStatus},
};

use super::super::register::RegistrationStatus;

/// Pure-data view of the Status tab.  Constructed each frame from the
/// live shared state; no egui types in scope so it's unit-testable.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusView {
    Unregistered {
        api_base_url: String,
        bootstrap_token_preview: String,
    },
    Registered {
        worker_id: String,
        api_base_url: String,
        engine: String,
        vram_total_gb: f32,
        vram_threshold_gb: f32,
        auto_enabled: bool,
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
        busy: bool,
        last_heartbeat: Option<&HeartbeatStatus>,
        vram_total_gb: f32,
    ) -> Self {
        let registered = cfg.worker_id.is_some() && cfg.auth_token.is_some();
        if !registered {
            return Self::Unregistered {
                api_base_url: cfg.api_base_url.clone(),
                bootstrap_token_preview: redact_token(&cfg.bootstrap_token),
            };
        }
        Self::Registered {
            worker_id: cfg.worker_id.clone().unwrap_or_default(),
            api_base_url: cfg.api_base_url.clone(),
            engine: cfg.engine.clone(),
            vram_total_gb,
            vram_threshold_gb: cfg.vram_threshold_gb,
            auto_enabled: cfg.auto_enabled,
            busy,
            last_heartbeat: last_heartbeat.map(HeartbeatSummary::from),
        }
    }
}

/// Format a secret token for display: first 4 + last 2 characters,
/// middle replaced by a fixed-width bullet run.  Short tokens are
/// fully redacted.
pub fn redact_token(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 8 {
        return "\u{2022}".repeat(chars.len().max(1));
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(2)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}\u{2022}\u{2022}\u{2022}\u{2022}{tail}")
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

pub fn render(
    ui: &mut egui::Ui,
    view: &StatusView,
    registration: &RegistrationStatus,
    register_form: &mut RegisterForm,
    on_register_clicked: &mut dyn FnMut(&RegisterForm),
) {
    match view {
        StatusView::Unregistered {
            api_base_url,
            bootstrap_token_preview,
        } => {
            render_unregistered(
                ui,
                api_base_url,
                bootstrap_token_preview,
                registration,
                register_form,
                on_register_clicked,
            );
        }
        StatusView::Registered { .. } => render_registered(ui, view),
    }
}

#[derive(Debug, Clone, Default)]
pub struct RegisterForm {
    pub api_base_url: String,
    pub bootstrap_token: String,
    pub bootstrap_token_visible: bool,
}

impl RegisterForm {
    /// Initialise the form fields from the resolved config so users
    /// see what's currently on disk instead of an empty box.
    pub fn seeded_from(cfg: &Config) -> Self {
        Self {
            api_base_url: cfg.api_base_url.clone(),
            bootstrap_token: cfg.bootstrap_token.clone(),
            bootstrap_token_visible: false,
        }
    }
}

fn render_unregistered(
    ui: &mut egui::Ui,
    _api_base_url: &str,
    _bootstrap_token_preview: &str,
    registration: &RegistrationStatus,
    form: &mut RegisterForm,
    on_register_clicked: &mut dyn FnMut(&RegisterForm),
) {
    ui.heading("Register this worker");
    ui.add_space(4.0);
    ui.label(
        "This worker hasn't registered with a studio yet.  Fill in the studio's \
         API base URL and your bootstrap token, then click Register.",
    );
    ui.add_space(12.0);

    egui::Grid::new("register_form")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label("API base URL");
            ui.add(
                egui::TextEdit::singleline(&mut form.api_base_url)
                    .desired_width(360.0)
                    .hint_text("https://studio.example.com"),
            );
            ui.end_row();

            ui.label("Bootstrap token");
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(&mut form.bootstrap_token)
                    .desired_width(280.0)
                    .password(!form.bootstrap_token_visible);
                ui.add(edit);
                ui.checkbox(&mut form.bootstrap_token_visible, "show");
            });
            ui.end_row();
        });

    ui.add_space(8.0);
    let can_submit = matches!(
        registration,
        RegistrationStatus::Idle | RegistrationStatus::Failed(_)
    ) && !form.api_base_url.trim().is_empty()
        && !form.bootstrap_token.trim().is_empty();
    if ui
        .add_enabled(can_submit, egui::Button::new("Register"))
        .clicked()
    {
        on_register_clicked(form);
    }

    ui.add_space(12.0);
    match registration {
        RegistrationStatus::Idle => {}
        RegistrationStatus::InFlight => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Registering\u{2026}");
            });
        }
        RegistrationStatus::Success => {
            ui.colored_label(egui::Color32::LIGHT_GREEN, "Registered.");
        }
        RegistrationStatus::Failed(reason) => {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!("Registration failed: {reason}"),
            );
        }
    }
}

fn render_registered(ui: &mut egui::Ui, view: &StatusView) {
    let StatusView::Registered {
        worker_id,
        api_base_url,
        engine,
        vram_total_gb,
        vram_threshold_gb,
        auto_enabled,
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
    } else if *auto_enabled {
        ("IDLE", egui::Color32::LIGHT_GREEN)
    } else {
        ("PAUSED", egui::Color32::LIGHT_GRAY)
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(badge.0).color(badge.1).strong());
        ui.label("\u{2014}");
        ui.label(if *busy {
            "running a job"
        } else if *auto_enabled {
            "waiting for work"
        } else {
            "claiming disabled"
        });
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

            ui.label("Engine");
            ui.monospace(engine);
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
            engine: "synthetic".into(),
            vram_threshold_gb: 12.0,
            auto_enabled: true,
            ..Config::default()
        }
    }

    #[test]
    fn build_unregistered_when_worker_id_missing() {
        let cfg = Config::default();
        let view = StatusView::build(&cfg, false, None, 0.0);
        assert!(matches!(view, StatusView::Unregistered { .. }));
    }

    #[test]
    fn build_unregistered_redacts_the_bootstrap_token_preview() {
        let cfg = Config {
            bootstrap_token: "abcd1234ef".into(),
            ..Config::default()
        };
        let view = StatusView::build(&cfg, false, None, 0.0);
        match view {
            StatusView::Unregistered {
                bootstrap_token_preview,
                ..
            } => {
                assert!(bootstrap_token_preview.starts_with("abcd"));
                assert!(bootstrap_token_preview.ends_with("ef"));
                assert!(bootstrap_token_preview.contains('\u{2022}'));
            }
            _ => panic!("expected Unregistered"),
        }
    }

    #[test]
    fn build_registered_when_worker_id_and_token_present() {
        let cfg = registered_cfg();
        let view = StatusView::build(&cfg, false, None, 24.0);
        match view {
            StatusView::Registered {
                worker_id,
                api_base_url,
                engine,
                vram_total_gb,
                vram_threshold_gb,
                auto_enabled,
                busy,
                last_heartbeat,
            } => {
                assert_eq!(worker_id, "w-abc");
                assert_eq!(api_base_url, "https://studio.example");
                assert_eq!(engine, "synthetic");
                assert!((vram_total_gb - 24.0).abs() < f32::EPSILON);
                assert!((vram_threshold_gb - 12.0).abs() < f32::EPSILON);
                assert!(auto_enabled);
                assert!(!busy);
                assert!(last_heartbeat.is_none());
            }
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
        let view = StatusView::build(&cfg, false, Some(&hb), 24.0);
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
        let view = StatusView::build(&cfg, true, Some(&hb), 24.0);
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
    fn redact_token_short_token_fully_redacted() {
        assert_eq!(redact_token("short"), "\u{2022}".repeat(5));
        assert_eq!(redact_token(""), "\u{2022}");
    }

    #[test]
    fn redact_token_preserves_head_and_tail() {
        let r = redact_token("abcdefghij");
        assert!(r.starts_with("abcd"));
        assert!(r.ends_with("ij"));
        assert_eq!(r.chars().filter(|c| *c == '\u{2022}').count(), 4);
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
}
