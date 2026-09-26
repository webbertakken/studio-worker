//! Models tab: every catalogue model with its lifecycle state, residency,
//! and Load / Unload controls (the daemon's `/models/:id/{load,unload}`).

use chrono::{DateTime, Utc};
use eframe::egui;

use crate::daemon_api::ModelEntry;

/// What the operator asked for on the Models tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAction {
    Load(String),
    Unload(String),
}

/// One row of the tab.
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
    pub enabled: bool,
    pub exclusive_group: Option<String>,
    pub can_load: bool,
    pub can_unload: bool,
}

impl ModelRow {
    pub fn from_entry(entry: &ModelEntry) -> Self {
        let (can_load, can_unload) = controls_for(&entry.state, entry.enabled);
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
            enabled: entry.enabled,
            exclusive_group: entry.exclusive_group.clone(),
            can_load,
            can_unload,
        }
    }
}

/// Which of Load / Unload the lifecycle allows from `state`
/// (see `docs/runtime/model-lifecycle.md`): load from unloaded or failed;
/// unload from loaded or loading (the load completes first).  A disabled
/// model cannot be loaded.
pub fn controls_for(state: &str, enabled: bool) -> (bool, bool) {
    let can_load = enabled && matches!(state, "unloaded" | "failed");
    let can_unload = matches!(state, "loaded" | "loading");
    (can_load, can_unload)
}

fn state_colour(state: &str) -> egui::Color32 {
    match state {
        "loaded" => egui::Color32::LIGHT_GREEN,
        "loading" | "unloading" => egui::Color32::from_rgb(232, 168, 56),
        "failed" => egui::Color32::LIGHT_RED,
        _ => egui::Color32::from_gray(170),
    }
}

/// Draw the tab; answers the action the operator took, if any.
pub fn render(ui: &mut egui::Ui, rows: &[ModelRow]) -> Option<ModelAction> {
    ui.heading(format!("Models ({})", rows.len()));
    ui.label(
        egui::RichText::new(
            "Loaded models stay in memory and answer at once; residency brings them back \
             after a restart.  Unloading frees their memory.",
        )
        .italics()
        .color(egui::Color32::from_gray(170)),
    );
    ui.add_space(8.0);
    if rows.is_empty() {
        ui.label(egui::RichText::new("The catalogue is empty.").italics());
        return None;
    }
    let mut action = None;
    egui::Grid::new("models_grid")
        .num_columns(7)
        .striped(true)
        .spacing([14.0, 8.0])
        .show(ui, |ui| {
            for header in ["Model", "Kind", "Engine", "Memory", "State", "Since", ""] {
                ui.label(egui::RichText::new(header).strong());
            }
            ui.end_row();
            let now = Utc::now();
            for row in rows {
                ui.vertical(|ui| {
                    ui.label(&row.name);
                    if row.name != row.id {
                        ui.label(
                            egui::RichText::new(&row.id)
                                .small()
                                .color(egui::Color32::from_gray(150)),
                        );
                    }
                    if let Some(group) = &row.exclusive_group {
                        ui.label(
                            egui::RichText::new(format!("one of group {group}"))
                                .small()
                                .color(egui::Color32::from_gray(150)),
                        );
                    }
                });
                ui.monospace(row.kind);
                ui.monospace(&row.engine);
                ui.label(format!("{:.1} GB", row.vram_gb));
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&row.state)
                                .color(state_colour(&row.state))
                                .strong(),
                        );
                        if row.resident {
                            ui.label(
                                egui::RichText::new("resident")
                                    .small()
                                    .color(egui::Color32::from_rgb(140, 180, 230)),
                            )
                            .on_hover_text("loaded again when the daemon restarts");
                        }
                        if !row.enabled {
                            ui.label(egui::RichText::new("disabled").small());
                        }
                    });
                    if let Some(error) = &row.error {
                        ui.colored_label(egui::Color32::from_rgb(230, 140, 130), error);
                    }
                });
                match row.since {
                    Some(since) => ui.label(super::status::format_age(now, since)),
                    None => ui.label("\u{2014}"),
                };
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(row.can_load, egui::Button::new("Load"))
                        .clicked()
                    {
                        action = Some(ModelAction::Load(row.id.clone()));
                    }
                    if ui
                        .add_enabled(row.can_unload, egui::Button::new("Unload"))
                        .clicked()
                    {
                        action = Some(ModelAction::Unload(row.id.clone()));
                    }
                });
                ui.end_row();
            }
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_api::ModelSourceBrief;
    use crate::types::{ModelEngine, TaskKind};

    fn entry(state: &str, enabled: bool) -> ModelEntry {
        ModelEntry {
            id: "qwen3.5-0.8b".into(),
            display_name: "Qwen3.5 0.8B".into(),
            kind: TaskKind::Llm,
            vram_gb_estimate: 1.5,
            source: ModelSourceBrief {
                engine: ModelEngine::LlamaCpp,
            },
            enabled,
            exclusive_group: None,
            state: state.into(),
            resident: state == "loaded",
            since: Some(Utc::now()),
            error: (state == "failed").then(|| "out of memory".to_string()),
        }
    }

    #[test]
    fn controls_follow_the_lifecycle_guards() {
        assert_eq!(controls_for("unloaded", true), (true, false));
        assert_eq!(controls_for("failed", true), (true, false));
        assert_eq!(controls_for("loading", true), (false, true));
        assert_eq!(controls_for("loaded", true), (false, true));
        assert_eq!(controls_for("unloading", true), (false, false));
        assert_eq!(controls_for("unloaded", false), (false, false));
    }

    #[test]
    fn a_row_carries_what_the_operator_needs() {
        let row = ModelRow::from_entry(&entry("failed", true));
        assert_eq!(row.kind, "llm");
        assert_eq!(row.engine, "llama-cpp");
        assert_eq!(row.error.as_deref(), Some("out of memory"));
        assert!(row.can_load && !row.can_unload);
    }

    #[test]
    fn render_draws_every_state_without_panicking() {
        let rows: Vec<ModelRow> = ["unloaded", "loading", "loaded", "unloading", "failed"]
            .into_iter()
            .map(|s| ModelRow::from_entry(&entry(s, true)))
            .collect();
        egui::__run_test_ui(|ui| {
            assert_eq!(render(ui, &rows), None);
            assert_eq!(render(ui, &[]), None);
        });
    }
}
