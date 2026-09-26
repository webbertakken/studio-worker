//! The window's chrome: the navigation rail, the pulse header and the
//! status bar.  Each keeps its size whatever the state, so nothing moves
//! when a job starts or ends.

use eframe::egui::{
    self, vec2, Align, CornerRadius, Key, KeyboardShortcut, Layout, Modifiers, RichText, Sense,
    UiBuilder,
};

use super::actions::Feedback;
use super::icons::{self, Icon};
use super::page::Page;
use super::pulse::{Pulse, Signal};
use super::theme::{stroke, Palette, Tone, CONTROL_RADIUS};
use super::widgets;

/// Width of the rail, in points.
pub const RAIL_WIDTH: f32 = 92.0;
/// Height of the header, in points.
pub const HEADER_HEIGHT: f32 = 64.0;
/// Height of the status bar, in points.
pub const STATUS_BAR_HEIGHT: f32 = 28.0;
/// Height of one rail item, in points.
pub const RAIL_ITEM_HEIGHT: f32 = 60.0;
/// Width of a header signal and of the GPU gauge, in points.
pub const SIGNAL_WIDTH: f32 = 156.0;
pub const GPU_WIDTH: f32 = 150.0;
/// Width of the Pause / Resume button: fits either label.
pub const PAUSE_WIDTH: f32 = 104.0;

/// The rail icon of a page.
pub fn page_icon(page: Page) -> Icon {
    match page {
        Page::Jobs => Icon::Jobs,
        Page::Models => Icon::Models,
        Page::Worker => Icon::Worker,
        Page::Logs => Icon::Logs,
        Page::Config => Icon::Config,
    }
}

/// The page `Ctrl+1` … `Ctrl+5` picks this frame, if any.
pub fn page_shortcut(ctx: &egui::Context) -> Option<Page> {
    const KEYS: [Key; 5] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5];
    ctx.input_mut(|i| {
        KEYS.iter().enumerate().find_map(|(n, key)| {
            i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, *key))
                .then(|| Page::from_digit(n as u8 + 1))
                .flatten()
        })
    })
}

/// The rail; answers the page the operator picked.
pub fn rail(ui: &mut egui::Ui, current: Page, version: &str) -> Option<Page> {
    let p = Palette::of_ui(ui);
    let mut picked = None;
    ui.add_space(14.0);
    ui.vertical_centered(|ui| {
        icons::show(ui, Icon::Worker, 26.0, p.accent);
        ui.label(RichText::new("studio").small().strong().color(p.text));
        ui.label(RichText::new("worker").small().color(p.muted));
    });
    ui.add_space(14.0);
    for page in [Page::Jobs, Page::Models, Page::Worker] {
        if rail_item(ui, page, current) {
            picked = Some(page);
        }
    }
    ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
        ui.add_space(8.0);
        ui.label(RichText::new(format!("v{version}")).small().color(p.muted));
        ui.add_space(6.0);
        for page in [Page::Config, Page::Logs] {
            if rail_item(ui, page, current) {
                picked = Some(page);
            }
        }
    });
    picked
}

fn rail_item(ui: &mut egui::Ui, page: Page, current: Page) -> bool {
    let p = Palette::of_ui(ui);
    let selected = page == current;
    let width = ui.available_width() - 16.0;
    let (outer, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), RAIL_ITEM_HEIGHT + 4.0),
        Sense::click(),
    );
    let rect = egui::Rect::from_center_size(outer.center(), vec2(width, RAIL_ITEM_HEIGHT));
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            true,
            selected,
            page.label(),
        )
    });
    let radius = CornerRadius::same(CONTROL_RADIUS * 2);
    if selected {
        ui.painter().rect_filled(rect, radius, p.accent_soft);
        let bar = egui::Rect::from_min_size(
            egui::pos2(outer.left() + 2.0, rect.top() + 14.0),
            vec2(3.0, rect.height() - 28.0),
        );
        ui.painter()
            .rect_filled(bar, CornerRadius::same(2), p.accent);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, radius, p.card_hover);
    }
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            radius,
            stroke(2.0, p.accent),
            egui::StrokeKind::Inside,
        );
    }
    let colour = if selected { p.accent } else { p.muted };
    icons::paint(
        ui.painter(),
        page_icon(page),
        icons::square(rect.center() - vec2(0.0, 9.0), 22.0),
        colour,
    );
    let text_colour = if selected { p.text } else { p.muted };
    ui.painter().text(
        rect.center() + vec2(0.0, 15.0),
        egui::Align2::CENTER_CENTER,
        page.label(),
        egui::TextStyle::Small.resolve(ui.style()),
        text_colour,
    );
    response
        .on_hover_text(format!("{} (Ctrl+{})", page.hint(), page.digit()))
        .clicked()
}

