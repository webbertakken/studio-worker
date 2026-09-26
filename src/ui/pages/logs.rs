//! Logs: a windowed view over the worker log ring the replica keeps in
//! `WorkerObservers::recent_logs`.  Separate from the shipping
//! queue (which is drained every WS tick); reading from the ring
//! means the display doesn't blank out between ships.

use std::collections::VecDeque;
use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;

use crate::types::LogEntry;

use super::super::icons::Icon;
use super::super::log_view::{self, LogLine};
use super::super::widgets;

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
    pub const ALL: [LevelFilter; 4] = [
        LevelFilter::All,
        LevelFilter::Info,
        LevelFilter::Warn,
        LevelFilter::Error,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LevelFilter::All => "All",
            LevelFilter::Info => "Info",
            LevelFilter::Warn => "Warn",
            LevelFilter::Error => "Error",
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
    /// One line saying what the page shows.
    pub fn summary(&self) -> String {
        if self.windowed {
            format!(
                "Showing the last {} matching entries of {} kept",
                self.entries.len(),
                self.total_buffer
            )
        } else {
            format!("{} of {} entries", self.entries.len(), self.total_buffer)
        }
    }

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

/// Draw the page: a toolbar, then the filtered log filling the page.
pub fn render(ui: &mut egui::Ui, buffer: &Arc<Mutex<VecDeque<LogEntry>>>, filter: &mut LogFilter) {
    widgets::page_title(
        ui,
        "Logs",
        "Everything the daemon logs at info and up, newest at the bottom.",
    );
    let view = {
        let buf = buffer.lock();
        // VecDeque does not slice; copy the (bounded) snapshot.
        let snapshot: Vec<LogEntry> = buf.iter().cloned().collect();
        LogsView::build(&snapshot, filter, LOGS_WINDOW)
    };
    let lines: Vec<LogLine> = view
        .entries
        .iter()
        .map(|e| LogLine::from_entry(e, &chrono::Local))
        .collect();
    let composed = log_view::compose(&lines);

    ui.horizontal(|ui| {
        for level in LevelFilter::ALL {
            ui.selectable_value(&mut filter.level, level, level.label());
        }
        ui.add_space(12.0);
        ui.add(
            egui::TextEdit::singleline(&mut filter.search)
                .desired_width(260.0)
                .hint_text("Search category, message or job id"),
        );
        ui.add_space(12.0);
        ui.checkbox(&mut filter.auto_scroll, "Follow")
            .on_hover_text("keep the newest line in view");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            widgets::copy_button(ui, "logs", "Copy", &composed.text);
        });
    });
    ui.add_space(4.0);
    // Always one line, so the log below never moves.
    ui.label(widgets::muted(ui, view.summary()).small());
    ui.add_space(6.0);
    let height = (ui.available_height() - 24.0).max(160.0);
    if view.entries.is_empty() {
        widgets::card(ui, |ui| {
            ui.set_min_height(height - 2.0 * f32::from(widgets::CARD_PADDING));
            widgets::empty_state(
                ui,
                Icon::Logs,
                "No log entries match",
                "Clear the search or pick another level.",
            );
        });
        return;
    }
    log_view::show(ui, "worker-log", &composed, filter.auto_scroll, height);
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
    fn the_summary_says_how_much_is_shown() {
        let buf: Vec<LogEntry> = (0..10)
            .map(|i| entry("info", "x", &format!("m{i}"), None))
            .collect();
        let view = LogsView::build(&buf, &LogFilter::default(), 4);
        assert_eq!(
            view.summary(),
            "Showing the last 4 matching entries of 10 kept"
        );
        let view = LogsView::build(&buf, &LogFilter::default(), LOGS_WINDOW);
        assert_eq!(view.summary(), "10 of 10 entries");
        let labels: Vec<_> = LevelFilter::ALL.iter().map(|l| l.label()).collect();
        assert_eq!(labels, ["All", "Info", "Warn", "Error"]);
    }

    #[test]
    fn the_page_draws_with_entries_and_without() {
        let ring = Arc::new(Mutex::new(VecDeque::from(vec![
            entry("warn", "heartbeat", "late", Some("j-1")),
            entry("error", "claim", "boom", None),
        ])));
        let mut filter = LogFilter::default();
        egui::__run_test_ui(|ui| render(ui, &ring, &mut filter));
        filter.search = "nothing matches this".into();
        egui::__run_test_ui(|ui| render(ui, &ring, &mut filter));
    }

    #[test]
    fn auto_scroll_default_is_on() {
        assert!(LogFilter::default().auto_scroll);
    }
}
