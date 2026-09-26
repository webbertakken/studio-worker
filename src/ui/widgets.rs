//! Small building blocks every page shares: cards, pills, status dots,
//! buttons, fact rows, copy buttons, empty states.  Rendering only; the
//! decisions they show are made by the pages' view models.

use eframe::egui::{self, vec2, Color32, CornerRadius, Frame, Margin, RichText, Sense};

use super::icons::{self, Icon};
use super::theme::{self, stroke, Palette, Tone, CARD_RADIUS, CONTROL_RADIUS};

/// Inner margin of a card, in points.
pub const CARD_PADDING: i8 = 16;
/// How long a copy button says "Copied", in seconds.
pub const COPIED_FOR_SECS: f64 = 1.5;
/// Width of a copy button: fits "Copied" so the label swap never moves it.
pub const COPY_BUTTON_WIDTH: f32 = 72.0;

/// The frame of a card.
pub fn card_frame(p: &Palette) -> Frame {
    Frame::new()
        .fill(p.card)
        .stroke(stroke(1.0, p.line))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(CARD_PADDING))
}

/// A full-width card around `add`.
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let p = Palette::of_ui(ui);
    card_frame(p)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Two cards side by side, both as tall as the taller.  Their backgrounds
/// are painted after both are laid out, in the same frame.
pub fn card_pair(
    ui: &mut egui::Ui,
    left: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui),
) {
    let p = *Palette::of_ui(ui);
    ui.columns(2, |cols| {
        let (left_bg, left_rect) = card_body(&mut cols[0], left);
        let (right_bg, right_rect) = card_body(&mut cols[1], right);
        let height = left_rect.height().max(right_rect.height());
        for (col, bg, rect) in [(0, left_bg, left_rect), (1, right_bg, right_rect)] {
            let rect = egui::Rect::from_min_size(rect.min, vec2(rect.width(), height));
            cols[col].painter().set(bg, card_shape(&p, rect));
            cols[col].allocate_rect(rect, Sense::hover());
        }
    });
}

/// Lay out a card's contents over a background slot; answers the slot and
/// the card's rectangle.
fn card_body(
    ui: &mut egui::Ui,
    add: impl FnOnce(&mut egui::Ui),
) -> (egui::layers::ShapeIdx, egui::Rect) {
    let bg = ui.painter().add(egui::Shape::Noop);
    let rect = Frame::new()
        .inner_margin(Margin::same(CARD_PADDING))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        })
        .response
        .rect;
    (bg, rect)
}

fn card_shape(p: &Palette, rect: egui::Rect) -> egui::Shape {
    egui::Shape::Rect(egui::epaint::RectShape::new(
        rect,
        CornerRadius::same(CARD_RADIUS),
        p.card,
        stroke(1.0, p.line),
        egui::StrokeKind::Inside,
    ))
}

/// A page's title and one line saying what it is for.
pub fn page_title(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    let p = Palette::of_ui(ui);
    ui.label(RichText::new(title).heading().color(p.text));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(p.muted));
    }
    ui.add_space(12.0);
}

/// A small label above a group of cards or facts.
pub fn section_label(ui: &mut egui::Ui, text: &str) {
    let p = Palette::of_ui(ui);
    ui.label(RichText::new(text).small().strong().color(p.muted));
    ui.add_space(2.0);
}

/// Secondary text.
pub fn muted(ui: &egui::Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).color(Palette::of_ui(ui).muted)
}

/// A rounded, tinted label that says what a state means.
pub fn pill(ui: &mut egui::Ui, text: &str, tone: Tone) -> egui::Response {
    let p = Palette::of_ui(ui);
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font, p.tone(tone));
    let size = galley.size() + vec2(16.0, 6.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(CONTROL_RADIUS * 2),
        p.tone_soft(tone),
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, p.tone(tone));
    response
}

