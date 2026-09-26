//! A log as the UI shows it: monospace, the level coloured, wrapping,
//! selectable and copyable.  The job detail pane and the Logs page share it.
//!
//! [`compose`] turns lines into one text plus the role of each span (pure,
//! tested); [`show`] lays that text out read-only with the spans coloured.

use std::ops::Range;

use chrono::{DateTime, TimeZone};
use eframe::egui::{self, text::LayoutJob, CornerRadius, Frame, Margin, TextFormat};

use crate::job_log::JobLogLine;
use crate::types::LogEntry;

use super::theme::{Palette, Tone, CONTROL_RADIUS};

/// One line of a log, normalised from a job log or the worker log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// `HH:MM:SS`, local time.
    pub time: String,
    pub level: String,
    /// Where it came from: a category or a shortened tracing target.
    pub source: String,
    pub message: String,
    pub job_id: Option<String>,
}

impl LogLine {
    /// A job log line, its time in `tz`.
    pub fn from_job_line<Tz: TimeZone>(line: &JobLogLine, tz: &Tz) -> Self
    where
        Tz::Offset: std::fmt::Display,
    {
        Self {
            time: line.ts.with_timezone(tz).format("%H:%M:%S").to_string(),
            level: line.level.clone(),
            source: short_target(&line.target).to_string(),
            message: line.message.clone(),
            job_id: None,
        }
    }

    /// A worker log entry, its time in `tz` when it parses.
    pub fn from_entry<Tz: TimeZone>(entry: &LogEntry, tz: &Tz) -> Self
    where
        Tz::Offset: std::fmt::Display,
    {
        Self {
            time: local_time(&entry.ts, tz),
            level: entry.level.clone(),
            source: entry.category.clone(),
            message: entry.message.clone(),
            job_id: entry.job_id.clone(),
        }
    }
}

/// A tracing target without the crate prefix: `engine::sdcpp`.
pub fn short_target(target: &str) -> &str {
    target.strip_prefix("studio_worker::").unwrap_or(target)
}

/// An RFC 3339 timestamp as local `HH:MM:SS`; anything else unchanged.
pub fn local_time<Tz: TimeZone>(ts: &str, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(tz).format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

/// The colour a level is shown in.
pub fn level_tone(level: &str) -> Tone {
    match level.to_ascii_lowercase().as_str() {
        "error" => Tone::Bad,
        "warn" | "warning" => Tone::Busy,
        "info" => Tone::Info,
        _ => Tone::Neutral,
    }
}

/// What a span of a composed log is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Time,
    Level(Tone),
    Source,
    Message,
    Job,
}

/// A log as one text and the role of each of its spans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composed {
    pub text: String,
    pub spans: Vec<(Range<usize>, Role)>,
}

/// Lay `lines` out as `time  LEVEL  source  message  · job`, one per line.
pub fn compose(lines: &[LogLine]) -> Composed {
    let mut out = Composed::default();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.text.push('\n');
        }
        push(&mut out, &line.time, Role::Time);
        out.text.push_str("  ");
        let level = format!("{:<5}", line.level.to_ascii_uppercase());
        push(&mut out, &level, Role::Level(level_tone(&line.level)));
        out.text.push_str("  ");
        if !line.source.is_empty() {
            push(&mut out, &line.source, Role::Source);
            out.text.push_str("  ");
        }
        push(&mut out, &line.message, Role::Message);
        if let Some(job) = &line.job_id {
            push(&mut out, &format!("  \u{00b7} {job}"), Role::Job);
        }
    }
    out
}

fn push(out: &mut Composed, text: &str, role: Role) {
    let start = out.text.len();
    out.text.push_str(text);
    out.spans.push((start..out.text.len(), role));
}

/// The coloured layout of `composed` for `text` (the same string).
fn layout_job(text: &str, composed: &Composed, p: &Palette, font: egui::FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    let format = |colour| TextFormat::simple(font.clone(), colour);
    let mut at = 0;
    for (range, role) in &composed.spans {
        if range.end > text.len() {
            break;
        }
        if range.start > at {
            job.append(&text[at..range.start], 0.0, format(p.text));
        }
        let colour = match role {
            Role::Time | Role::Source | Role::Job => p.muted,
            Role::Level(tone) => p.tone(*tone),
            Role::Message => p.text,
        };
        job.append(&text[range.clone()], 0.0, format(colour));
        at = range.end;
    }
    if at < text.len() {
        job.append(&text[at..], 0.0, format(p.text));
    }
    job
}

