//! Jobs: what runs now and what ran, in two panes.  The list on the left
//! keeps a slot for running work at the top and the history below, grouped
//! by day; the pane on the right shows the selected job, its image and its
//! log.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use eframe::egui::{self, vec2, Align, CornerRadius, Layout, RichText, Sense, UiBuilder};

use crate::job_log::JobLog;
use crate::runtime::{CurrentJob, JobOutcome, JobSource, RecentJob, WorkerObservers};
use crate::thumbnail::Thumbnails;
use crate::types::TaskKind;

use super::super::format::{clock, day_label, format_duration, moment};
use super::super::icons::{self, Icon};
use super::super::log_view::{self, LogLine};
use super::super::theme::{self, stroke, Palette, Tone, CARD_RADIUS, CONTROL_RADIUS};
use super::super::widgets;

// ---------------------------------------------------------------------------
// View model
// ---------------------------------------------------------------------------

/// Pure-data view of the page, built each frame from the observers.
#[derive(Debug, Clone, PartialEq)]
pub struct JobsView {
    /// Every job running now, oldest first.
    pub running: Vec<JobCard>,
    /// Finished studio and local jobs, newest first.
    pub history: Vec<JobCard>,
    /// Where the local API answers, once bound.
    pub local_api_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobCard {
    pub job_id: String,
    pub kind: TaskKind,
    pub model: String,
    pub prompt: String,
    pub source: JobSource,
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

/// Which finished jobs the history shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryFilter {
    #[default]
    All,
    /// Offers from the studio.
    Studio,
    /// Everything asked of this machine: local API jobs, lane chats,
    /// streaming speech sessions.
    Local,
}

impl HistoryFilter {
    pub const ALL: [HistoryFilter; 3] = [
        HistoryFilter::All,
        HistoryFilter::Studio,
        HistoryFilter::Local,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HistoryFilter::All => "All",
            HistoryFilter::Studio => "Studio",
            HistoryFilter::Local => "Local",
        }
    }

    pub fn matches(self, source: JobSource) -> bool {
        match self {
            HistoryFilter::All => true,
            HistoryFilter::Studio => source == JobSource::Studio,
            HistoryFilter::Local => source != JobSource::Studio,
        }
    }
}

impl JobsView {
    pub fn build(observers: &WorkerObservers) -> Self {
        let thumbnails = &observers.thumbnails;
        let mut running: Vec<JobCard> = observers
            .active_jobs
            .lock()
            .iter()
            .map(|j| JobCard::from_current(j, thumbnails))
            .collect();
        running.sort_by_key(|c| c.started_at);
        let mut history: Vec<JobCard> = observers
            .recent_jobs
            .lock()
            .iter()
            .chain(observers.local_jobs.lock().iter())
            .map(|j| JobCard::from_recent(j, thumbnails))
            .collect();
        history.sort_by_key(|c| std::cmp::Reverse(c.finished_at));
        Self {
            running,
            history,
            local_api_url: observers.local_api_url.lock().clone(),
        }
    }

    /// Finished jobs `filter` lets through, newest first.
    pub fn history_for(&self, filter: HistoryFilter) -> Vec<&JobCard> {
        self.history
            .iter()
            .filter(|c| filter.matches(c.source))
            .collect()
    }

    pub fn count(&self, filter: HistoryFilter) -> usize {
        self.history_for(filter).len()
    }

    /// The job with `id`, running or finished.
    pub fn find(&self, id: &str) -> Option<&JobCard> {
        self.running
            .iter()
            .chain(&self.history)
            .find(|c| c.job_id == id)
    }

    /// Ids of every job the view holds.
    pub fn job_ids(&self) -> impl Iterator<Item = &str> {
        self.running
            .iter()
            .chain(&self.history)
            .map(|c| c.job_id.as_str())
    }

    /// Ids in list order under `filter`: running, then the history.
    pub fn listed_ids(&self, filter: HistoryFilter) -> Vec<&str> {
        self.running
            .iter()
            .map(|c| c.job_id.as_str())
            .chain(
                self.history_for(filter)
                    .into_iter()
                    .map(|c| c.job_id.as_str()),
            )
            .collect()
    }
}