/// A dot in the colour of `tone`, with a soft halo of strength `glow`
/// (0 = none) for running work.
pub fn status_dot(ui: &mut egui::Ui, tone: Tone, glow: f32) -> egui::Response {
    let p = Palette::of_ui(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
    let colour = p.tone(tone);
    if glow > 0.0 {
        ui.painter()
            .circle_filled(rect.center(), 7.0, theme::with_alpha(colour, 0.35 * glow));
    }
    ui.painter().circle_filled(rect.center(), 4.0, colour);
    response
}

/// The one button that matters on a card: brass-filled.
pub fn primary_button(
    ui: &mut egui::Ui,
    text: &str,
    enabled: bool,
    min_width: f32,
) -> egui::Response {
    let p = Palette::of_ui(ui);
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).strong().color(p.on_accent))
            .fill(p.accent)
            .min_size(vec2(min_width, 30.0)),
    )
}

/// A quiet button of at least `min_width`, so its label can change
/// without moving what sits next to it.
pub fn button(ui: &mut egui::Ui, text: &str, enabled: bool, min_width: f32) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(text).min_size(vec2(min_width, 30.0)),
    )
}

/// Whether a copy button clicked at `clicked_at` still says "Copied" at
/// `now` (both in egui seconds).
pub fn shows_copied(clicked_at: Option<f64>, now: f64) -> bool {
    clicked_at.is_some_and(|at| now >= at && now - at < COPIED_FOR_SECS)
}

/// A "Copy" button that puts `text` on the clipboard and says "Copied"
/// for a moment.  `id_salt` keeps several apart.
pub fn copy_button(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    label: &str,
    text: &str,
) -> egui::Response {
    let id = ui.id().with(("copy", id_salt));
    let now = ui.input(|i| i.time);
    let clicked_at: Option<f64> = ui.data(|d| d.get_temp(id));
    let copied = shows_copied(clicked_at, now);
    let shown = if copied { "Copied" } else { label };
    let response =
        button(ui, shown, true, COPY_BUTTON_WIDTH).on_hover_text("Copy to the clipboard");
    if response.clicked() {
        ui.ctx().copy_text(text.to_string());
        ui.data_mut(|d| d.insert_temp(id, now));
    }
    if copied {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
    response
}

/// Two-column facts: a muted label and its value.
pub fn facts(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut FactRows<'_>)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([16.0, 8.0])
        .min_col_width(88.0)
        .show(ui, |ui| add(&mut FactRows { ui }));
}

/// Rows of [`facts`].
pub struct FactRows<'a> {
    pub ui: &'a mut egui::Ui,
}

impl FactRows<'_> {
    /// A row whose value is plain text.
    pub fn text(&mut self, label: &str, value: impl Into<String>) {
        self.row(label, |ui| {
            ui.add(egui::Label::new(value.into()).wrap());
        });
    }

    /// A row whose value is monospace (ids, paths, URLs).
    pub fn mono(&mut self, label: &str, value: &str) {
        self.row(label, |ui| {
            ui.add(egui::Label::new(RichText::new(value).monospace()).wrap());
        });
    }

    /// A monospace row in the colour of `tone` (versions, states of ids).
    pub fn mono_toned(&mut self, label: &str, value: &str, tone: Tone) {
        let colour = Palette::of_ui(self.ui).tone(tone);
        let colour = if tone == Tone::Neutral {
            Palette::of_ui(self.ui).text
        } else {
            colour
        };
        self.row(label, |ui| {
            ui.add(egui::Label::new(RichText::new(value).monospace().color(colour)).wrap());
        });
    }

    /// A row whose value is text in the colour of `tone`.
    pub fn toned(&mut self, label: &str, value: impl Into<String>, tone: Tone) {
        let colour = Palette::of_ui(self.ui).tone(tone);
        self.row(label, |ui| {
            ui.add(egui::Label::new(RichText::new(value.into()).color(colour)).wrap());
        });
    }

    /// A row with a custom value.
    pub fn row(&mut self, label: &str, add: impl FnOnce(&mut egui::Ui)) {
        let text = muted(self.ui, label);
        self.ui.label(text);
        self.ui.horizontal_wrapped(add);
        self.ui.end_row();
    }
}