/// Show `composed` read-only: selectable, copyable, wrapping, in a sunken
/// panel of `height` points (the panel keeps its height whatever the log
/// holds).  `follow` keeps the newest line in view.
pub fn show(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    composed: &Composed,
    follow: bool,
    height: f32,
) {
    let p = Palette::of_ui(ui);
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    Frame::new()
        .fill(p.inset)
        .corner_radius(CornerRadius::same(CONTROL_RADIUS))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt(id_salt)
                .stick_to_bottom(follow)
                .auto_shrink([false, false])
                .min_scrolled_height(height)
                .max_height(height)
                .show(ui, |ui| {
                    let mut layouter =
                        |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
                            let mut job = layout_job(buf.as_str(), composed, p, font.clone());
                            job.wrap.max_width = wrap_width;
                            ui.ctx().fonts_mut(|f| f.layout_job(job))
                        };
                    let mut text = composed.text.as_str();
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .desired_rows(1)
                            .frame(egui::Frame::NONE)
                            .layouter(&mut layouter),
                    );
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn line(level: &str, message: &str, job: Option<&str>) -> LogLine {
        LogLine {
            time: "03:04:05".into(),
            level: level.into(),
            source: "engine".into(),
            message: message.into(),
            job_id: job.map(str::to_string),
        }
    }

    #[test]
    fn a_line_reads_time_level_source_message_and_job() {
        let composed = compose(&[line("warn", "slow download", Some("j-1"))]);
        assert_eq!(
            composed.text,
            "03:04:05  WARN   engine  slow download  \u{00b7} j-1"
        );
        let roles: Vec<Role> = composed.spans.iter().map(|(_, r)| *r).collect();
        assert_eq!(
            roles,
            [
                Role::Time,
                Role::Level(Tone::Busy),
                Role::Source,
                Role::Message,
                Role::Job
            ]
        );
        for (range, _) in &composed.spans {
            assert!(composed.text.get(range.clone()).is_some(), "on char bounds");
        }
    }

    #[test]
    fn lines_are_joined_without_a_trailing_newline_and_an_empty_log_is_empty() {
        let composed = compose(&[line("info", "a", None), line("error", "b", None)]);
        assert_eq!(composed.text.lines().count(), 2);
        assert!(!composed.text.ends_with('\n'));
        assert_eq!(compose(&[]), Composed::default());
    }

    #[test]
    fn levels_have_their_colours() {
        assert_eq!(level_tone("error"), Tone::Bad);
        assert_eq!(level_tone("WARN"), Tone::Busy);
        assert_eq!(level_tone("info"), Tone::Info);
        assert_eq!(level_tone("debug"), Tone::Neutral);
    }

    #[test]
    fn job_lines_and_entries_normalise_their_time_and_source() {
        let job_line = JobLogLine {
            ts: "2026-01-02T03:04:05Z".parse().unwrap(),
            level: "info".into(),
            target: "studio_worker::engine::sdcpp".into(),
            message: "loaded".into(),
        };
        let l = LogLine::from_job_line(&job_line, &Utc);
        assert_eq!(
            (l.time.as_str(), l.source.as_str()),
            ("03:04:05", "engine::sdcpp")
        );

        let entry = LogEntry {
            ts: "2026-01-02T03:04:05Z".into(),
            level: "warn".into(),
            category: "heartbeat".into(),
            message: "late".into(),
            job_id: Some("j-9".into()),
        };
        let l = LogLine::from_entry(&entry, &Utc);
        assert_eq!(l.time, "03:04:05");
        assert_eq!(l.job_id.as_deref(), Some("j-9"));
        assert_eq!(local_time("not a time", &Utc), "not a time");
        assert_eq!(short_target("other::x"), "other::x");
    }

    #[test]
    fn an_empty_source_is_left_out() {
        let mut l = line("info", "m", None);
        l.source.clear();
        assert_eq!(compose(&[l]).text, "03:04:05  INFO   m");
    }

    #[test]
    fn the_layout_colours_every_span_and_survives_a_stale_buffer() {
        let composed = compose(&[line("error", "boom", Some("j"))]);
        let font = egui::FontId::monospace(13.0);
        let job = layout_job(&composed.text, &composed, &Palette::DARK, font.clone());
        assert_eq!(job.text, composed.text);
        assert!(job
            .sections
            .iter()
            .any(|s| s.format.color == Palette::DARK.bad));
        // A buffer shorter than the spans still lays out what it has.
        let job = layout_job("03:04", &composed, &Palette::DARK, font);
        assert_eq!(job.text, "03:04");
    }

    #[test]
    fn the_log_shows_without_panicking() {
        let composed = compose(&[line("info", "hello", None)]);
        egui::__run_test_ui(|ui| show(ui, "log", &composed, true, 120.0));
    }
}