impl JobCard {
    fn from_current(j: &CurrentJob, thumbnails: &Thumbnails) -> Self {
        Self {
            job_id: j.job_id.clone(),
            kind: j.kind,
            model: j.model.clone(),
            prompt: j.prompt.clone(),
            source: j.source,
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
            kind: r.kind,
            model: r.model.clone(),
            prompt: r.prompt.clone(),
            source: r.source,
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

    /// The card's title: the prompt's first line, or `No prompt`.
    pub fn title(&self) -> &str {
        self.prompt
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("No prompt")
    }

    pub fn has_prompt(&self) -> bool {
        !self.prompt.trim().is_empty()
    }

    /// The prompt says more than its title: worth showing whole.
    pub fn prompt_beyond_title(&self) -> bool {
        self.has_prompt() && self.prompt.trim() != self.title()
    }

    /// `image · synthetic-image`.
    pub fn kind_line(&self) -> String {
        format!("{} \u{00b7} {}", self.kind.as_str(), self.model)
    }

    /// `local · 14:02:11 · 107 ms`; a running job counts up from its start.
    pub fn when_line<Tz: TimeZone>(&self, now: DateTime<Utc>, tz: &Tz) -> String
    where
        Tz::Offset: std::fmt::Display,
    {
        let at = self.finished_at.unwrap_or(self.started_at);
        let lead = if self.finished_at.is_some() {
            ""
        } else {
            "started "
        };
        format!(
            "{} \u{00b7} {lead}{} \u{00b7} {}",
            self.source.as_str(),
            clock(at, tz),
            format_duration(self.elapsed(now))
        )
    }

    /// The outcome pill.
    pub fn outcome(&self) -> (&'static str, Tone) {
        match self.state {
            JobCardState::InFlight => ("Running", Tone::Busy),
            JobCardState::Completed => ("Done", Tone::Good),
            JobCardState::Failed(_) => ("Failed", Tone::Bad),
        }
    }

    /// The facts the detail pane lists.
    pub fn facts<Tz: TimeZone>(&self, now: DateTime<Utc>, tz: &Tz) -> Vec<(&'static str, String)>
    where
        Tz::Offset: std::fmt::Display,
    {
        let mut facts = vec![
            ("Source", self.source.as_str().to_string()),
            ("Kind", self.kind.as_str().to_string()),
            ("Model", self.model.clone()),
            ("Started", moment(self.started_at, now, tz)),
        ];
        match self.finished_at {
            Some(done) => {
                facts.push(("Finished", moment(done, now, tz)));
                facts.push(("Took", format_duration(self.elapsed(now))));
            }
            None => facts.push(("Running for", format_duration(self.elapsed(now)))),
        }
        facts
    }
}

/// Cards grouped under the day they finished (in `tz`), newest day first.
pub fn group_by_day<'a, Tz: TimeZone>(
    cards: &[&'a JobCard],
    now: DateTime<Utc>,
    tz: &Tz,
) -> Vec<(String, Vec<&'a JobCard>)>
where
    Tz::Offset: std::fmt::Display,
{
    let today = now.with_timezone(tz).date_naive();
    let mut groups: Vec<(chrono::NaiveDate, Vec<&'a JobCard>)> = Vec::new();
    for card in cards {
        let day = card
            .finished_at
            .unwrap_or(card.started_at)
            .with_timezone(tz)
            .date_naive();
        match groups.last_mut() {
            Some((d, list)) if *d == day => list.push(card),
            _ => groups.push((day, vec![card])),
        }
    }
    groups
        .into_iter()
        .map(|(day, list)| (day_label(day, today), list))
        .collect()
}

/// Selecting a card selects its job; selecting it again clears it.
pub fn toggle_selection(selected: Option<&str>, clicked: &str) -> Option<String> {
    (selected != Some(clicked)).then(|| clicked.to_string())
}

/// Move the selection `delta` places through `ids` (list order), stopping
/// at either end; from no selection, down picks the first and up the last.
pub fn step_selection(ids: &[&str], current: Option<&str>, delta: i32) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let last = ids.len() as i32 - 1;
    let at = match current.and_then(|c| ids.iter().position(|id| *id == c)) {
        Some(i) => (i as i32 + delta).clamp(0, last),
        None if delta >= 0 => 0,
        None => last,
    };
    Some(ids[at as usize].to_string())
}

/// Which job an initial-selection override picks: a job id, or `latest`
/// for the newest finished job (`STUDIO_WORKER_UI_JOB`).
pub fn resolve_initial_selection(spec: &str, view: &JobsView) -> Option<String> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("latest") {
        return view.history.first().map(|c| c.job_id.clone());
    }
    view.job_ids().find(|id| *id == spec).map(str::to_string)
}

/// Decode a PNG thumbnail into an egui image.
pub fn decode_thumbnail(png: &[u8]) -> Option<egui::ColorImage> {
    let image = image::load_from_memory(png).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, &image))
}

/// The size an image of `size` takes to fit `max`, keeping its aspect and
/// never growing beyond `max_scale` × its own size.
pub fn fit(size: egui::Vec2, max: egui::Vec2, max_scale: f32) -> egui::Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let scale = (max.x / size.x).min(max.y / size.y).min(max_scale);
    size * scale.max(0.0)
}

// ---------------------------------------------------------------------------
// Page state
// ---------------------------------------------------------------------------

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

