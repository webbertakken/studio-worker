//! Config: the operator-editable subset of [`Config`] as widgets.
//! The daemon owns the config: Save sends the edit to it
//! (`PUT /daemon/config`), which validates, saves and applies it; the
//! answer comes back through [`ConfigDraft::saved`] or
//! [`ConfigDraft::save_failed`].  The window's own preferences (theme,
//! reduce motion, notifications) sit on the same page but are applied and
//! stored at once (see `ui::prefs`).
//!
//! Internal state (`worker_id`, `auth_token`, `install_id`,
//! `registration_*`) is deliberately not surfaced here.  The
//! auto-register flow owns it end-to-end.

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::config::{self, default_models_root, Config};

use super::super::prefs::UiPrefs;
use super::super::theme::{Palette, ThemeChoice, Tone};
use super::super::widgets;

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

/// What the save footer says about the draft, and its tone.
pub fn save_state(draft: &ConfigDraft) -> (String, Tone) {
    if draft.pending {
        ("Saving\u{2026}".into(), Tone::Busy)
    } else if let Some(err) = &draft.last_save_error {
        (format!("Not saved: {err}"), Tone::Bad)
    } else if draft.dirty() {
        ("Unsaved changes".into(), Tone::Busy)
    } else {
        ("Up to date".into(), Tone::Neutral)
    }
}

/// What the operator did on the page.
#[derive(Debug, Default)]
pub struct ConfigOutcome {
    /// The config to send to the daemon: Save was pressed.
    pub save: Option<Config>,
    /// A window preference changed: apply and store it.
    pub prefs_changed: bool,
}

/// Draw the page.
pub fn render(
    ui: &mut egui::Ui,
    draft: &mut ConfigDraft,
    config_path: &Path,
    prefs: &mut UiPrefs,
) -> ConfigOutcome {
    let mut outcome = ConfigOutcome::default();
    egui::Panel::bottom("config-footer")
        .frame(
            egui::Frame::new()
                .fill(Palette::of_ui(ui).page)
                .inner_margin(egui::Margin::symmetric(0, 10)),
        )
        .exact_size(FOOTER_HEIGHT)
        .show_inside(ui, |ui| {
            outcome.save = footer(ui, draft);
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show_inside(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("config")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    widgets::page_title(
                        ui,
                        "Config",
                        &format!(
                            "The daemon keeps these in {}; Save sends them to it, and it \
                             checks, saves and applies them.",
                            config_path.display()
                        ),
                    );
                    worker_sections(ui, draft);
                    ui.add_space(12.0);
                    outcome.prefs_changed = window_section(ui, prefs);
                    ui.add_space(12.0);
                });
        });
    outcome
}

/// Height of the save footer, in points: the same whatever it says.
pub const FOOTER_HEIGHT: f32 = 52.0;

fn footer(ui: &mut egui::Ui, draft: &mut ConfigDraft) -> Option<Config> {
    let p = Palette::of_ui(ui);
    let mut save = None;
    ui.horizontal(|ui| {
        let dirty = draft.dirty();
        if widgets::primary_button(ui, "Save", dirty && !draft.pending, 96.0).clicked() {
            draft.pending = true;
            save = Some(draft.current.clone());
        }
        if widgets::button(ui, "Reset", dirty, 96.0)
            .on_hover_text("go back to what the daemon has")
            .clicked()
        {
            draft.reset();
        }
        ui.add_space(8.0);
        let (text, tone) = save_state(draft);
        if draft.pending {
            ui.spinner();
        }
        ui.add(egui::Label::new(egui::RichText::new(text).color(p.tone(tone))).truncate());
    });
    save
}

fn worker_sections(ui: &mut egui::Ui, draft: &mut ConfigDraft) {
    let c = &mut draft.current;
    section(ui, "CONNECTION", "", |ui| {
        labeled_text(ui, "Studio API base URL", &mut c.api_base_url);
    });
    section(ui, "WORKER", "", |ui| {
        labeled_slider(
            ui,
            "VRAM threshold (GB)",
            &mut c.vram_threshold_gb,
            0.0,
            96.0,
        );
        hint_row(ui, "The most device memory one studio job may ask for.");
    });
    section(ui, "AUTO-UPDATE", "", |ui| {
        labeled_bool(ui, "Update automatically", &mut c.auto_update_enabled);
        labeled_u64(
            ui,
            "Check every (seconds)",
            &mut c.auto_update_interval_secs,
        );
        labeled_text(ui, "Release feed URL", &mut c.auto_update_feed);
        labeled_bool(ui, "Track pre-releases", &mut c.auto_update_prerelease);
    });
    section(ui, "MODELS", "", |ui| {
        labeled_folder(ui, "Models folder", &mut c.models_root);
        hint_row(
            ui,
            "Where model weights are stored.  Serving many kinds of jobs takes a fair bit of \
             disk space.  A new folder applies after a restart.",
        );
    });
    section(ui, "START-UP", "", |ui| {
        labeled_bool(ui, "Start with my machine", &mut c.auto_start);
        labeled_bool(ui, "Start the window minimised", &mut c.start_minimised);
        labeled_bool(
            ui,
            "Only utilise when I'm not using this computer",
            &mut c.only_when_idle,
        );
        hint_row(
            ui,
            "Experimental.  Takes studio jobs only after 2 minutes without keyboard or mouse \
             input; a job that is running carries on.",
        );
    });
}