/// An empty state: an icon, a title and one line of guidance.
pub fn empty_state(ui: &mut egui::Ui, icon: Icon, title: &str, body: &str) {
    let p = Palette::of_ui(ui);
    ui.vertical_centered(|ui| {
        ui.add_space(8.0);
        icons::show(ui, icon, 32.0, p.muted);
        ui.add_space(6.0);
        ui.label(RichText::new(title).strong().color(p.text));
        ui.label(RichText::new(body).color(p.muted));
        ui.add_space(8.0);
    });
}

/// A tinted box for an error or a failure reason; selectable, wrapping.
pub fn problem_box(ui: &mut egui::Ui, text: &str) {
    tinted_box(ui, Tone::Bad, text);
}

/// A tinted box in the colour of `tone`.
pub fn tinted_box(ui: &mut egui::Ui, tone: Tone, text: &str) {
    let p = Palette::of_ui(ui);
    Frame::new()
        .fill(p.tone_soft(tone))
        .corner_radius(CornerRadius::same(CONTROL_RADIUS))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(RichText::new(text).color(p.tone(tone)))
                    .wrap()
                    .selectable(true),
            );
        });
}

/// A thin horizontal bar: `segments` of (fraction, colour) over a track.
pub fn meter(ui: &mut egui::Ui, width: f32, height: f32, segments: &[(f32, Color32)]) {
    let p = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let radius = CornerRadius::same((height / 2.0) as u8);
    ui.painter().rect_filled(rect, radius, p.neutral_soft);
    let mut x = rect.left();
    for (fraction, colour) in segments {
        let w = (fraction.clamp(0.0, 1.0) * rect.width()).min(rect.right() - x);
        if w <= 0.0 {
            continue;
        }
        let seg = egui::Rect::from_min_size(egui::pos2(x, rect.top()), vec2(w, height));
        ui.painter().rect_filled(seg, radius, *colour);
        x += w;
    }
    ui.painter()
        .rect_stroke(rect, radius, stroke(1.0, p.line), egui::StrokeKind::Inside);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_button_says_copied_for_a_moment() {
        assert!(!shows_copied(None, 10.0));
        assert!(shows_copied(Some(10.0), 10.0));
        assert!(shows_copied(Some(10.0), 11.4));
        assert!(!shows_copied(Some(10.0), 11.6));
        assert!(!shows_copied(Some(10.0), 9.0), "a clock that went back");
    }

    #[test]
    fn the_widgets_draw_in_both_themes() {
        for dark in [true, false] {
            egui::__run_test_ui(|ui| {
                ui.ctx().set_visuals(theme::visuals(Palette::of(dark)));
                page_title(ui, "Title", "What it is for");
                section_label(ui, "SECTION");
                card(ui, |ui| {
                    for tone in Tone::ALL {
                        pill(ui, "pill", tone);
                        status_dot(ui, tone, 0.8);
                    }
                    primary_button(ui, "Go", true, 90.0);
                    button(ui, "Later", false, 90.0);
                    copy_button(ui, "id", "Copy", "text");
                    facts(ui, "facts", |rows| {
                        rows.text("Label", "value");
                        rows.mono("Id", "w-1");
                        rows.toned("State", "ok", Tone::Good);
                        rows.mono_toned("Version", "1.0", Tone::Neutral);
                        rows.mono_toned("Daemon", "0.9", Tone::Busy);
                    });
                    empty_state(ui, Icon::Jobs, "Nothing yet", "Wait a moment.");
                    problem_box(ui, "it broke");
                    meter(ui, 120.0, 8.0, &[(0.4, Color32::RED), (0.8, Color32::BLUE)]);
                });
                card_pair(
                    ui,
                    |ui| {
                        ui.label("short");
                    },
                    |ui| {
                        ui.label("tall");
                        ui.label("taller");
                    },
                );
            });
        }
    }
}