/// What the page remembers between frames.
#[derive(Default)]
pub struct JobsState {
    pub filter: HistoryFilter,
    /// The job whose image is shown larger.
    pub lightbox: Option<String>,
    pub textures: ThumbnailTextures,
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Height of a job card, in points: the same for every job, so the list
/// never jumps when a job starts, ends or gains a thumbnail.
pub const CARD_HEIGHT: f32 = 112.0;
/// Side of a card's tile (thumbnail or kind glyph), in points.
pub const TILE: f32 = 88.0;
/// The largest the detail pane shows a thumbnail, in points.
pub const DETAIL_IMAGE_MAX: f32 = 200.0;
/// Height of the log's header row and padding, in points.
pub const LOG_CHROME: f32 = 58.0;
/// The shortest the log panel gets, in points.
pub const LOG_MIN_HEIGHT: f32 = 220.0;
/// Width share of the detail pane, and its bounds in points.
pub const DETAIL_SHARE: f32 = 0.46;
pub const DETAIL_MIN: f32 = 340.0;
pub const DETAIL_MAX: f32 = 620.0;
/// Room kept for the list's scroll bar, in points.
pub const SCROLL_GUTTER: f32 = 14.0;
/// Gap between the panes, in points.
pub const PANE_GAP: f32 = 16.0;

/// What the page needs besides its view.
pub struct JobsContext<'a> {
    pub thumbnails: &'a Thumbnails,
    pub state: &'a mut JobsState,
    pub selected: Option<&'a str>,
    /// The selected job's log, once fetched: `(job id, log)`.
    pub log: Option<&'a (String, JobLog)>,
    pub paused: bool,
    pub reduce_motion: bool,
}

/// Draw the page; answers the new selection when the operator changed it.
pub fn render(ui: &mut egui::Ui, view: &JobsView, cx: JobsContext<'_>) -> Option<Option<String>> {
    let now = Utc::now();
    let JobsContext {
        thumbnails,
        state,
        selected,
        log,
        paused,
        reduce_motion,
    } = cx;
    state.textures.retain(view.job_ids());
    let selected = selected.filter(|id| view.find(id).is_some());
    let mut changed = keyboard_selection(ui, view, state, selected);

    let glow = theme::breath(ui.input(|i| i.time), reduce_motion);
    let total = ui.available_width();
    let detail_w = (total * DETAIL_SHARE)
        .clamp(DETAIL_MIN, DETAIL_MAX)
        .min(total * 0.6);
    // The row adds its item spacing once, before the gap.
    let spacing = ui.spacing().item_spacing.x;
    let list_w = (total - detail_w - PANE_GAP - spacing).max(200.0);
    let height = ui.available_height();

    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(list_w, height), Layout::top_down(Align::Min), |ui| {
            egui::ScrollArea::vertical()
                .id_salt("jobs-list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Keep the cards clear of the scroll bar.
                    ui.set_max_width(ui.available_width() - SCROLL_GUTTER);
                    let clicked =
                        render_list(ui, view, state, thumbnails, selected, paused, glow, now);
                    if let Some(id) = clicked {
                        changed = Some(toggle_selection(selected, &id));
                    }
                });
        });
        ui.add_space(PANE_GAP);
        ui.allocate_ui_with_layout(vec2(detail_w, height), Layout::top_down(Align::Min), |ui| {
            let card = selected.and_then(|id| view.find(id));
            render_detail(ui, card, state, thumbnails, log, now, height);
        });
    });

    render_lightbox(ui, view, state, thumbnails);
    changed
}

/// `↑` / `↓` move the selection, `Esc` clears it, while no widget holds
/// the keyboard.
fn keyboard_selection(
    ui: &egui::Ui,
    view: &JobsView,
    state: &JobsState,
    selected: Option<&str>,
) -> Option<Option<String>> {
    let free = ui.ctx().memory(|m| m.focused().is_none()) && state.lightbox.is_none();
    if !free {
        return None;
    }
    let (down, up, esc) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowDown),
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::Escape),
        )
    });
    let ids = view.listed_ids(state.filter);
    if down || up {
        let next = step_selection(&ids, selected, if down { 1 } else { -1 });
        return (next.as_deref() != selected).then_some(next);
    }
    (esc && selected.is_some()).then_some(None)
}