/// The pulse header; answers the pause state the operator asked for.
pub fn header(ui: &mut egui::Ui, pulse: &Pulse, glow: f32) -> Option<bool> {
    let p = Palette::of_ui(ui);
    let mut pause = None;
    let full = ui.available_rect_before_wrap();
    let right_w = PAUSE_WIDTH + GPU_WIDTH + 2.0 * SIGNAL_WIDTH + 4.0 * 16.0;
    let activity_rect = egui::Rect::from_min_max(
        full.min,
        egui::pos2(
            (full.right() - right_w).max(full.left() + 120.0),
            full.bottom(),
        ),
    );
    ui.scope_builder(UiBuilder::new().max_rect(activity_rect), |ui| {
        ui.horizontal_centered(|ui| {
            let halo = if pulse.activity.glows() { glow } else { 0.0 };
            widgets::status_dot(ui, pulse.activity.tone(), halo);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                compact(ui);
                ui.add(
                    egui::Label::new(
                        RichText::new(pulse.activity.headline())
                            .size(16.0)
                            .strong()
                            .color(p.text),
                    )
                    .truncate(),
                );
                ui.add(egui::Label::new(widgets::muted(ui, pulse.activity.detail())).truncate());
            });
        });
    });
    let right_rect =
        egui::Rect::from_min_max(egui::pos2(full.right() - right_w, full.top()), full.max);
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(right_rect)
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            let (label, hint) = if pulse.paused {
                ("Resume", "start accepting studio job offers again")
            } else {
                (
                    "Pause",
                    "stop accepting studio job offers; a running job finishes",
                )
            };
            let response = if pulse.paused {
                widgets::primary_button(ui, label, pulse.can_pause, PAUSE_WIDTH)
            } else {
                widgets::button(ui, label, pulse.can_pause, PAUSE_WIDTH)
            };
            if response
                .on_hover_text(hint)
                .on_disabled_hover_text("the daemon does not answer")
                .clicked()
            {
                pause = Some(!pulse.paused);
            }
            ui.add_space(16.0);
            gpu_gauge(ui, pulse);
            ui.add_space(16.0);
            signal(ui, "Studio", &pulse.studio);
            ui.add_space(16.0);
            signal(ui, "Daemon", &pulse.daemon);
        },
    );
    pause
}

/// Tighten spacing so a two-line block fits the header's height.
fn compact(ui: &mut egui::Ui) {
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.spacing_mut().interact_size.y = 18.0;
}

fn signal(ui: &mut egui::Ui, what: &str, s: &Signal) {
    let p = Palette::of_ui(ui);
    ui.allocate_ui_with_layout(
        vec2(SIGNAL_WIDTH, HEADER_HEIGHT - 16.0),
        Layout::top_down(Align::Min),
        |ui| {
            compact(ui);
            ui.set_width(SIGNAL_WIDTH);
            ui.add_space(3.0);
            ui.label(RichText::new(what).small().color(p.muted));
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(vec2(10.0, 14.0), Sense::hover());
                ui.painter()
                    .circle_filled(dot.center(), 4.0, p.tone(s.tone));
                ui.add(egui::Label::new(RichText::new(&s.label).color(p.text)).truncate());
            });
        },
    )
    .response
    .on_hover_text(&s.detail);
}

