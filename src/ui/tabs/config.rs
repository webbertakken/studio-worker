//! Config tab — every field of `Config` reachable as an editable
//! widget.  Save writes through `crate::config::save`; the runtime
//! loops pick up the new values on their next tick because every
//! tick snapshots `Arc<Mutex<Config>>`.
//!
//! Fork #3 (engine selection) is kept as a yellow "restart required"
//! banner per the chosen default; we do not auto-restart the loops.

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::config::{self, Config};

use super::super::{autostart, notifier::NotificationPrefs};

/// Buffer the user is editing.  `dirty` is true when any field
/// differs from `original`; Save / Reset clear it.
#[derive(Debug, Clone)]
pub struct ConfigDraft {
    pub current: Config,
    pub original: Config,
    pub last_save_error: Option<String>,
    pub auth_token_visible: bool,
}

impl ConfigDraft {
    pub fn from(cfg: &Config) -> Self {
        Self {
            current: cfg.clone(),
            original: cfg.clone(),
            last_save_error: None,
            auth_token_visible: false,
        }
    }

    pub fn dirty(&self) -> bool {
        !configs_equal(&self.current, &self.original)
    }

    pub fn engine_changed(&self) -> bool {
        self.current.engine != self.original.engine
            || self.current.engines != self.original.engines
            || self.current.gradio_endpoint_url != self.original.gradio_endpoint_url
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
    }
}

/// Equality covering every persisted field.  We do this manually
/// because `Config` doesn't (and shouldn't) implement `PartialEq` —
/// the runtime is concerned with semantics, not byte-equality.
fn configs_equal(a: &Config, b: &Config) -> bool {
    a.api_base_url == b.api_base_url
        && a.worker_id == b.worker_id
        && a.label == b.label
        && a.auth_token == b.auth_token
        && (a.vram_threshold_gb - b.vram_threshold_gb).abs() < f32::EPSILON
        && a.auto_start == b.auto_start
        && a.auto_enabled == b.auto_enabled
        && a.engine == b.engine
        && a.engines == b.engines
        && a.gradio_endpoint_url == b.gradio_endpoint_url
        && a.supported_models_override == b.supported_models_override
        && a.auto_update_enabled == b.auto_update_enabled
        && a.auto_update_interval_secs == b.auto_update_interval_secs
        && a.auto_update_feed == b.auto_update_feed
        && a.auto_update_prerelease == b.auto_update_prerelease
        && a.models_root == b.models_root
}

const ENGINE_CHOICES: &[&str] = &[
    "synthetic",
    "gradio",
    "multi",
    "llama",
    "whisper",
    "image-candle",
    "video",
    "tts",
];

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

    if draft.engine_changed() {
        ui.colored_label(
            egui::Color32::from_rgb(232, 168, 56),
            "Engine fields changed — restart the worker for the new engine to take effect.",
        );
        ui.add_space(4.0);
    }

    section(ui, "Connection", |ui| {
        labeled_text(ui, "API base URL", &mut draft.current.api_base_url);
        labeled_optional(ui, "Label", &mut draft.current.label);
        labeled_optional(ui, "Worker ID", &mut draft.current.worker_id);
        labeled_optional_password(
            ui,
            "Auth token",
            &mut draft.current.auth_token,
            &mut draft.auth_token_visible,
        );
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
        labeled_bool(
            ui,
            "Auto-enabled (claim jobs)",
            &mut draft.current.auto_enabled,
        );
    });

    section(ui, "Engine", |ui| {
        labeled_combo(ui, "Engine", &mut draft.current.engine, ENGINE_CHOICES);
        labeled_csv(ui, "Engines (multi only)", &mut draft.current.engines);
        labeled_optional(
            ui,
            "Gradio endpoint URL",
            &mut draft.current.gradio_endpoint_url,
        );
        labeled_csv(
            ui,
            "Supported models override",
            &mut draft.current.supported_models_override,
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
        labeled_optional_path(ui, "Models root", &mut draft.current.models_root);
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
        if let Ok(exe) = std::env::current_exe() {
            if autostart_enabled {
                let _ = autostart::enable(&exe);
            } else {
                let _ = autostart::disable();
            }
        }
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

fn labeled_optional(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    ui.label(label);
    let mut buf = value.clone().unwrap_or_default();
    let r = ui.add(egui::TextEdit::singleline(&mut buf).desired_width(360.0));
    if r.changed() {
        *value = if buf.is_empty() { None } else { Some(buf) };
    }
    ui.end_row();
}

fn labeled_optional_password(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<String>,
    visible: &mut bool,
) {
    ui.label(label);
    let mut buf = value.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut buf)
                .desired_width(280.0)
                .password(!*visible),
        );
        if r.changed() {
            *value = if buf.is_empty() { None } else { Some(buf) };
        }
        ui.checkbox(visible, "show");
    });
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

fn labeled_combo(ui: &mut egui::Ui, label: &str, value: &mut String, choices: &[&str]) {
    ui.label(label);
    egui::ComboBox::from_id_salt(label)
        .selected_text(value.as_str())
        .show_ui(ui, |ui| {
            for choice in choices {
                ui.selectable_value(value, (*choice).into(), *choice);
            }
        });
    ui.end_row();
}

fn labeled_csv(ui: &mut egui::Ui, label: &str, value: &mut Vec<String>) {
    ui.label(label);
    let mut buf = value.join(", ");
    let r = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .desired_width(360.0)
            .hint_text("comma-separated"),
    );
    if r.changed() {
        *value = buf
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    ui.end_row();
}

fn labeled_optional_path(ui: &mut egui::Ui, label: &str, value: &mut Option<PathBuf>) {
    ui.label(label);
    let mut buf = value
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let r = ui.add(egui::TextEdit::singleline(&mut buf).desired_width(360.0));
    if r.changed() {
        *value = if buf.is_empty() {
            None
        } else {
            Some(PathBuf::from(buf))
        };
    }
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
        assert!(!draft.engine_changed());
    }

    #[test]
    fn draft_marks_dirty_after_edit() {
        let cfg = Config::default();
        let mut draft = ConfigDraft::from(&cfg);
        draft.current.vram_threshold_gb = 24.0;
        assert!(draft.dirty());
    }

    #[test]
    fn engine_changed_flips_only_for_engine_fields() {
        let cfg = Config::default();
        let mut draft = ConfigDraft::from(&cfg);
        draft.current.vram_threshold_gb = 24.0;
        assert!(!draft.engine_changed());
        draft.current.engine = "gradio".into();
        assert!(draft.engine_changed());
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
        draft.current.engine = "gradio".into();
        draft.reset();
        assert_eq!(draft.current.engine, "synthetic");
        assert!(!draft.dirty());
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
