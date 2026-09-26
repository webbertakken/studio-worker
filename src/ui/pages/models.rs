//! Models: what is in memory, what can be, and one action per model
//! (the daemon's `/models/:id/{load,unload}`).  Two groups that never
//! reorder, so a model stays where it is while it loads or unloads.

use chrono::{DateTime, Utc};
use eframe::egui::{self, vec2, Align, Color32, Layout, RichText};

use crate::daemon_api::ModelEntry;

use super::super::format::format_age;
use super::super::icons::{self, Icon};
use super::super::pulse::{format_gb, holds_memory, GpuMemory};
use super::super::theme::{Palette, Tone};
use super::super::widgets;

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAction {
    Load(String),
    Unload(String),
}

/// The one control a model row offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowControl {
    Load,
    /// Load again after a failure.
    Retry,
    /// Unload; also while loading (the load completes first).
    Unload,
    /// A transition under way: a disabled button saying so.
    InProgress(&'static str),
    /// Nothing to do here, and why.
    Unavailable(&'static str),
}

impl RowControl {
    /// The button's label.
    pub fn label(self) -> &'static str {
        match self {
            RowControl::Load => "Load",
            RowControl::Retry => "Retry",
            RowControl::Unload => "Unload",
            RowControl::InProgress(label) | RowControl::Unavailable(label) => label,
        }
    }

    /// The action a click sends for the model `id`.
    pub fn action(self, id: &str) -> Option<ModelAction> {
        match self {
            RowControl::Load | RowControl::Retry => Some(ModelAction::Load(id.to_string())),
            RowControl::Unload => Some(ModelAction::Unload(id.to_string())),
            RowControl::InProgress(_) | RowControl::Unavailable(_) => None,
        }
    }
}

/// The control the lifecycle allows from `state` (see
/// `docs/runtime/model-lifecycle.md`).
pub fn control_for(state: &str, enabled: bool, loadable: bool) -> RowControl {
    if !enabled {
        return RowControl::Unavailable("Disabled");
    }
    if !loadable {
        return RowControl::Unavailable("Per job");
    }
    match state {
        "unloaded" => RowControl::Load,
        "failed" => RowControl::Retry,
        "loaded" | "loading" => RowControl::Unload,
        "unloading" => RowControl::InProgress("Unloading\u{2026}"),
        _ => RowControl::Unavailable("Unknown"),
    }
}

/// The tone of a lifecycle state.
pub fn state_tone(state: &str) -> Tone {
    match state {
        "loaded" => Tone::Good,
        "loading" | "unloading" => Tone::Busy,
        "failed" => Tone::Bad,
        _ => Tone::Neutral,
    }
}

/// One model as the page shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRow {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub engine: String,
    pub vram_gb: f32,
    pub state: String,
    pub resident: bool,
    pub since: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub exclusive_group: Option<String>,
    pub control: RowControl,
}

impl ModelRow {
    pub fn from_entry(entry: &ModelEntry) -> Self {
        let engine = serde_json::to_value(&entry.source.engine)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        Self {
            id: entry.id.clone(),
            name: entry.display_name.clone(),
            kind: entry.kind.as_str(),
            engine,
            vram_gb: entry.vram_gb_estimate,
            state: entry.state.clone(),
            resident: entry.resident,
            since: entry.since,
            error: entry.error.clone(),
            exclusive_group: entry.exclusive_group.clone(),
            control: control_for(&entry.state, entry.enabled, entry.loadable),
        }
    }

    /// `llm · llama-cpp · ≈ 1.5 GB`.
    pub fn meta_line(&self) -> String {
        format!(
            "{} \u{00b7} {} \u{00b7} \u{2248} {} GB",
            self.kind,
            self.engine,
            format_gb(self.vram_gb)
        )
    }
}

/// The page, grouped.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelsView {
    /// Models with an in-process loader: they can stay in memory.
    pub kept: Vec<ModelRow>,
    /// Models whose engine loads for each job.
    pub per_job: Vec<ModelRow>,
    pub memory: GpuMemory,
    /// Each model holding memory: its name and estimate, in catalogue order.
    pub holders: Vec<(String, f32)>,
}

