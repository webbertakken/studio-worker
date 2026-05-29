//! Config tab — user-editable subset of [`Config`] reachable as
//! widgets.  Save writes through `crate::config::save`; the runtime
//! loops pick up the new values on their next tick because every
//! tick snapshots `Arc<Mutex<Config>>`.
//!
//! Internal state (`worker_id`, `auth_token`, `install_id`,
//! `registration_*`) is deliberately not surfaced here.  The
//! auto-register flow owns it end-to-end.

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::config::{self, default_models_root, Config};

use super::super::notifier::NotificationPrefs;
use crate::autostart;

/// Buffer the user is editing.  `dirty` is true when any field
/// differs from `original`; Save / Reset clear it.
#[derive(Debug, Clone)]
pub struct ConfigDraft {
    pub current: Config,
    pub original: Config,
    pub last_save_error: Option<String>,
    /// Last autostart-toggle failure, surfaced next to the toggle so
    /// the operator sees why it did not stick (the checkbox otherwise
    /// silently reverts on the next frame because `is_enabled()`
    /// re-reads disk).
    pub autostart_error: Option<String>,
}

impl ConfigDraft {
    pub fn from(cfg: &Config) -> Self {
        Self {
            current: cfg.clone(),
            original: cfg.clone(),
            last_save_error: None,
            autostart_error: None,
        }
    }

    pub fn dirty(&self) -> bool {
        !configs_equal(&self.current, &self.original)
    }

    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        match config::save(&self.current, path) {
            Ok(()) => {
                self.original = self.current.clone();
                self.last_save_error = None;
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e}");
                self.last_save_error = Some(msg.clone());
                Err(msg)
            }
        }
    }

    pub fn reset(&mut self) {
        self.current = self.original.clone();
        self.last_save_error = None;
        self.autostart_error = None;
    }
}

/// Equality over the persisted, user-editable fields.  Internal state
/// (registration ids, auth token, worker id, install id) is excluded
/// because the UI never mutates it; the auto-register flow owns it.
fn configs_equal(a: &Config, b: &Config) -> bool {
    a.api_base_url == b.api_base_url
        && (a.vram_threshold_gb - b.vram_threshold_gb).abs() < f32::EPSILON
        && a.auto_start == b.auto_start
        && a.auto_update_enabled == b.auto_update_enabled
        && a.auto_update_interval_secs == b.auto_update_interval_secs
        && a.auto_update_feed == b.auto_update_feed
        && a.auto_update_prerelease == b.auto_update_prerelease
        && a.models_root == b.models_root
}

pub fn render(
    ui: &mut egui::Ui,
    draft: &mut ConfigDraft,
    config_path: &Path,
    notification_prefs: &mut NotificationPrefs,
) {
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
        labeled_bool(ui, "Auto-start on boot", &mut draft.current.auto_start);
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

    let mut autostart_enabled = autostart::is_enabled();
    let prev_autostart = autostart_enabled;
    section(ui, "Background mode", |ui| {
        ui.label("Run in tray on login");
        ui.checkbox(&mut autostart_enabled, "");
        ui.end_row();
    });
    if autostart_enabled != prev_autostart {
        let outcome = match std::env::current_exe() {
            Ok(exe) if autostart_enabled => autostart::enable(&exe),
            Ok(_) => autostart::disable(),
            Err(e) => Err(anyhow::anyhow!("cannot resolve current executable: {e}")),
        };
        // `autostart::enable`/`disable` already emit a structured
        // tracing event; surface any failure in the UI too so the
        // operator sees why the toggle did not stick instead of it
        // silently reverting on the next frame.
        draft.autostart_error = outcome.err().map(|e| format!("{e}"));
    }
    if let Some(err) = &draft.autostart_error {
        ui.colored_label(
            egui::Color32::LIGHT_RED,
            format!("could not change autostart: {err}"),
        );
    }

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let dirty = draft.dirty();
        let save = ui.add_enabled(dirty, egui::Button::new("Save"));
        if save.clicked() {
            let _ = draft.save(config_path);
        }
        if ui.add_enabled(dirty, egui::Button::new("Reset")).clicked() {
            draft.reset();
        }
        if let Some(err) = &draft.last_save_error {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("save failed: {err}"));
        } else if !dirty && draft.last_save_error.is_none() {
            ui.label(
                egui::RichText::new("up to date")
                    .italics()
                    .color(egui::Color32::from_gray(150)),
            );
        }
    });
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
    use tempfile::tempdir;

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
    fn save_writes_through_and_clears_dirty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config::default();
        let mut draft = ConfigDraft::from(&cfg);
        draft.current.vram_threshold_gb = 24.0;
        draft.save(&path).unwrap();
        assert!(!draft.dirty());
        // Reload from disk and confirm the value persisted.
        let (loaded, _) = config::load(Some(&path.to_string_lossy())).unwrap();
        assert!((loaded.vram_threshold_gb - 24.0).abs() < f32::EPSILON);
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
    fn reset_clears_autostart_error() {
        let cfg = Config::default();
        let mut draft = ConfigDraft::from(&cfg);
        draft.autostart_error = Some("boom".into());
        draft.reset();
        assert!(draft.autostart_error.is_none());
    }

    #[test]
    fn save_failure_records_last_save_error() {
        let cfg = Config::default();
        let mut draft = ConfigDraft::from(&cfg);
        // /proc is read-only on Linux — a write attempt fails.
        let bad = Path::new("/proc/this-should-fail/config.toml");
        let res = draft.save(bad);
        assert!(res.is_err());
        assert!(draft.last_save_error.is_some());
    }
}