#[allow(clippy::too_many_arguments)]
fn render_list(
    ui: &mut egui::Ui,
    view: &JobsView,
    state: &mut JobsState,
    thumbnails: &Thumbnails,
    selected: Option<&str>,
    paused: bool,
    glow: f32,
    now: DateTime<Utc>,
) -> Option<String> {
    let p = Palette::of_ui(ui);
    let mut clicked = None;

    widgets::section_label(ui, &format!("NOW RUNNING \u{00b7} {}", view.running.len()));
    if view.running.is_empty() {
        idle_card(ui, paused);
    }
    for card in &view.running {
        let texture = texture_for(ui, state, thumbnails, card);
        if job_card(ui, card, texture, selected, glow, now) {
            clicked = Some(card.job_id.clone());
        }
        ui.add_space(8.0);
    }

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        widgets::section_label(ui, "HISTORY");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for filter in HistoryFilter::ALL.iter().rev() {
                let text = format!("{} {}", filter.label(), view.count(*filter));
                if ui
                    .selectable_label(state.filter == *filter, text)
                    .on_hover_text(match filter {
                        HistoryFilter::All => "every finished job",
                        HistoryFilter::Studio => "jobs the studio offered",
                        HistoryFilter::Local => {
                            "local API jobs, chats on loaded models, speech streams"
                        }
                    })
                    .clicked()
                {
                    state.filter = *filter;
                }
            }
        });
    });
    if state.filter == HistoryFilter::Local {
        ui.horizontal(|ui| match &view.local_api_url {
            Some(url) => {
                ui.label(widgets::muted(ui, "Local API at"));
                ui.label(RichText::new(url).monospace().color(p.text));
                widgets::copy_button(ui, "local-api-url", "Copy", url);
            }
            None => {
                ui.label(widgets::muted(ui, "The local API has not bound yet."));
            }
        });
    }
    ui.add_space(4.0);

    let history = view.history_for(state.filter);
    if history.is_empty() {
        widgets::card(ui, |ui| {
            widgets::empty_state(
                ui,
                Icon::Jobs,
                "No finished jobs yet",
                "Jobs land here when they finish, newest first.",
            );
        });
    }
    for (day, cards) in group_by_day(&history, now, &chrono::Local) {
        ui.add_space(6.0);
        ui.label(RichText::new(day).strong().color(p.muted));
        ui.add_space(2.0);
        for card in cards {
            let texture = texture_for(ui, state, thumbnails, card);
            if job_card(ui, card, texture, selected, 0.0, now) {
                clicked = Some(card.job_id.clone());
            }
            ui.add_space(8.0);
        }
    }
    clicked
}

fn texture_for(
    ui: &egui::Ui,
    state: &mut JobsState,
    thumbnails: &Thumbnails,
    card: &JobCard,
) -> Option<egui::TextureHandle> {
    card.has_thumbnail
        .then(|| state.textures.get(ui.ctx(), thumbnails, &card.job_id))
        .flatten()
}

/// The card that holds the running slot while nothing runs.
fn idle_card(ui: &mut egui::Ui, paused: bool) {
    let p = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), CARD_HEIGHT), Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(CARD_RADIUS),
        p.page,
        stroke(1.0, p.line),
        egui::StrokeKind::Inside,
    );
    let (title, body) = if paused {
        (
            "Paused",
            "Not claiming studio jobs. Local requests still run.",
        )
    } else {
        (
            "Nothing running",
            "Studio offers and local requests show here while they run.",
        )
    };
    ui.scope_builder(UiBuilder::new().max_rect(rect.shrink(16.0)), |ui| {
        ui.horizontal_centered(|ui| {
            icons::show(ui, Icon::Jobs, 28.0, p.muted);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong().color(p.text));
                ui.label(RichText::new(body).color(p.muted));
            });
        });
    });
}

/// One job card; answers whether it was clicked (or activated with the
/// keyboard).
fn job_card(
    ui: &mut egui::Ui,
    card: &JobCard,
    texture: Option<egui::TextureHandle>,
    selected: Option<&str>,
    glow: f32,
    now: DateTime<Utc>,
) -> bool {
    let p = Palette::of_ui(ui);
    let is_selected = selected == Some(card.job_id.as_str());
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), CARD_HEIGHT), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, is_selected, card.title())
    });

    let fill = if is_selected || response.hovered() {
        p.card_hover
    } else {
        p.card
    };
    let border = if response.has_focus() {
        stroke(2.0, p.accent)
    } else if is_selected {
        stroke(1.5, p.accent)
    } else if card.state == JobCardState::InFlight {
        stroke(1.5, theme::with_alpha(p.accent, 0.35 + 0.65 * glow))
    } else {
        stroke(1.0, p.line)
    };
    let radius = CornerRadius::same(CARD_RADIUS);
    if card.state == JobCardState::InFlight && glow > 0.0 {
        ui.painter().rect_filled(
            rect.expand(3.0),
            CornerRadius::same(CARD_RADIUS + 3),
            theme::with_alpha(p.accent, 0.12 * glow),
        );
    }
    ui.painter()
        .rect(rect, radius, fill, border, egui::StrokeKind::Inside);
    if is_selected {
        let bar =
            egui::Rect::from_min_size(rect.min + vec2(0.0, 12.0), vec2(3.0, rect.height() - 24.0));
        ui.painter()
            .rect_filled(bar, CornerRadius::same(2), p.accent);
    }

    let inner = rect.shrink(12.0);
    let tile = egui::Rect::from_min_size(inner.min, vec2(TILE, TILE));
    paint_tile(ui, tile, card.kind, texture.as_ref());

    let text_rect =
        egui::Rect::from_min_max(egui::pos2(tile.right() + 14.0, inner.top()), inner.max);
    ui.scope_builder(UiBuilder::new().max_rect(text_rect), |ui| {
        ui.horizontal_top(|ui| {
            let (label, tone) = card.outcome();
            let pill_w = 76.0;
            let title_w = (ui.available_width() - pill_w - 8.0).max(40.0);
            let title_colour = if card.has_prompt() { p.text } else { p.muted };
            let mut job = egui::text::LayoutJob::simple(
                card.title().to_string(),
                egui::TextStyle::Body.resolve(ui.style()),
                title_colour,
                title_w,
            );
            job.wrap.max_rows = 2;
            job.wrap.break_anywhere = false;
            let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
            let (title_rect, _) = ui.allocate_exact_size(vec2(title_w, 40.0), Sense::hover());
            ui.painter().galley(title_rect.min, galley, title_colour);
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                widgets::pill(ui, label, tone);
            });
        });
        ui.add_space(2.0);
        ui.add(
            egui::Label::new(RichText::new(card.kind_line()).monospace().color(p.muted)).truncate(),
        );
        ui.add(
            egui::Label::new(RichText::new(card.when_line(now, &chrono::Local)).color(p.muted))
                .truncate(),
        );
    });
    response.clicked()
}

