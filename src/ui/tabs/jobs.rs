//! Jobs tab: every running job, recent studio jobs and the local queue.
//! A card shows its image thumbnail when it has one; selecting a card shows
//! the job's log.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eframe::egui;

use crate::job_log::JobLog;
use crate::runtime::{CurrentJob, JobOutcome, RecentJob, WorkerObservers};
use crate::thumbnail::Thumbnails;

/// Pure-data view of the Jobs tab.  Built each frame from the
/// observers; no egui types in scope.
#[derive(Debug, Clone, PartialEq)]
pub struct JobsView {
    /// Every job running now, whatever its source.
    pub running: Vec<JobCard>,
    pub recent: Vec<JobCard>,
    /// Jobs submitted to the always-on local API (the local queue).
    pub local: Vec<JobCard>,
    /// URL the local API is reachable at, if it bound.
    pub local_api_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobCard {
    pub job_id: String,
    pub kind: String,
    pub model: String,
    pub prompt: String,
    /// `studio`, `local`, `lane` or `stream`.
    pub source: &'static str,
    pub state: JobCardState,
    pub started_at: DateTime<Utc>,
    /// Set on finished jobs only.
    pub finished_at: Option<DateTime<Utc>>,
    pub has_thumbnail: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobCardState {
    InFlight,
    Completed,
    Failed(String),
}

impl JobsView {
    pub fn build(observers: &WorkerObservers, now: DateTime<Utc>) -> Self {
        let thumbnails = &observers.thumbnails;
        let running = observers
            .active_jobs
            .lock()
            .iter()
            .map(|j| JobCard::from_current(j, now, thumbnails))
            .collect();
        let recent = observers
            .recent_jobs
            .lock()
            .iter()
            .map(|j| JobCard::from_recent(j, thumbnails))
            .collect();
        let local = observers
            .local_jobs
            .lock()
            .iter()
            .map(|j| JobCard::from_recent(j, thumbnails))
            .collect();
        let local_api_url = observers.local_api_url.lock().clone();
        Self {
            running,
            recent,
            local,
            local_api_url,
        }
    }

    /// Ids of every job the view shows.
    pub fn job_ids(&self) -> impl Iterator<Item = &str> {
        self.running
            .iter()
            .chain(&self.recent)
            .chain(&self.local)
            .map(|c| c.job_id.as_str())
    }
}

impl JobCard {
    fn from_current(j: &CurrentJob, _now: DateTime<Utc>, thumbnails: &Thumbnails) -> Self {
        Self {
            job_id: j.job_id.clone(),
            kind: j.kind.as_str().to_string(),
            model: j.model.clone(),
            prompt: j.prompt.clone(),
            source: j.source.as_str(),
            state: JobCardState::InFlight,
            started_at: j.started_at,
            finished_at: None,
            has_thumbnail: thumbnails.contains(&j.job_id),
        }
    }

    fn from_recent(r: &RecentJob, thumbnails: &Thumbnails) -> Self {
        let state = match &r.outcome {
            JobOutcome::Completed => JobCardState::Completed,
            JobOutcome::Failed { reason } => JobCardState::Failed(reason.clone()),
        };
        Self {
            job_id: r.job_id.clone(),
            kind: r.kind.as_str().to_string(),
            model: r.model.clone(),
            prompt: r.prompt.clone(),
            source: r.source.as_str(),
            state,
            started_at: r.started_at,
            finished_at: Some(r.finished_at),
            has_thumbnail: thumbnails.contains(&r.job_id),
        }
    }

    pub fn elapsed(&self, now: DateTime<Utc>) -> chrono::Duration {
        let end = self.finished_at.unwrap_or(now);
        end.signed_duration_since(self.started_at)
    }
}

/// Render a `chrono::Duration` as `118 ms` / `12s` / `3m 04s` / `1h 12m`.
pub fn format_duration(d: chrono::Duration) -> String {
    let millis = d.num_milliseconds().max(0);
    if millis < 1000 {
        return format!("{millis} ms");
    }
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        let rem = secs % 60;
        return format!("{mins}m {rem:02}s");
    }
    let hours = mins / 60;
    let rem_min = mins % 60;
    format!("{hours}h {rem_min:02}m")
}

/// Selecting a card selects its job; selecting it again clears the
/// selection.  Pure so the toggle is unit-tested.
pub fn toggle_selection(selected: Option<&str>, clicked: &str) -> Option<String> {
    (selected != Some(clicked)).then(|| clicked.to_string())
}

/// Which job an initial-selection override picks: a job id, or `latest`
/// for the newest finished job.  For screenshots and headless inspection
/// (`STUDIO_WORKER_UI_JOB`), like `STUDIO_WORKER_UI_TAB`.
pub fn resolve_initial_selection(spec: &str, view: &JobsView) -> Option<String> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("latest") {
        return view
            .recent
            .iter()
            .chain(&view.local)
            .max_by_key(|c| c.finished_at)
            .map(|c| c.job_id.clone());
    }
    view.job_ids().find(|id| *id == spec).map(str::to_string)
}