/// The window's own preferences, applied and stored at once.
fn window_section(ui: &mut egui::Ui, prefs: &mut UiPrefs) -> bool {
    let before = *prefs;
    section(
        ui,
        "THIS WINDOW",
        "Applied and stored at once, for this window only.",
        |ui| {
            ui.label("Theme");
            ui.horizontal(|ui| {
                for choice in ThemeChoice::ALL {
                    ui.selectable_value(&mut prefs.theme, choice, choice.label());
                }
            });
            ui.end_row();
            labeled_bool(ui, "Reduce motion", &mut prefs.reduce_motion);
            hint_row(ui, "Hold the glow of running work steady.");
            labeled_bool(
                ui,
                "Notify when a job completes",
                &mut prefs.notify_on_completion,
            );
            labeled_bool(ui, "Notify when a job fails", &mut prefs.notify_on_failure);
        },
    );
    *prefs != before
}

// ---------------------------------------------------------------------------
// Widget helpers
// ---------------------------------------------------------------------------

fn section(ui: &mut egui::Ui, title: &str, note: &str, add: impl FnOnce(&mut egui::Ui)) {
    widgets::card(ui, |ui| {
        widgets::section_label(ui, title);
        if !note.is_empty() {
            ui.label(widgets::muted(ui, note));
            ui.add_space(4.0);
        }
        egui::Grid::new(title)
            .num_columns(2)
            .spacing([20.0, 10.0])
            .min_col_width(200.0)
            .show(ui, |ui| {
                add(ui);
            });
    });
    ui.add_space(12.0);
}

fn hint_row(ui: &mut egui::Ui, text: &str) {
    ui.label("");
    let hint = widgets::muted(ui, text);
    ui.add(egui::Label::new(hint).wrap());
    ui.end_row();
}

fn labeled_text(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(380.0));
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

/// A path with a folder picker: the text edit shows the value; Browse
/// opens the native picker (rfd) and replaces it on confirm.
// The picker is a native dialog: nothing to drive in a headless test.
#[cfg_attr(coverage_nightly, coverage(off))]
fn labeled_folder(ui: &mut egui::Ui, label: &str, value: &mut PathBuf) {
    ui.label(label);
    ui.horizontal(|ui| {
        let mut buf = value.to_string_lossy().to_string();
        let r = ui.add(egui::TextEdit::singleline(&mut buf).desired_width(300.0));
        if r.changed() {
            *value = PathBuf::from(buf);
        }
        if ui.button("Browse\u{2026}").clicked() {
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

    #[test]
    fn the_footer_says_where_the_draft_stands() {
        let mut draft = ConfigDraft::from(&Config::default());
        assert_eq!(save_state(&draft), ("Up to date".into(), Tone::Neutral));
        draft.current.vram_threshold_gb = 3.0;
        assert_eq!(save_state(&draft), ("Unsaved changes".into(), Tone::Busy));
        draft.pending = true;
        assert_eq!(save_state(&draft).1, Tone::Busy);
        draft.save_failed("vramThresholdGb: too big".into());
        assert_eq!(
            save_state(&draft),
            ("Not saved: vramThresholdGb: too big".into(), Tone::Bad)
        );
    }

    #[test]
    fn the_page_draws_clean_dirty_and_pending() {
        let mut draft = ConfigDraft::from(&Config::default());
        let mut prefs = UiPrefs::default();
        for step in 0..3 {
            match step {
                1 => draft.current.vram_threshold_gb = 3.0,
                2 => draft.pending = true,
                _ => {}
            }
            egui::__run_test_ui(|ui| {
                let outcome = render(ui, &mut draft, Path::new("/tmp/c.toml"), &mut prefs);
                assert!(outcome.save.is_none() && !outcome.prefs_changed);
            });
        }
    }
}