fn gpu_gauge(ui: &mut egui::Ui, pulse: &Pulse) {
    let p = Palette::of_ui(ui);
    ui.allocate_ui_with_layout(
        vec2(GPU_WIDTH, HEADER_HEIGHT - 16.0),
        Layout::top_down(Align::Min),
        |ui| {
            compact(ui);
            ui.set_width(GPU_WIDTH);
            ui.add_space(3.0);
            ui.label(RichText::new("GPU memory").small().color(p.muted));
            ui.add_space(1.0);
            match &pulse.gpu {
                Some(gpu) => {
                    let colour = match gpu.tone() {
                        Tone::Busy => p.accent,
                        _ => p.info,
                    };
                    widgets::meter(ui, GPU_WIDTH, 6.0, &[(gpu.fraction(), colour)]);
                    ui.label(RichText::new(gpu.label()).small().color(p.text));
                }
                None => {
                    widgets::meter(ui, GPU_WIDTH, 6.0, &[]);
                    ui.label(RichText::new("unknown").small().color(p.muted));
                }
            }
        },
    )
    .response
    .on_hover_text("Held by the loaded models, from their catalogue estimates");
}

/// The status bar: the link on the left, the last action on the right.
pub fn status_bar(ui: &mut egui::Ui, link_line: &str, feedback: Option<&Feedback>) {
    let p = Palette::of_ui(ui);
    ui.horizontal_centered(|ui| {
        ui.add(egui::Label::new(RichText::new(link_line).small().color(p.muted)).truncate());
        if let Some(f) = feedback {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, tone) = feedback_line(f, &chrono::Local);
                ui.add(
                    egui::Label::new(RichText::new(text).small().color(p.tone(tone))).truncate(),
                );
            });
        }
    });
}

/// `12:03:04 · paused`, red when the action failed.
pub fn feedback_line<Tz: chrono::TimeZone>(f: &Feedback, tz: &Tz) -> (String, Tone)
where
    Tz::Offset: std::fmt::Display,
{
    let tone = if f.ok { Tone::Neutral } else { Tone::Bad };
    (
        format!("{} \u{00b7} {}", super::format::clock(f.at, tz), f.text),
        tone,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_register::RegistrationState;
    use crate::daemon_link::LinkState;
    use crate::runtime::SessionState;
    use chrono::{TimeZone, Utc};

    #[test]
    fn every_page_has_its_own_icon() {
        use std::collections::HashSet;
        let icons: HashSet<_> = Page::ALL
            .iter()
            .map(|p| format!("{:?}", page_icon(*p)))
            .collect();
        assert_eq!(icons.len(), Page::ALL.len());
    }

    #[test]
    fn feedback_reads_time_and_text_and_turns_red_on_failure() {
        let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let ok = Feedback {
            text: "paused".into(),
            ok: true,
            at,
        };
        assert_eq!(
            feedback_line(&ok, &Utc),
            ("03:04:05 \u{00b7} paused".into(), Tone::Neutral)
        );
        let failed = Feedback { ok: false, ..ok };
        assert_eq!(feedback_line(&failed, &Utc).1, Tone::Bad);
    }

    #[test]
    fn ctrl_digit_picks_a_page() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            modifiers: Modifiers::COMMAND,
            events: vec![egui::Event::Key {
                key: Key::Num4,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::COMMAND,
            }],
            ..Default::default()
        };
        let mut picked = None;
        let _ = ctx.run_ui(input, |ui| picked = page_shortcut(ui.ctx()));
        assert_eq!(picked, Some(Page::Logs));
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            picked = page_shortcut(ui.ctx())
        });
        assert_eq!(picked, None);
    }

    #[test]
    fn the_chrome_draws_for_every_page_and_state() {
        let link = LinkState::Connecting;
        let pulse = Pulse::build(super::super::pulse::PulseInputs {
            link: &link,
            registered: false,
            registration: &RegistrationState::Pristine,
            session: &SessionState::default(),
            busy: false,
            paused: false,
            active: &[],
            models: &[],
            vram_total_gb: 0.0,
            now: Utc::now(),
        });
        let feedback = Feedback {
            text: "daemon not reachable".into(),
            ok: false,
            at: Utc::now(),
        };
        for page in Page::ALL {
            egui::__run_test_ui(|ui| {
                assert_eq!(rail(ui, page, "0.4.9"), None);
                assert_eq!(header(ui, &pulse, 0.5), None);
                status_bar(ui, "connecting", Some(&feedback));
            });
        }
    }
}