/// One log line as the log panel shows it: `HH:MM:SS level message`, the
/// time in `tz` (the operator's local zone on screen).
pub fn format_log_line<Tz: chrono::TimeZone>(line: &crate::job_log::JobLogLine, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format!(
        "{} {:<5} {}",
        line.ts.with_timezone(tz).format("%H:%M:%S"),
        line.level,
        line.message
    )
}

/// Decode a PNG thumbnail into an egui image.
pub fn decode_thumbnail(png: &[u8]) -> Option<egui::ColorImage> {
    let image = image::load_from_memory(png).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, &image))
}

/// GPU textures of the thumbnails on screen, made on first use and
/// dropped when their job leaves the view.
#[derive(Default)]
pub struct ThumbnailTextures {
    textures: HashMap<String, egui::TextureHandle>,
}

impl ThumbnailTextures {
    fn get(
        &mut self,
        ctx: &egui::Context,
        thumbnails: &Thumbnails,
        job_id: &str,
    ) -> Option<egui::TextureHandle> {
        if let Some(texture) = self.textures.get(job_id) {
            return Some(texture.clone());
        }
        let image = decode_thumbnail(&thumbnails.get(job_id)?)?;
        let texture = ctx.load_texture(
            format!("thumbnail-{job_id}"),
            image,
            egui::TextureOptions::LINEAR,
        );
        self.textures.insert(job_id.to_string(), texture.clone());
        Some(texture)
    }

    fn retain<'a>(&mut self, visible: impl Iterator<Item = &'a str>) {
        let visible: Vec<&str> = visible.collect();
        self.textures.retain(|id, _| visible.contains(&id.as_str()));
    }

    pub fn len(&self) -> usize {
        self.textures.len()
    }

    pub fn is_empty(&self) -> bool {
        self.textures.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Side of the thumbnail shown on a card, in points.
const THUMBNAIL_POINTS: f32 = 96.0;
/// Height of the log panel under a selected card, in points.
const LOG_PANEL_POINTS: f32 = 220.0;

/// What the Jobs tab needs besides its view.
pub struct JobsContext<'a> {
    pub thumbnails: &'a Thumbnails,
    pub textures: &'a mut ThumbnailTextures,
    pub selected: Option<&'a str>,
    /// The selected job's log, once fetched: `(job id, log)`.
    pub log: Option<&'a (String, JobLog)>,
}

/// Draw the tab; answers the new selection when the operator changed it.
pub fn render(ui: &mut egui::Ui, view: &JobsView, cx: JobsContext<'_>) -> Option<Option<String>> {
    let now = Utc::now();
    let JobsContext {
        thumbnails,
        textures,
        selected,
        log,
    } = cx;
    textures.retain(view.job_ids());
    let mut changed = None;
    let mut section = |ui: &mut egui::Ui, title: String, cards: &[JobCard], empty: &str| {
        ui.heading(title);
        ui.add_space(4.0);
        if cards.is_empty() {
            ui.label(egui::RichText::new(empty).italics());
        }
        for card in cards {
            let texture = card
                .has_thumbnail
                .then(|| textures.get(ui.ctx(), thumbnails, &card.job_id))
                .flatten();
            let is_selected = selected == Some(card.job_id.as_str());
            let card_log = log.filter(|(id, _)| is_selected && id == &card.job_id);
            if render_card(
                ui,
                card,
                now,
                texture,
                is_selected,
                card_log.map(|(_, l)| l),
            ) {
                changed = Some(toggle_selection(selected, &card.job_id));
            }
            ui.add_space(4.0);
        }
        ui.add_space(12.0);
    };

    section(
        ui,
        format!("Running ({})", view.running.len()),
        &view.running,
        "No job running.",
    );
    section(
        ui,
        format!("Studio jobs ({})", view.recent.len()),
        &view.recent,
        "No studio jobs yet.",
    );
    let local_title = match &view.local_api_url {
        Some(url) => format!("Local queue ({}) \u{00b7} {url}", view.local.len()),
        None => format!("Local queue ({})", view.local.len()),
    };
    section(ui, local_title, &view.local, "No local jobs yet.");
    changed
}

