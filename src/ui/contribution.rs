//! The Worker page's contribution card: how many studio jobs this worker
//! completed today and in total, per kind, so whoever hosts it sees what
//! it gives the studio.

use chrono::NaiveDate;
use eframe::egui::{self, RichText};

use crate::job_tally::{sum, TallySnapshot};
use crate::types::TaskKind;

use super::format::format_count;
use super::icons::{self, Icon};
use super::theme::Palette;
use super::widgets;

/// One kind of job the worker did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindRow {
    pub kind: TaskKind,
    pub label: &'static str,
    pub today: u64,
    pub total: u64,
}

/// The card, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributionView {
    pub today: u64,
    pub total: u64,
    pub since: Option<NaiveDate>,
    /// Kinds with at least one job, in [`TaskKind::ALL`] order so rows never
    /// move as counts change.
    pub rows: Vec<KindRow>,
}

impl ContributionView {
    pub fn build(tally: &TallySnapshot) -> Self {
        let rows = TaskKind::ALL
            .into_iter()
            .filter_map(|kind| {
                let count =
                    |counts: &crate::job_tally::KindCounts| counts.get(&kind).copied().unwrap_or(0);
                let (today, total) = (count(&tally.today), count(&tally.total));
                (today + total > 0).then_some(KindRow {
                    kind,
                    label: kind_label(kind),
                    today,
                    total,
                })
            })
            .collect();
        Self {
            today: sum(&tally.today),
            total: sum(&tally.total),
            since: tally.since,
            rows,
        }
    }

    /// `Counting since 12 May 2026.`, or nothing before the first job.
    pub fn since_line(&self) -> Option<String> {
        self.since
            .map(|d| format!("Counting since {}.", d.format("%-d %b %Y")))
    }
}

/// What a job of `kind` made, as a plural noun.
pub fn kind_label(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Image => "Images",
        TaskKind::Llm => "Text",
        TaskKind::AudioStt => "Transcriptions",
        TaskKind::AudioTts => "Speech",
        TaskKind::Video => "Videos",
        TaskKind::Mesh => "3D models",
        TaskKind::Vision => "Detections",
        TaskKind::WakeWord => "Wake words",
        TaskKind::ModelConvert => "Model conversions",
    }
}

/// Draw the card.
pub fn card(ui: &mut egui::Ui, view: &ContributionView) {
    let p = *Palette::of_ui(ui);
    widgets::card(ui, |ui| {
        widgets::section_label(ui, "CONTRIBUTION TO THE STUDIO");
        if view.total == 0 {
            widgets::empty_state(
                ui,
                Icon::Jobs,
                "No studio jobs yet",
                "Every job this worker completes for the studio counts here.",
            );
            return;
        }
        ui.horizontal(|ui| {
            figure(ui, view.today, "jobs today", p.accent);
            ui.add_space(40.0);
            figure(ui, view.total, "jobs in total", p.text);
        });
        ui.add_space(8.0);
        egui::Grid::new("contribution-kinds")
            .num_columns(3)
            .spacing([24.0, 6.0])
            .min_col_width(64.0)
            .show(ui, |ui| {
                ui.label(RichText::new("KIND").small().strong().color(p.muted));
                ui.label(RichText::new("TODAY").small().strong().color(p.muted));
                ui.label(RichText::new("TOTAL").small().strong().color(p.muted));
                ui.end_row();
                for row in &view.rows {
                    ui.horizontal(|ui| {
                        icons::show(ui, Icon::of_kind(row.kind), 16.0, p.muted);
                        ui.label(RichText::new(row.label).color(p.text));
                    });
                    let today_colour = if row.today == 0 { p.muted } else { p.accent };
                    ui.label(RichText::new(format_count(row.today)).color(today_colour));
                    ui.label(RichText::new(format_count(row.total)).color(p.text));
                    ui.end_row();
                }
            });
        if let Some(line) = view.since_line() {
            ui.add_space(8.0);
            ui.label(widgets::muted(
                ui,
                format!("{line}  Thank you for lending this machine to the studio."),
            ));
        }
    });
}

/// A large number over its caption.
fn figure(ui: &mut egui::Ui, value: u64, caption: &str, colour: egui::Color32) {
    ui.vertical(|ui| {
        ui.label(
            RichText::new(format_count(value))
                .size(28.0)
                .strong()
                .color(colour),
        );
        ui.label(widgets::muted(ui, caption));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tally(today: &[(TaskKind, u64)], total: &[(TaskKind, u64)]) -> TallySnapshot {
        TallySnapshot {
            today: today.iter().copied().collect(),
            total: total.iter().copied().collect(),
            since: NaiveDate::from_ymd_opt(2026, 5, 12),
        }
    }

    #[test]
    fn rows_list_the_kinds_done_in_a_stable_order() {
        let view = ContributionView::build(&tally(
            &[(TaskKind::Llm, 3)],
            &[
                (TaskKind::Video, 1),
                (TaskKind::Llm, 40),
                (TaskKind::Image, 1_200),
            ],
        ));
        assert_eq!(view.today, 3);
        assert_eq!(view.total, 1_241);
        let rows: Vec<_> = view
            .rows
            .iter()
            .map(|r| (r.label, r.today, r.total))
            .collect();
        assert_eq!(
            rows,
            [("Images", 0, 1_200), ("Text", 3, 40), ("Videos", 0, 1)]
        );
    }

    #[test]
    fn nothing_done_yet_has_no_rows_and_no_start_day() {
        let view = ContributionView::build(&TallySnapshot::default());
        assert_eq!(view.total, 0);
        assert!(view.rows.is_empty());
        assert_eq!(view.since_line(), None);
    }

    #[test]
    fn the_start_day_reads_as_a_date() {
        let view = ContributionView::build(&tally(&[], &[(TaskKind::Image, 1)]));
        assert_eq!(
            view.since_line().as_deref(),
            Some("Counting since 12 May 2026.")
        );
    }

    #[test]
    fn every_kind_has_a_label() {
        let labels: std::collections::BTreeSet<_> =
            TaskKind::ALL.into_iter().map(kind_label).collect();
        assert_eq!(labels.len(), TaskKind::ALL.len(), "labels are distinct");
    }

    #[test]
    fn the_card_draws_empty_and_full() {
        for view in [
            ContributionView::build(&TallySnapshot::default()),
            ContributionView::build(&tally(
                &[(TaskKind::Image, 2)],
                &TaskKind::ALL.map(|k| (k, 1_000)),
            )),
        ] {
            egui::__run_test_ui(|ui| card(ui, &view));
        }
    }
}