/// A card's tile: the thumbnail, or the kind's glyph on a quiet ground.
fn paint_tile(
    ui: &egui::Ui,
    tile: egui::Rect,
    kind: TaskKind,
    texture: Option<&egui::TextureHandle>,
) {
    let p = Palette::of_ui(ui);
    let radius = CornerRadius::same(CONTROL_RADIUS);
    ui.painter().rect_filled(tile, radius, p.neutral_soft);
    match texture {
        Some(texture) => {
            let size = fit(texture.size_vec2(), tile.size(), 8.0);
            let image_rect = egui::Rect::from_center_size(tile.center(), size);
            egui::Image::new(texture)
                .corner_radius(radius)
                .paint_at(ui, image_rect);
        }
        None => {
            icons::paint(
                ui.painter(),
                Icon::of_kind(kind),
                icons::square(tile.center(), 32.0),
                p.muted,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_detail(
    ui: &mut egui::Ui,
    card: Option<&JobCard>,
    state: &mut JobsState,
    thumbnails: &Thumbnails,
    log: Option<&(String, JobLog)>,
    now: DateTime<Utc>,
    height: f32,
) {
    let p = Palette::of_ui(ui);
    let frame = widgets::card_frame(p);
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(height - 2.0 * f32::from(widgets::CARD_PADDING) - 2.0);
        let Some(card) = card else {
            ui.add_space(height * 0.25);
            widgets::empty_state(
                ui,
                Icon::Jobs,
                "No job selected",
                "Pick a job to see what it made and read its log.  The arrow keys move the selection.",
            );
            return;
        };
        let inner_height = ui.available_height();
        egui::ScrollArea::vertical()
            .id_salt("job-detail")
            .auto_shrink([false, false])
            .max_height(inner_height)
            .show(ui, |ui| {
                let content_top = ui.cursor().top();
                detail_header(ui, card);
                ui.add_space(12.0);
                let texture = card
                    .has_thumbnail
                    .then(|| state.textures.get(ui.ctx(), thumbnails, &card.job_id))
                    .flatten();
                match texture {
                    // The image and the facts side by side keep the log in view.
                    Some(texture) => {
                        ui.horizontal_top(|ui| {
                            detail_image(ui, state, card, &texture);
                            ui.add_space(16.0);
                            ui.vertical(|ui| detail_facts(ui, card, now));
                        });
                    }
                    None => detail_facts(ui, card, now),
                }
                if card.prompt_beyond_title() {
                    ui.add_space(10.0);
                    widgets::section_label(ui, "PROMPT");
                    ui.add(
                        egui::Label::new(RichText::new(card.prompt.trim()).color(p.text))
                            .wrap()
                            .selectable(true),
                    );
                }
                if let JobCardState::Failed(reason) = &card.state {
                    ui.add_space(10.0);
                    widgets::section_label(ui, "WHY IT FAILED");
                    widgets::problem_box(ui, if reason.is_empty() { "no reason given" } else { reason });
                }
                ui.add_space(12.0);
                // The log fills what is left of the pane: its header row and
                // its own padding aside.
                let used = ui.cursor().top() - content_top;
                let log_height = (inner_height - used - LOG_CHROME).max(LOG_MIN_HEIGHT);
                detail_log(ui, card, log, log_height);
            });
    });
}

fn detail_header(ui: &mut egui::Ui, card: &JobCard) {
    let p = Palette::of_ui(ui);
    ui.horizontal_top(|ui| {
        icons::show(ui, Icon::of_kind(card.kind), 22.0, p.muted);
        let (label, tone) = card.outcome();
        let width = (ui.available_width() - 84.0).max(80.0);
        ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
            let colour = if card.has_prompt() { p.text } else { p.muted };
            ui.add(
                egui::Label::new(
                    RichText::new(card.title())
                        .size(17.0)
                        .strong()
                        .color(colour),
                )
                .wrap(),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            widgets::pill(ui, label, tone);
        });
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new(&card.job_id).monospace().color(p.muted));
        widgets::copy_button(ui, ("job-id", &card.job_id), "Copy id", &card.job_id);
    });
}