/// Draw one card; answers whether its log toggle was clicked.
fn render_card(
    ui: &mut egui::Ui,
    card: &JobCard,
    now: DateTime<Utc>,
    thumbnail: Option<egui::TextureHandle>,
    selected: bool,
    log: Option<&JobLog>,
) -> bool {
    let mut clicked = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            if let Some(texture) = &thumbnail {
                let size = texture.size_vec2();
                let scale = THUMBNAIL_POINTS / size.x.max(size.y).max(1.0);
                ui.add(egui::Image::new(texture).fit_to_exact_size(size * scale))
                    .on_hover_text("output thumbnail");
            }
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let (label, colour) = match &card.state {
                        JobCardState::InFlight => {
                            ("RUNNING", egui::Color32::from_rgb(232, 168, 56))
                        }
                        JobCardState::Completed => ("OK", egui::Color32::LIGHT_GREEN),
                        JobCardState::Failed(_) => ("FAIL", egui::Color32::LIGHT_RED),
                    };
                    ui.label(egui::RichText::new(label).color(colour).strong());
                    ui.label(
                        egui::RichText::new(card.source)
                            .color(egui::Color32::from_rgb(140, 180, 230)),
                    );
                    ui.monospace(&card.kind);
                    ui.label("\u{00b7}");
                    ui.monospace(&card.model);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.monospace(format_duration(card.elapsed(now)));
                    });
                });
                if !card.prompt.is_empty() {
                    ui.label(
                        egui::RichText::new(&card.prompt)
                            .italics()
                            .color(egui::Color32::from_gray(190)),
                    );
                }
                if let JobCardState::Failed(reason) = &card.state {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 140, 130),
                        format!("reason: {reason}"),
                    );
                }
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("job {}", card.job_id))
                            .color(egui::Color32::from_gray(150))
                            .small(),
                    );
                    let toggle = if selected { "Hide log" } else { "Show log" };
                    clicked = ui.small_button(toggle).clicked();
                });
            });
        });
        if selected {
            ui.separator();
            render_log(ui, &card.job_id, log);
        }
    });
    clicked
}

