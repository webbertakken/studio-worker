//! Config tab — the operator-editable subset of [`Config`] as widgets.
//! The daemon owns the config: Save sends the edit to it
//! (`PUT /daemon/config`), which validates, saves and applies it; the
//! answer comes back through [`ConfigDraft::saved`] or
//! [`ConfigDraft::save_failed`].
//!
//! Internal state (`worker_id`, `auth_token`, `install_id`,
//! `registration_*`) is deliberately not surfaced here.  The
//! auto-register flow owns it end-to-end.

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::config::{self, default_models_root, Config};

use super::super::notifier::NotificationPrefs;

/// Buffer the user is editing.  `dirty` is true when any field
/// differs from `original`; Save / Reset clear it.
#[derive(Debug, Clone)]
pub struct ConfigDraft {
    pub current: Config,
    pub original: Config,
    pub last_save_error: Option<String>,
    /// A save is on its way to the daemon.
    pub pending: bool,
}

impl ConfigDraft {
    pub fn from(cfg: &Config) -> Self {
        Self {
            current: cfg.clone(),
            original: cfg.clone(),
            last_save_error: None,
            pending: false,
        }
    }

    pub fn dirty(&self) -> bool {
        !configs_equal(&self.current, &self.original)
    }

    /// Follow the daemon's config while the operator is not editing, so
    /// the tab never shows values the daemon no longer has.
    pub fn follow(&mut self, live: &Config) {
        if !self.dirty() && !self.pending && !configs_equal(&self.original, live) {
            *self = Self::from(live);
        }
    }

    /// The daemon saved `saved`: it is the new baseline.
    pub fn saved(&mut self, saved: &Config) {
        let changed = config::changed_fields(&self.original, saved).join(",");
        tracing::info!(
            target: "studio_worker::ui::config",
            changed = ?changed,
            "operator applied config changes via UI"
        );
        self.original = saved.clone();
        self.current = saved.clone();
        self.last_save_error = None;
        self.pending = false;
    }

    /// The daemon refused the edit or could not be reached; keep it.
    pub fn save_failed(&mut self, error: String) {
        tracing::warn!(
            target: "studio_worker::ui::config",
            error = %error,
            "config changes not applied"
        );
        self.last_save_error = Some(error);
        self.pending = false;
    }

    pub fn reset(&mut self) {
        self.current = self.original.clone();
        self.last_save_error = None;
    }
}

/// Equality over the operator-editable fields (see
/// [`config::changed_fields`]).
fn configs_equal(a: &Config, b: &Config) -> bool {
    config::changed_fields(a, b).is_empty()
}

/// Draw the tab; answers the config to send to the daemon when the
/// operator pressed Save.
pub fn render(
    ui: &mut egui::Ui,
    draft: &mut ConfigDraft,
    config_path: &Path,
    notification_prefs: &mut NotificationPrefs,
) -> Option<Config> {
    let mut save_requested = None;
    ui.heading("Configuration");
    ui.label(
        egui::RichText::new(format!("{}", config_path.display()))
            .color(egui::Color32::from_gray(150))
            .small(),
    );
    ui.add_space(8.0);

    section(ui, "Connection", |ui| {
        labeled_text(ui, "API base URL", &mut draft.current.api_base_url);
    });

    section(ui, "Worker", |ui| {
        labeled_slider(
            ui,
            "VRAM threshold (GB)",
            &mut draft.current.vram_threshold_gb,
            0.0,
            96.0,
        );
    });

    section(ui, "Auto-update", |ui| {
        labeled_bool(
            ui,
            "Auto-update enabled",
            &mut draft.current.auto_update_enabled,
        );
        labeled_u64(
            ui,
            "Interval (seconds)",
            &mut draft.current.auto_update_interval_secs,
        );
        labeled_text(ui, "Release feed URL", &mut draft.current.auto_update_feed);
        labeled_bool(
            ui,
            "Track pre-releases",
            &mut draft.current.auto_update_prerelease,
        );
    });

    section(ui, "Models", |ui| {
        labeled_folder(ui, "Models root", &mut draft.current.models_root);
        ui.label("");
        ui.label(
            egui::RichText::new(
                "This is where the models will be stored.  You might need a fair bit \
                 of disk space to be able to satisfy different types of jobs.",
            )
            .italics()
            .color(egui::Color32::from_gray(160)),
        );
        ui.end_row();
    });

    section(ui, "Notifications", |ui| {
        ui.label("On job completion");
        ui.checkbox(&mut notification_prefs.on_completion, "");
        ui.end_row();
        ui.label("On job failure");
        ui.checkbox(&mut notification_prefs.on_failure, "");
        ui.end_row();
    });

    section(ui, "Window", |ui| {
        ui.label("Start minimised");
        ui.checkbox(&mut draft.current.start_minimised, "");
        ui.end_row();
    });

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let dirty = draft.dirty();
        let save = ui.add_enabled(dirty && !draft.pending, egui::Button::new("Save"));
        if save.clicked() {
            draft.pending = true;
            save_requested = Some(draft.current.clone());
        }
        if ui.add_enabled(dirty, egui::Button::new("Reset")).clicked() {
            draft.reset();
        }
        if draft.pending {
            ui.spinner();
            ui.label("saving\u{2026}");
        } else if let Some(err) = &draft.last_save_error {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("save failed: {err}"));
        } else if !dirty && draft.last_save_error.is_none() {
            ui.label(
                egui::RichText::new("up to date")
                    .italics()
                    .color(egui::Color32::from_gray(150)),
            );
        }
    });
    save_requested
}