impl ModelsView {
    pub fn build(models: &[ModelEntry], vram_total_gb: f32) -> Self {
        let (kept, per_job) = models
            .iter()
            .partition::<Vec<&ModelEntry>, _>(|m| m.loadable);
        Self {
            kept: kept.into_iter().map(ModelRow::from_entry).collect(),
            per_job: per_job.into_iter().map(ModelRow::from_entry).collect(),
            memory: GpuMemory::from_models(models, vram_total_gb),
            holders: models
                .iter()
                .filter(|m| holds_memory(&m.state))
                .map(|m| (m.display_name.clone(), m.vram_gb_estimate.max(0.0)))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.kept.is_empty() && self.per_job.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Width of the state column and of the action button, in points.
const STATE_COLUMN: f32 = 118.0;
const ACTION_WIDTH: f32 = 104.0;

/// Draw the page; answers the action the operator took, if any.
pub fn render(ui: &mut egui::Ui, view: &ModelsView) -> Option<ModelAction> {
    widgets::page_title(
        ui,
        "Models",
        "Loaded models stay in memory and answer at once; resident ones come back after a \
         restart.  Unloading frees their memory.",
    );
    memory_card(ui, view);
    ui.add_space(16.0);
    if view.is_empty() {
        widgets::card(ui, |ui| {
            widgets::empty_state(
                ui,
                Icon::Models,
                "The catalogue is empty",
                "Models appear here once the daemon knows them.",
            );
        });
        return None;
    }
    let now = Utc::now();
    let mut action = None;
    for (title, rows) in [
        ("KEPT IN MEMORY", &view.kept),
        ("LOADED PER JOB", &view.per_job),
    ] {
        if rows.is_empty() {
            continue;
        }
        widgets::section_label(ui, &format!("{title} \u{00b7} {}", rows.len()));
        for row in rows {
            if let Some(clicked) = model_row(ui, row, now) {
                action = Some(clicked);
            }
            ui.add_space(8.0);
        }
        ui.add_space(10.0);
    }
    action
}

fn segment_colours(p: &Palette) -> [Color32; 4] {
    [p.info, p.good, p.accent, p.muted]
}

fn memory_card(ui: &mut egui::Ui, view: &ModelsView) {
    let p = Palette::of_ui(ui);
    widgets::card(ui, |ui| {
        ui.horizontal(|ui| {
            icons::show(ui, Icon::Models, 22.0, p.muted);
            ui.label(
                RichText::new(format!(
                    "{} GB held by loaded models",
                    format_gb(view.memory.held_gb)
                ))
                .size(17.0)
                .strong()
                .color(p.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let total = if view.memory.total_gb > 0.0 {
                    format!("of {} GB on the device", format_gb(view.memory.total_gb))
                } else {
                    "device total unknown".to_string()
                };
                ui.label(widgets::muted(ui, total));
            });
        });
        ui.add_space(8.0);
        let colours = segment_colours(p);
        let total = view
            .memory
            .total_gb
            .max(view.memory.held_gb)
            .max(f32::EPSILON);
        let segments: Vec<(f32, Color32)> = view
            .holders
            .iter()
            .enumerate()
            .map(|(i, (_, gb))| (gb / total, colours[i % colours.len()]))
            .collect();
        widgets::meter(ui, ui.available_width(), 10.0, &segments);
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if view.holders.is_empty() {
                ui.label(widgets::muted(
                    ui,
                    "Nothing loaded: the device is free for per-job engines.",
                ));
            }
            for (i, (name, gb)) in view.holders.iter().enumerate() {
                let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 4.0, colours[i % colours.len()]);
                ui.label(
                    RichText::new(format!("{name} \u{2248} {} GB", format_gb(*gb))).color(p.text),
                );
                ui.add_space(10.0);
            }
        });
        ui.label(
            widgets::muted(
                ui,
                "Estimates from the catalogue; per-job engines are not counted.",
            )
            .small(),
        );
    });
}