fn detail_image(
    ui: &mut egui::Ui,
    state: &mut JobsState,
    card: &JobCard,
    texture: &egui::TextureHandle,
) {
    let max = vec2(
        DETAIL_IMAGE_MAX.min(ui.available_width() * 0.5),
        DETAIL_IMAGE_MAX,
    );
    let size = fit(texture.size_vec2(), max, 4.0);
    let response = ui
        .add(
            egui::Image::new(texture)
                .fit_to_exact_size(size)
                .corner_radius(CornerRadius::same(CONTROL_RADIUS))
                .sense(Sense::click()),
        )
        .on_hover_text("View larger (Enter)")
        .on_hover_cursor(egui::CursorIcon::ZoomIn);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "View the image larger")
    });
    if response.has_focus() {
        let p = Palette::of_ui(ui);
        ui.painter().rect_stroke(
            response.rect.expand(2.0),
            CornerRadius::same(CONTROL_RADIUS),
            stroke(2.0, p.accent),
            egui::StrokeKind::Outside,
        );
    }
    if response.clicked() {
        state.lightbox = Some(card.job_id.clone());
    }
}

fn detail_facts(ui: &mut egui::Ui, card: &JobCard, now: DateTime<Utc>) {
    widgets::facts(ui, "job-facts", |rows| {
        for (label, value) in card.facts(now, &chrono::Local) {
            if label == "Model" {
                rows.mono(label, &value);
            } else {
                rows.text(label, value);
            }
        }
    });
}

fn detail_log(ui: &mut egui::Ui, card: &JobCard, log: Option<&(String, JobLog)>, height: f32) {
    let log = log.filter(|(id, _)| id == &card.job_id).map(|(_, l)| l);
    let lines: Vec<LogLine> = log
        .map(|l| {
            l.lines
                .iter()
                .map(|line| LogLine::from_job_line(line, &chrono::Local))
                .collect()
        })
        .unwrap_or_default();
    let composed = log_view::compose(&lines);
    ui.horizontal(|ui| {
        widgets::section_label(ui, "LOG");
        let note = match log {
            None => "fetching\u{2026}".to_string(),
            Some(l) if l.dropped > 0 => format!("{} earlier lines dropped", l.dropped),
            Some(l) if l.lines.is_empty() => "no lines".to_string(),
            Some(l) => format!("{} lines", l.lines.len()),
        };
        ui.label(widgets::muted(ui, note).small());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            widgets::copy_button(ui, ("job-log", &card.job_id), "Copy log", &composed.text);
        });
    });
    log_view::show(
        ui,
        ("job-log", &card.job_id),
        &composed,
        card.state == JobCardState::InFlight,
        height,
    );
}

