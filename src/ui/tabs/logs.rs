//! Logs tab — windowed view over the shared `Vec<LogEntry>` the runtime
//! loops are filling.  The buffer is drained periodically by
//! `log_shipper_tick`, so the UI sees a rolling window that's
//! re-filled at heartbeat / claim cadence.

use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;

use crate::types::LogEntry;

pub const LOGS_WINDOW: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter {
    pub level: LevelFilter,
    pub search: String,
    pub auto_scroll: bool,
}

impl Default for LogFilter {
    fn default() -> Self {
        Self {
            level: LevelFilter::All,
            search: String::new(),
            auto_scroll: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelFilter {
    All,
    Info,
    Warn,
    Error,
}

impl LevelFilter {
    pub fn label(self) -> &'static str {
        match self {
            LevelFilter::All => "all",
            LevelFilter::Info => "info",
            LevelFilter::Warn => "warn",
            LevelFilter::Error => "error",
        }
    }

    pub fn matches(self, entry_level: &str) -> bool {
        match self {
            LevelFilter::All => true,
            LevelFilter::Info => entry_level == "info",
            LevelFilter::Warn => entry_level == "warn",
            LevelFilter::Error => entry_level == "error",
        }
    }
}

/// Snapshot of the filtered log window the renderer iterates over.
#[derive(Debug, Clone, PartialEq)]
pub struct LogsView {
    pub entries: Vec<LogEntry>,
    /// True when the underlying buffer is longer than the window —
    /// surfaces "showing last N of M" hint in the UI.
    pub windowed: bool,
    pub total_buffer: usize,
}

impl LogsView {
    pub fn build(buffer: &[LogEntry], filter: &LogFilter, window: usize) -> Self {
        let needle = filter.search.trim().to_lowercase();
        let filtered: Vec<LogEntry> = buffer
            .iter()
            .filter(|e| filter.level.matches(&e.level))
            .filter(|e| {
                needle.is_empty()
                    || e.message.to_lowercase().contains(&needle)
                    || e.category.to_lowercase().contains(&needle)
                    || e.job_id
                        .as_deref()
                        .map(|j| j.to_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .cloned()
            .collect();
        let total_buffer = buffer.len();
        let windowed = filtered.len() > window;
        let entries = if windowed {
            filtered[filtered.len() - window..].to_vec()
        } else {
            filtered
        };
        Self {
            entries,
            windowed,
            total_buffer,
        }
    }
}

pub fn render(ui: &mut egui::Ui, buffer: &Arc<Mutex<Vec<LogEntry>>>, filter: &mut LogFilter) {
    ui.heading("Logs");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Level:");
        for level in [
            LevelFilter::All,
            LevelFilter::Info,
            LevelFilter::Warn,
            LevelFilter::Error,
        ] {
            ui.selectable_value(&mut filter.level, level, level.label());
        }
        ui.separator();
        ui.label("Search:");
        ui.add(
            egui::TextEdit::singleline(&mut filter.search)
                .desired_width(220.0)
                .hint_text("category / message / job id"),
        );
        ui.separator();
        ui.checkbox(&mut filter.auto_scroll, "auto-scroll");
    });
    ui.add_space(6.0);

    let view = {
        let buf = buffer.lock();
        LogsView::build(&buf, filter, LOGS_WINDOW)
    };

    if view.entries.is_empty() {
        ui.label(
            egui::RichText::new("No log entries match the current filter.")
                .italics()
                .color(egui::Color32::from_gray(150)),
        );
        return;
    }

    if view.windowed {
        ui.label(
            egui::RichText::new(format!(
                "showing last {} entries (buffer holds {} total before next ship)",
                view.entries.len(),
                view.total_buffer
            ))
            .italics()
            .color(egui::Color32::from_gray(150)),
        );
    }
    ui.add_space(4.0);

    let scroll = egui::ScrollArea::vertical()
        .max_height(f32::INFINITY)
        .stick_to_bottom(filter.auto_scroll);
    scroll.show_rows(ui, 18.0, view.entries.len(), |ui, range| {
        for entry in &view.entries[range] {
            render_entry(ui, entry);
        }
    });
}

fn render_entry(ui: &mut egui::Ui, e: &LogEntry) {
    let colour = match e.level.as_str() {
        "error" => egui::Color32::LIGHT_RED,
        "warn" => egui::Color32::from_rgb(232, 168, 56),
        _ => egui::Color32::from_gray(200),
    };
    ui.horizontal(|ui| {
        ui.monospace(
            egui::RichText::new(&e.ts)
                .color(egui::Color32::from_gray(120))
                .size(11.0),
        );
        ui.label(
            egui::RichText::new(format!("[{}]", e.level))
                .color(colour)
                .strong()
                .size(11.0),
        );
        ui.label(
            egui::RichText::new(format!("{}:", e.category))
                .color(egui::Color32::from_gray(170))
                .size(11.0),
        );
        ui.label(egui::RichText::new(&e.message).size(11.0));
        if let Some(j) = &e.job_id {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.monospace(
                    egui::RichText::new(j)
                        .color(egui::Color32::from_gray(120))
                        .size(11.0),
                );
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, category: &str, message: &str, job_id: Option<&str>) -> LogEntry {
        LogEntry {
            ts: "2026-05-25T10:00:00Z".into(),
            level: level.into(),
            category: category.into(),
            message: message.into(),
            job_id: job_id.map(str::to_string),
        }
    }

    #[test]
    fn build_with_default_filter_returns_everything() {
        let buf = vec![
            entry("info", "claim", "a", None),
            entry("warn", "heartbeat", "b", None),
        ];
        let view = LogsView::build(&buf, &LogFilter::default(), LOGS_WINDOW);
        assert_eq!(view.entries.len(), 2);
        assert!(!view.windowed);
    }

    #[test]
    fn level_filter_excludes_other_levels() {
        let buf = vec![
            entry("info", "x", "a", None),
            entry("warn", "x", "b", None),
            entry("error", "x", "c", None),
        ];
        let filter = LogFilter {
            level: LevelFilter::Error,
            ..LogFilter::default()
        };
        let view = LogsView::build(&buf, &filter, LOGS_WINDOW);
        assert_eq!(view.entries.len(), 1);
        assert_eq!(view.entries[0].level, "error");
    }

    #[test]
    fn search_matches_message_category_or_job_id_case_insensitive() {
        let buf = vec![
            entry("info", "claim", "Boom and bust", None),
            entry("info", "Boom", "noise", None),
            entry("info", "x", "y", Some("Boom-1")),
            entry("info", "z", "unrelated", None),
        ];
        let filter = LogFilter {
            search: "boom".into(),
            ..LogFilter::default()
        };
        let view = LogsView::build(&buf, &filter, LOGS_WINDOW);
        assert_eq!(view.entries.len(), 3);
    }

    #[test]
    fn windows_to_last_n_when_buffer_exceeds_cap() {
        let buf: Vec<LogEntry> = (0..1000)
            .map(|i| entry("info", "x", &format!("m{i}"), None))
            .collect();
        let view = LogsView::build(&buf, &LogFilter::default(), 500);
        assert_eq!(view.entries.len(), 500);
        assert!(view.windowed);
        assert_eq!(view.entries.last().unwrap().message, "m999");
        assert_eq!(view.entries.first().unwrap().message, "m500");
    }

    #[test]
    fn auto_scroll_default_is_on() {
        assert!(LogFilter::default().auto_scroll);
    }
}