// ---------------------------------------------------------------------------
// Widget helpers
// ---------------------------------------------------------------------------

fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(title)
        .default_open(true)
        .show(ui, |ui| {
            egui::Grid::new(title)
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    add(ui);
                });
        });
    ui.add_space(4.0);
}

fn labeled_text(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(360.0));
    ui.end_row();
}

fn labeled_bool(ui: &mut egui::Ui, label: &str, value: &mut bool) {
    ui.label(label);
    ui.checkbox(value, "");
    ui.end_row();
}

fn labeled_slider(ui: &mut egui::Ui, label: &str, value: &mut f32, min: f32, max: f32) {
    ui.label(label);
    ui.add(egui::Slider::new(value, min..=max).fixed_decimals(1));
    ui.end_row();
}

fn labeled_u64(ui: &mut egui::Ui, label: &str, value: &mut u64) {
    ui.label(label);
    let mut buf = value.to_string();
    if ui
        .add(egui::TextEdit::singleline(&mut buf).desired_width(120.0))
        .changed()
    {
        if let Ok(n) = buf.parse::<u64>() {
            *value = n;
        }
    }
    ui.end_row();
}

/// Path-with-folder-picker widget.  The text edit reflects the
/// current value at all times; the "Browse…" button opens the
/// native picker (rfd) and overwrites it on confirm.
fn labeled_folder(ui: &mut egui::Ui, label: &str, value: &mut PathBuf) {
    ui.label(label);
    ui.horizontal(|ui| {
        let mut buf = value.to_string_lossy().to_string();
        let r = ui.add(egui::TextEdit::singleline(&mut buf).desired_width(280.0));
        if r.changed() {
            *value = PathBuf::from(buf);
        }
        if ui.button("Browse…").clicked() {
            let starting = if value.is_absolute() {
                value.clone()
            } else {
                default_models_root()
            };
            if let Some(picked) = rfd::FileDialog::new()
                .set_directory(starting.parent().unwrap_or(&starting))
                .pick_folder()
            {
                *value = picked;
            }
        }
    });
    ui.end_row();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_starts_clean() {
        let cfg = Config::default();
        let draft = ConfigDraft::from(&cfg);
        assert!(!draft.dirty());
    }

    #[test]
    fn draft_marks_dirty_after_edit() {
        let cfg = Config::default();
        let mut draft = ConfigDraft::from(&cfg);
        draft.current.vram_threshold_gb = 24.0;
        assert!(draft.dirty());
    }

    #[test]
    fn draft_marks_dirty_when_models_root_changes() {
        let cfg = Config::default();
        let mut draft = ConfigDraft::from(&cfg);
        draft.current.models_root = PathBuf::from("/tmp/other-models");
        assert!(draft.dirty());
    }

    #[test]
    fn saved_makes_the_answer_the_new_baseline() {
        let mut draft = ConfigDraft::from(&Config::default());
        draft.current.vram_threshold_gb = 24.0;
        draft.pending = true;
        let mut answer = draft.current.clone();
        answer.vram_threshold_gb = 23.5;
        draft.saved(&answer);
        assert!(!draft.dirty() && !draft.pending);
        assert_eq!(draft.current.vram_threshold_gb, 23.5);
    }

    #[test]
    fn a_clean_draft_follows_the_daemon_but_an_edit_is_kept() {
        let mut live = Config::default();
        let mut draft = ConfigDraft::from(&live);
        live.vram_threshold_gb = 5.0;
        draft.follow(&live);
        assert_eq!(draft.current.vram_threshold_gb, 5.0);

        draft.current.vram_threshold_gb = 7.0;
        live.vram_threshold_gb = 6.0;
        draft.follow(&live);
        assert_eq!(draft.current.vram_threshold_gb, 7.0, "edits survive a poll");
    }

    #[test]
    fn reset_reverts_unsaved_edits() {
        let cfg = Config::default();
        let mut draft = ConfigDraft::from(&cfg);
        draft.current.vram_threshold_gb = 33.0;
        draft.reset();
        assert!((draft.current.vram_threshold_gb - cfg.vram_threshold_gb).abs() < f32::EPSILON);
        assert!(!draft.dirty());
    }

    #[test]
    fn saved_emits_operator_apply_breadcrumb() {
        use crate::test_support::capture;
        let logs = capture(move || {
            let mut draft = ConfigDraft::from(&Config::default());
            draft.current.vram_threshold_gb = 24.0;
            let answer = draft.current.clone();
            draft.saved(&answer);
        });
        assert!(logs.contains("studio_worker::ui::config"), "{logs}");
        assert!(logs.contains("changed=\"vram_threshold_gb\""), "{logs}");
        assert!(
            logs.contains("operator applied config changes via UI"),
            "{logs}"
        );
    }

    #[test]
    fn save_failed_keeps_the_edit_and_the_error() {
        let mut draft = ConfigDraft::from(&Config::default());
        draft.current.vram_threshold_gb = 3.0;
        draft.pending = true;
        draft.save_failed("invalid config".into());
        assert!(draft.dirty() && !draft.pending);
        assert_eq!(draft.last_save_error.as_deref(), Some("invalid config"));
    }
}