fn render_lightbox(
    ui: &mut egui::Ui,
    view: &JobsView,
    state: &mut JobsState,
    thumbnails: &Thumbnails,
) {
    let Some(id) = state.lightbox.clone() else {
        return;
    };
    let Some(card) = view.find(&id) else {
        state.lightbox = None;
        return;
    };
    let Some(texture) = state.textures.get(ui.ctx(), thumbnails, &id) else {
        state.lightbox = None;
        return;
    };
    let p = Palette::of_ui(ui);
    let screen = ui.ctx().content_rect().size();
    let max = vec2(screen.x * 0.8, screen.y * 0.75);
    let modal = egui::Modal::new(egui::Id::new("job-lightbox"))
        .frame(widgets::card_frame(p))
        .show(ui.ctx(), |ui| {
            let size = fit(texture.size_vec2(), max, 3.0);
            ui.add(
                egui::Image::new(&texture)
                    .fit_to_exact_size(size)
                    .corner_radius(CornerRadius::same(CONTROL_RADIUS)),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.set_max_width(size.x.max(260.0));
                ui.add(egui::Label::new(RichText::new(card.title()).color(p.text)).truncate());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    widgets::button(ui, "Close", true, 72.0).clicked()
                })
                .inner
            })
            .inner
        });
    if modal.inner || modal.should_close() {
        state.lightbox = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{CurrentJob, JobOutcome, JobSource, RecentJob, WorkerObservers};

    fn recent(id: &str, outcome: JobOutcome, source: JobSource, secs_ago: i64) -> RecentJob {
        let finished = Utc::now() - chrono::Duration::seconds(secs_ago);
        RecentJob {
            job_id: id.into(),
            kind: TaskKind::Image,
            model: "synthetic".into(),
            prompt: "a red fox\nin snow".into(),
            outcome,
            started_at: finished - chrono::Duration::milliseconds(107),
            finished_at: finished,
            source,
        }
    }

    fn running(id: &str, secs_ago: i64) -> CurrentJob {
        CurrentJob {
            job_id: id.into(),
            kind: TaskKind::Llm,
            model: "qwen".into(),
            prompt: "hi".into(),
            started_at: Utc::now() - chrono::Duration::seconds(secs_ago),
            source: JobSource::Lane,
        }
    }

    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 2, image::Rgb([9, 9, 9])))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn seeded() -> WorkerObservers {
        let o = WorkerObservers::default();
        o.recent_jobs.lock().push_front(recent(
            "studio-old",
            JobOutcome::Failed {
                reason: "boom".into(),
            },
            JobSource::Studio,
            30,
        ));
        o.local_jobs.lock().push_front(recent(
            "local-new",
            JobOutcome::Completed,
            JobSource::Local,
            5,
        ));
        o.local_jobs.lock().push_front(recent(
            "stream-1",
            JobOutcome::Completed,
            JobSource::Stream,
            1,
        ));
        o.active_jobs.lock().push(running("run-young", 2));
        o.active_jobs.lock().push(running("run-old", 20));
        o.thumbnails.insert("local-new", png());
        *o.local_api_url.lock() = Some("http://127.0.0.1:4787".into());
        o
    }

    #[test]
    fn an_empty_worker_has_an_empty_view() {
        let view = JobsView::build(&WorkerObservers::default());
        assert!(view.running.is_empty() && view.history.is_empty());
        assert!(view.local_api_url.is_none());
    }

    #[test]
    fn running_jobs_list_oldest_first_and_history_newest_first() {
        let view = JobsView::build(&seeded());
        let running: Vec<_> = view.running.iter().map(|c| c.job_id.as_str()).collect();
        assert_eq!(running, ["run-old", "run-young"]);
        let history: Vec<_> = view.history.iter().map(|c| c.job_id.as_str()).collect();
        assert_eq!(history, ["stream-1", "local-new", "studio-old"]);
        assert!(view.find("local-new").unwrap().has_thumbnail);
        assert!(view.find("missing").is_none());
    }

    #[test]
    fn filters_split_studio_from_everything_local() {
        let view = JobsView::build(&seeded());
        assert_eq!(view.count(HistoryFilter::All), 3);
        assert_eq!(view.count(HistoryFilter::Studio), 1);
        assert_eq!(view.count(HistoryFilter::Local), 2, "local and stream");
        assert_eq!(
            view.listed_ids(HistoryFilter::Studio),
            ["run-old", "run-young", "studio-old"]
        );
        let labels: Vec<_> = HistoryFilter::ALL.iter().map(|f| f.label()).collect();
        assert_eq!(labels, ["All", "Studio", "Local"]);
    }

    #[test]
    fn a_card_reads_what_which_model_when_and_how_long() {
        let view = JobsView::build(&seeded());
        let card = view.find("local-new").unwrap();
        assert_eq!(card.title(), "a red fox");
        assert_eq!(card.kind_line(), "image \u{00b7} synthetic");
        let when = card.when_line(Utc::now(), &Utc);
        assert!(when.starts_with("local \u{00b7} "), "{when}");
        assert!(when.ends_with(" \u{00b7} 107 ms"), "{when}");
        assert_eq!(card.outcome(), ("Done", Tone::Good));

        let run = view.find("run-old").unwrap();
        assert!(run.when_line(Utc::now(), &Utc).contains("started "));
        assert_eq!(run.outcome(), ("Running", Tone::Busy));
        assert_eq!(
            view.find("studio-old").unwrap().outcome(),
            ("Failed", Tone::Bad)
        );
    }

    #[test]
    fn a_job_without_a_prompt_says_so() {
        let mut card = JobsView::build(&seeded()).history[0].clone();
        card.prompt = "  \n ".into();
        assert_eq!(card.title(), "No prompt");
        assert!(!card.has_prompt());
        assert!(!card.prompt_beyond_title());
    }

    #[test]
    fn the_whole_prompt_shows_only_when_it_says_more_than_the_title() {
        let mut card = JobsView::build(&seeded()).history[0].clone();
        assert!(card.prompt_beyond_title(), "two lines");
        card.prompt = " one line \n".into();
        assert!(!card.prompt_beyond_title());
    }

    #[test]
    fn facts_say_took_when_finished_and_running_for_while_running() {
        let view = JobsView::build(&seeded());
        let labels = |id: &str| -> Vec<&'static str> {
            view.find(id)
                .unwrap()
                .facts(Utc::now(), &Utc)
                .into_iter()
                .map(|(l, _)| l)
                .collect()
        };
        assert_eq!(
            labels("local-new"),
            ["Source", "Kind", "Model", "Started", "Finished", "Took"]
        );
        assert_eq!(
            labels("run-old"),
            ["Source", "Kind", "Model", "Started", "Running for"]
        );
    }

    #[test]
    fn history_groups_by_day_newest_first() {
        let now = Utc::now();
        let mut today = JobsView::build(&seeded()).history;
        let mut old = today[0].clone();
        old.job_id = "last-week".into();
        old.finished_at = Some(now - chrono::Duration::days(8));
        let mut yesterday = today[0].clone();
        yesterday.job_id = "yesterday".into();
        yesterday.finished_at = Some(now - chrono::Duration::days(1));
        today.push(yesterday);
        today.push(old);
        let refs: Vec<&JobCard> = today.iter().collect();
        let groups = group_by_day(&refs, now, &Utc);
        let labels: Vec<_> = groups.iter().map(|(l, c)| (l.as_str(), c.len())).collect();
        assert_eq!(labels[0], ("Today", 3));
        assert_eq!(labels[1], ("Yesterday", 1));
        assert_eq!(labels[2].1, 1);
        assert!(group_by_day(&[], now, &Utc).is_empty());
    }

    #[test]
    fn selecting_a_card_toggles() {
        assert_eq!(toggle_selection(None, "a").as_deref(), Some("a"));
        assert_eq!(toggle_selection(Some("a"), "a"), None);
        assert_eq!(toggle_selection(Some("a"), "b").as_deref(), Some("b"));
    }

    #[test]
    fn arrows_step_through_the_list_and_stop_at_its_ends() {
        let ids = ["a", "b", "c"];
        assert_eq!(step_selection(&ids, None, 1).as_deref(), Some("a"));
        assert_eq!(step_selection(&ids, None, -1).as_deref(), Some("c"));
        assert_eq!(step_selection(&ids, Some("a"), 1).as_deref(), Some("b"));
        assert_eq!(step_selection(&ids, Some("c"), 1).as_deref(), Some("c"));
        assert_eq!(step_selection(&ids, Some("a"), -1).as_deref(), Some("a"));
        assert_eq!(step_selection(&ids, Some("gone"), 1).as_deref(), Some("a"));
        assert_eq!(step_selection(&[], Some("a"), 1), None);
    }

    #[test]
    fn an_initial_selection_picks_the_newest_or_a_named_job() {
        let view = JobsView::build(&seeded());
        assert_eq!(
            resolve_initial_selection("latest", &view).as_deref(),
            Some("stream-1")
        );
        assert_eq!(
            resolve_initial_selection(" run-old ", &view).as_deref(),
            Some("run-old")
        );
        assert_eq!(resolve_initial_selection("missing", &view), None);
        let empty = JobsView::build(&WorkerObservers::default());
        assert_eq!(resolve_initial_selection("latest", &empty), None);
    }

    #[test]
    fn images_fit_their_box_and_never_blow_up_too_far() {
        let fitted = fit(vec2(384.0, 192.0), vec2(88.0, 88.0), 8.0);
        assert_eq!(fitted, vec2(88.0, 44.0));
        let capped = fit(vec2(4.0, 2.0), vec2(800.0, 800.0), 3.0);
        assert_eq!(capped, vec2(12.0, 6.0));
        assert_eq!(
            fit(egui::Vec2::ZERO, vec2(10.0, 10.0), 2.0),
            egui::Vec2::ZERO
        );
    }

    #[test]
    fn a_thumbnail_decodes_and_garbage_does_not() {
        let image = decode_thumbnail(&png()).expect("decodes");
        assert_eq!(image.size, [4, 2]);
        assert!(decode_thumbnail(b"nope").is_none());
    }

    fn render_once(
        view: &JobsView,
        o: &WorkerObservers,
        state: &mut JobsState,
        selected: Option<&str>,
        log: Option<&(String, JobLog)>,
    ) {
        egui::__run_test_ui(|ui| {
            render(
                ui,
                view,
                JobsContext {
                    thumbnails: &o.thumbnails,
                    state,
                    selected,
                    log,
                    paused: false,
                    reduce_motion: false,
                },
            );
        });
    }

    #[test]
    fn every_state_of_the_page_draws_and_textures_follow_the_view() {
        let o = seeded();
        let view = JobsView::build(&o);
        let log = (
            "local-new".to_string(),
            JobLog {
                lines: vec![crate::job_log::JobLogLine {
                    ts: Utc::now(),
                    level: "info".into(),
                    target: "studio_worker::job".into(),
                    message: "job finished".into(),
                }],
                dropped: 3,
            },
        );
        let mut state = JobsState::default();
        for filter in HistoryFilter::ALL {
            state.filter = filter;
            for selected in [None, Some("local-new"), Some("run-old"), Some("studio-old")] {
                render_once(&view, &o, &mut state, selected, Some(&log));
            }
        }
        assert_eq!(state.textures.len(), 1, "the thumbnail became a texture");

        state.lightbox = Some("local-new".into());
        render_once(&view, &o, &mut state, Some("local-new"), None);
        assert_eq!(
            state.lightbox.as_deref(),
            Some("local-new"),
            "the larger view stays open"
        );

        let empty = JobsView::build(&WorkerObservers::default());
        egui::__run_test_ui(|ui| {
            render(
                ui,
                &empty,
                JobsContext {
                    thumbnails: &o.thumbnails,
                    state: &mut state,
                    selected: Some("local-new"),
                    log: None,
                    paused: true,
                    reduce_motion: true,
                },
            );
        });
        assert!(
            state.textures.is_empty(),
            "a job that left drops its texture"
        );
        assert!(
            state.lightbox.is_none(),
            "a job that left closes its larger view"
        );
    }
}