fn model_row(ui: &mut egui::Ui, row: &ModelRow, now: DateTime<Utc>) -> Option<ModelAction> {
    let p = Palette::of_ui(ui);
    let mut action = None;
    widgets::card(ui, |ui| {
        ui.horizontal_top(|ui| {
            // State column: a glance down it reads every model's state.
            ui.allocate_ui_with_layout(
                vec2(STATE_COLUMN, 44.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(STATE_COLUMN);
                    ui.horizontal(|ui| {
                        let tone = state_tone(&row.state);
                        widgets::status_dot(ui, tone, 0.0);
                        ui.label(RichText::new(&row.state).strong().color(p.tone(tone)));
                    });
                    if let Some(since) = row.since {
                        ui.label(
                            widgets::muted(ui, format!("since {}", format_age(now, since))).small(),
                        );
                    }
                },
            );
            let middle = (ui.available_width() - ACTION_WIDTH - 12.0).max(120.0);
            ui.allocate_ui_with_layout(vec2(middle, 44.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(middle);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&row.name).strong().color(p.text));
                    ui.label(RichText::new(&row.id).monospace().small().color(p.muted));
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(widgets::muted(ui, row.meta_line()));
                    if row.resident {
                        widgets::pill(ui, "resident", Tone::Info)
                            .on_hover_text("loaded again when the daemon restarts");
                    }
                    if let Some(group) = &row.exclusive_group {
                        widgets::pill(ui, &format!("one of {group}"), Tone::Neutral)
                            .on_hover_text("only one model of this group is loaded at a time");
                    }
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                // Quiet buttons: the state column carries the colour.
                let clicked = match row.control {
                    RowControl::Load | RowControl::Retry | RowControl::Unload => {
                        widgets::button(ui, row.control.label(), true, ACTION_WIDTH).clicked()
                    }
                    RowControl::InProgress(label) | RowControl::Unavailable(label) => {
                        let hint = if row.control == RowControl::Unavailable("Per job") {
                            "no in-process loader: this engine loads for each job"
                        } else {
                            "nothing to do right now"
                        };
                        widgets::button(ui, label, false, ACTION_WIDTH)
                            .on_disabled_hover_text(hint);
                        false
                    }
                };
                if clicked {
                    action = row.control.action(&row.id);
                }
            });
        });
        if let Some(error) = &row.error {
            ui.add_space(6.0);
            widgets::problem_box(ui, error);
        }
    });
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_api::ModelSourceBrief;
    use crate::types::{ModelEngine, TaskKind};

    fn entry(id: &str, state: &str, loadable: bool) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            display_name: format!("Model {id}"),
            kind: TaskKind::Llm,
            vram_gb_estimate: 1.5,
            source: ModelSourceBrief {
                engine: ModelEngine::LlamaCpp,
            },
            enabled: true,
            exclusive_group: Some("stt".into()),
            state: state.into(),
            resident: state == "loaded",
            since: Some(Utc::now()),
            error: (state == "failed").then(|| "out of memory".to_string()),
            loadable,
        }
    }

    #[test]
    fn the_control_follows_the_lifecycle() {
        assert_eq!(control_for("unloaded", true, true), RowControl::Load);
        assert_eq!(control_for("failed", true, true), RowControl::Retry);
        assert_eq!(control_for("loading", true, true), RowControl::Unload);
        assert_eq!(control_for("loaded", true, true), RowControl::Unload);
        assert_eq!(
            control_for("unloading", true, true),
            RowControl::InProgress("Unloading\u{2026}")
        );
        assert_eq!(
            control_for("unloaded", true, false),
            RowControl::Unavailable("Per job")
        );
        assert_eq!(
            control_for("unloaded", false, true),
            RowControl::Unavailable("Disabled")
        );
        assert_eq!(
            control_for("weird", true, true),
            RowControl::Unavailable("Unknown")
        );
    }

    #[test]
    fn a_control_sends_its_action() {
        assert_eq!(RowControl::Retry.label(), "Retry");
        assert_eq!(
            RowControl::Retry.action("m"),
            Some(ModelAction::Load("m".into()))
        );
        assert_eq!(
            RowControl::Unload.action("m"),
            Some(ModelAction::Unload("m".into()))
        );
        assert_eq!(RowControl::InProgress("x").action("m"), None);
        assert_eq!(RowControl::Unavailable("Per job").label(), "Per job");
    }

    #[test]
    fn states_have_tones() {
        assert_eq!(state_tone("loaded"), Tone::Good);
        assert_eq!(state_tone("loading"), Tone::Busy);
        assert_eq!(state_tone("unloading"), Tone::Busy);
        assert_eq!(state_tone("failed"), Tone::Bad);
        assert_eq!(state_tone("unloaded"), Tone::Neutral);
    }

    #[test]
    fn a_row_carries_what_the_operator_needs() {
        let row = ModelRow::from_entry(&entry("q", "failed", true));
        assert_eq!(
            row.meta_line(),
            "llm \u{00b7} llama-cpp \u{00b7} \u{2248} 1.5 GB"
        );
        assert_eq!(row.error.as_deref(), Some("out of memory"));
        assert_eq!(row.control, RowControl::Retry);
    }

    #[test]
    fn models_group_by_loader_in_catalogue_order_and_memory_adds_up() {
        let models = [
            entry("a", "loaded", true),
            entry("sd", "unloaded", false),
            entry("b", "unloaded", true),
            entry("c", "loading", true),
        ];
        let view = ModelsView::build(&models, 24.0);
        let ids = |rows: &[ModelRow]| rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&view.kept), ["a", "b", "c"]);
        assert_eq!(ids(&view.per_job), ["sd"]);
        assert_eq!(view.memory.held_gb, 3.0);
        assert_eq!(
            view.holders,
            [("Model a".to_string(), 1.5), ("Model c".to_string(), 1.5)]
        );
        assert!(!view.is_empty());
        assert!(ModelsView::build(&[], 0.0).is_empty());
    }

    #[test]
    fn every_state_draws_in_both_themes() {
        let models: Vec<ModelEntry> = ["unloaded", "loading", "loaded", "unloading", "failed"]
            .into_iter()
            .enumerate()
            .map(|(i, s)| entry(&format!("m{i}"), s, i % 2 == 0))
            .collect();
        for dark in [true, false] {
            egui::__run_test_ui(|ui| {
                ui.ctx()
                    .set_visuals(super::super::super::theme::visuals(Palette::of(dark)));
                assert_eq!(render(ui, &ModelsView::build(&models, 24.0)), None);
                assert_eq!(render(ui, &ModelsView::build(&[], 0.0)), None);
            });
        }
    }
}