fn render_log(ui: &mut egui::Ui, job_id: &str, log: Option<&JobLog>) {
    let Some(log) = log else {
        ui.label(egui::RichText::new("Fetching the log\u{2026}").italics());
        return;
    };
    if log.dropped > 0 {
        ui.label(
            egui::RichText::new(format!("{} earlier lines dropped", log.dropped))
                .small()
                .color(egui::Color32::from_gray(150)),
        );
    }
    egui::ScrollArea::vertical()
        .id_salt(("job-log", job_id))
        .max_height(LOG_PANEL_POINTS)
        .stick_to_bottom(true)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for line in &log.lines {
                let colour = match line.level.as_str() {
                    "error" => egui::Color32::LIGHT_RED,
                    "warn" => egui::Color32::from_rgb(232, 168, 56),
                    _ => egui::Color32::from_gray(210),
                };
                ui.label(
                    egui::RichText::new(format_log_line(line, &chrono::Local))
                        .monospace()
                        .color(colour),
                );
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{CurrentJob, JobOutcome, JobSource, RecentJob, WorkerObservers};
    use crate::types::TaskKind;

    fn recent(id: &str, outcome: JobOutcome, source: JobSource) -> RecentJob {
        let now = Utc::now();
        RecentJob {
            job_id: id.into(),
            kind: TaskKind::Image,
            model: "synthetic".into(),
            prompt: "p".into(),
            outcome,
            started_at: now,
            finished_at: now,
            source,
        }
    }

    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 2, image::Rgb([9, 9, 9])))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn build_empty_view_when_observers_empty() {
        let view = JobsView::build(&WorkerObservers::default(), Utc::now());
        assert!(view.running.is_empty() && view.recent.is_empty() && view.local.is_empty());
    }

    #[test]
    fn build_lists_every_running_job_with_its_source() {
        let observers = WorkerObservers::default();
        observers.active_jobs.lock().push(CurrentJob {
            job_id: "j-x".into(),
            kind: TaskKind::Llm,
            model: "qwen".into(),
            prompt: "hi".into(),
            started_at: Utc::now(),
            source: JobSource::Lane,
        });
        let view = JobsView::build(&observers, Utc::now());
        let card = &view.running[0];
        assert_eq!(card.job_id, "j-x");
        assert_eq!(card.source, "lane");
        assert_eq!(card.kind, "llm");
        assert!(matches!(card.state, JobCardState::InFlight));
        assert!(card.finished_at.is_none());
    }

    #[test]
    fn build_marks_jobs_with_a_thumbnail_and_keeps_rings_apart() {
        let observers = WorkerObservers::default();
        observers.recent_jobs.lock().push_front(recent(
            "studio-1",
            JobOutcome::Failed {
                reason: "boom".into(),
            },
            JobSource::Studio,
        ));
        observers.local_jobs.lock().push_front(recent(
            "local-1",
            JobOutcome::Completed,
            JobSource::Local,
        ));
        observers.thumbnails.insert("local-1", png());
        *observers.local_api_url.lock() = Some("http://127.0.0.1:4787".into());

        let view = JobsView::build(&observers, Utc::now());

        assert_eq!(view.recent[0].job_id, "studio-1");
        assert!(matches!(view.recent[0].state, JobCardState::Failed(_)));
        assert!(!view.recent[0].has_thumbnail);
        assert_eq!(view.local[0].job_id, "local-1");
        assert!(view.local[0].has_thumbnail);
        assert_eq!(view.local_api_url.as_deref(), Some("http://127.0.0.1:4787"));
        assert_eq!(view.job_ids().collect::<Vec<_>>(), ["studio-1", "local-1"]);
    }

    #[test]
    fn an_initial_selection_picks_the_newest_or_a_named_job() {
        let observers = WorkerObservers::default();
        let mut older = recent("old", JobOutcome::Completed, JobSource::Studio);
        older.finished_at -= chrono::Duration::seconds(10);
        observers.recent_jobs.lock().push_front(older);
        observers.local_jobs.lock().push_front(recent(
            "new",
            JobOutcome::Completed,
            JobSource::Local,
        ));
        let view = JobsView::build(&observers, Utc::now());
        assert_eq!(
            resolve_initial_selection("latest", &view).as_deref(),
            Some("new")
        );
        assert_eq!(
            resolve_initial_selection("old", &view).as_deref(),
            Some("old")
        );
        assert_eq!(resolve_initial_selection("missing", &view), None);
        let empty = JobsView::build(&WorkerObservers::default(), Utc::now());
        assert_eq!(resolve_initial_selection("latest", &empty), None);
    }

    #[test]
    fn selecting_a_card_toggles() {
        assert_eq!(toggle_selection(None, "a").as_deref(), Some("a"));
        assert_eq!(toggle_selection(Some("a"), "a"), None);
        assert_eq!(toggle_selection(Some("a"), "b").as_deref(), Some("b"));
    }

    #[test]
    fn a_log_line_reads_time_level_message() {
        let line = crate::job_log::JobLogLine {
            ts: "2026-01-02T03:04:05Z".parse().unwrap(),
            level: "warn".into(),
            target: "t".into(),
            message: "slow download".into(),
        };
        assert_eq!(format_log_line(&line, &Utc), "03:04:05 warn  slow download");
    }

    #[test]
    fn a_thumbnail_decodes_and_garbage_does_not() {
        let image = decode_thumbnail(&png()).expect("decodes");
        assert_eq!(image.size, [4, 2]);
        assert!(decode_thumbnail(b"nope").is_none());
    }

    #[test]
    fn render_shows_thumbnails_and_the_selected_log_without_panicking() {
        let observers = WorkerObservers::default();
        observers.local_jobs.lock().push_front(recent(
            "local-1",
            JobOutcome::Completed,
            JobSource::Local,
        ));
        observers.thumbnails.insert("local-1", png());
        let view = JobsView::build(&observers, Utc::now());
        let log = (
            "local-1".to_string(),
            JobLog {
                lines: vec![crate::job_log::JobLogLine {
                    ts: Utc::now(),
                    level: "info".into(),
                    target: "t".into(),
                    message: "job finished".into(),
                }],
                dropped: 3,
            },
        );
        let mut textures = ThumbnailTextures::default();
        egui::__run_test_ui(|ui| {
            let changed = render(
                ui,
                &view,
                JobsContext {
                    thumbnails: &observers.thumbnails,
                    textures: &mut textures,
                    selected: Some("local-1"),
                    log: Some(&log),
                },
            );
            assert_eq!(changed, None);
        });
        assert_eq!(textures.len(), 1, "the thumbnail became a texture");

        // A job that left the view drops its texture.
        let empty = JobsView::build(&WorkerObservers::default(), Utc::now());
        egui::__run_test_ui(|ui| {
            render(
                ui,
                &empty,
                JobsContext {
                    thumbnails: &observers.thumbnails,
                    textures: &mut textures,
                    selected: None,
                    log: None,
                },
            );
        });
        assert!(textures.is_empty());
    }

    #[test]
    fn format_duration_covers_every_range() {
        assert_eq!(format_duration(chrono::Duration::seconds(12)), "12s");
        assert_eq!(format_duration(chrono::Duration::seconds(184)), "3m 04s");
        assert_eq!(
            format_duration(chrono::Duration::seconds(3600 + 12 * 60)),
            "1h 12m"
        );
        assert_eq!(format_duration(chrono::Duration::seconds(-5)), "0 ms");
        assert_eq!(
            format_duration(chrono::Duration::milliseconds(118)),
            "118 ms"
        );
    }
}
