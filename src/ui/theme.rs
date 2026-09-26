//! The tray UI's look: two palettes (dark, light) held to WCAG 2.2 AA
//! contrast by tests, the egui visuals and style built from them, and the
//! soft "breathing" glow of running work.

use std::f64::consts::TAU;

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};
use serde::{Deserialize, Serialize};

/// WCAG 2.2 AA minimum contrast for text (1.4.3).
pub const TEXT_CONTRAST_MIN: f32 = 4.5;
/// WCAG 2.2 AA minimum contrast for indicators and focus rings (1.4.11).
pub const NON_TEXT_CONTRAST_MIN: f32 = 3.0;

/// One breath of the running glow, in seconds: slow enough to read as calm.
pub const BREATH_PERIOD_SECS: f64 = 2.4;

/// Corner radius of cards and panels, in points.
pub const CARD_RADIUS: u8 = 10;
/// Corner radius of buttons, inputs and pills, in points.
pub const CONTROL_RADIUS: u8 = 5;

/// The operator's theme choice (Config → This window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::Dark, ThemeChoice::Light, ThemeChoice::System];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::Dark => "Dark",
            ThemeChoice::Light => "Light",
            ThemeChoice::System => "Follow system",
        }
    }

    pub fn preference(self) -> egui::ThemePreference {
        match self {
            ThemeChoice::Dark => egui::ThemePreference::Dark,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::System => egui::ThemePreference::System,
        }
    }
}

/// What a colour means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Nothing to report.
    Neutral,
    /// Healthy, done, loaded.
    Good,
    /// Running, in transition, or asking for attention.
    Busy,
    /// Failed, refused, unreachable.
    Bad,
    /// Informational: sources, residency.
    Info,
}

impl Tone {
    pub const ALL: [Tone; 5] = [Tone::Neutral, Tone::Good, Tone::Busy, Tone::Bad, Tone::Info];
}

/// Every colour the UI paints with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Behind the page.
    pub page: Color32,
    /// Behind the rail, the header and the status bar.
    pub chrome: Color32,
    pub card: Color32,
    pub card_hover: Color32,
    /// Sunken areas: logs, inputs.
    pub inset: Color32,
    /// Hairlines between areas.
    pub line: Color32,
    /// Borders of inputs and checkboxes: 3:1 against every surface.
    pub control_line: Color32,
    pub text: Color32,
    /// Secondary text; still AA on every background.
    pub muted: Color32,
    /// Brass: selection, focus, running work.
    pub accent: Color32,
    /// Text on an accent-filled button.
    pub on_accent: Color32,
    /// The selected rail item's background.
    pub accent_soft: Color32,
    pub good: Color32,
    pub bad: Color32,
    pub info: Color32,
    pub neutral_soft: Color32,
    pub good_soft: Color32,
    pub busy_soft: Color32,
    pub bad_soft: Color32,
    pub info_soft: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Palette {
    pub const DARK: Palette = Palette {
        dark: true,
        page: hex(0x16171A),
        chrome: hex(0x101113),
        card: hex(0x1E1F23),
        card_hover: hex(0x25272C),
        inset: hex(0x0C0D0F),
        line: hex(0x2C2E34),
        control_line: hex(0x75716A),
        text: hex(0xEDE9E3),
        muted: hex(0xA9A49C),
        accent: hex(0xE8B660),
        on_accent: hex(0x1A1408),
        accent_soft: hex(0x33291A),
        good: hex(0x86CFA0),
        bad: hex(0xF4897C),
        info: hex(0x93BBF0),
        neutral_soft: hex(0x2B2D33),
        good_soft: hex(0x1C3226),
        busy_soft: hex(0x3A2F1A),
        bad_soft: hex(0x3E2321),
        info_soft: hex(0x1C2A3E),
    };

    pub const LIGHT: Palette = Palette {
        dark: false,
        page: hex(0xF3F1ED),
        chrome: hex(0xE9E6E0),
        card: hex(0xFFFFFF),
        card_hover: hex(0xF8F6F2),
        inset: hex(0xF0EDE7),
        line: hex(0xD6D0C6),
        control_line: hex(0x857F75),
        text: hex(0x1C1B19),
        muted: hex(0x57524B),
        accent: hex(0x8A5700),
        on_accent: hex(0xFFFFFF),
        accent_soft: hex(0xF3E4C6),
        good: hex(0x1B6A3F),
        bad: hex(0xB3261E),
        info: hex(0x1D58A3),
        neutral_soft: hex(0xECE9E3),
        good_soft: hex(0xE2F0E7),
        busy_soft: hex(0xF5E8CF),
        bad_soft: hex(0xFAE5E2),
        info_soft: hex(0xE0EAF6),
    };

    pub fn of(dark_mode: bool) -> &'static Palette {
        if dark_mode {
            &Self::DARK
        } else {
            &Self::LIGHT
        }
    }

    /// The palette of the theme `ui` paints in.
    pub fn of_ui(ui: &egui::Ui) -> &'static Palette {
        Self::of(ui.visuals().dark_mode)
    }

    /// The colour of text or an indicator that means `tone`.
    pub fn tone(&self, tone: Tone) -> Color32 {
        match tone {
            Tone::Neutral => self.muted,
            Tone::Good => self.good,
            Tone::Busy => self.accent,
            Tone::Bad => self.bad,
            Tone::Info => self.info,
        }
    }

    /// The background of a pill or box that means `tone`.
    pub fn tone_soft(&self, tone: Tone) -> Color32 {
        match tone {
            Tone::Neutral => self.neutral_soft,
            Tone::Good => self.good_soft,
            Tone::Busy => self.busy_soft,
            Tone::Bad => self.bad_soft,
            Tone::Info => self.info_soft,
        }
    }

    /// The surfaces text is drawn on.
    pub fn surfaces(&self) -> [Color32; 5] {
        [
            self.page,
            self.chrome,
            self.card,
            self.card_hover,
            self.inset,
        ]
    }
}

fn linear(channel: u8) -> f32 {
    let c = channel as f32 / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance of an opaque colour.
pub fn luminance(c: Color32) -> f32 {
    0.2126 * linear(c.r()) + 0.7152 * linear(c.g()) + 0.0722 * linear(c.b())
}

/// WCAG contrast ratio between two opaque colours, 1.0 to 21.0.
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// How strongly running work glows at time `t` (seconds), 0.35 to 1.0:
/// one slow breath per [`BREATH_PERIOD_SECS`].  Reduce motion holds it
/// steady at full strength.
pub fn breath(t: f64, reduce_motion: bool) -> f32 {
    if reduce_motion {
        return 1.0;
    }
    let phase = (t / BREATH_PERIOD_SECS * TAU).sin() * 0.5 + 0.5;
    (0.35 + 0.65 * phase) as f32
}

/// `colour` at `alpha` (0..=1), for glows over any background.
pub fn with_alpha(colour: Color32, alpha: f32) -> Color32 {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(colour.r(), colour.g(), colour.b(), a)
}

/// A stroke of `width` points (typed, so float literals never fall back).
pub fn stroke(width: f32, colour: Color32) -> Stroke {
    Stroke::new(width, colour)
}

/// The egui visuals of `palette`.
pub fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let control = CornerRadius::same(CONTROL_RADIUS);
    v.panel_fill = p.page;
    v.window_fill = p.card;
    v.window_stroke = stroke(1.0, p.line);
    v.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    v.extreme_bg_color = p.inset;
    v.text_edit_bg_color = Some(p.inset);
    v.faint_bg_color = p.card_hover;
    v.code_bg_color = p.inset;
    v.hyperlink_color = p.info;
    v.warn_fg_color = p.accent;
    v.error_fg_color = p.bad;
    v.weak_text_color = Some(p.muted);
    v.selection.bg_fill = with_alpha(p.accent, 0.35);
    v.selection.stroke = stroke(1.0, p.text);
    v.text_cursor.stroke = stroke(2.0, p.accent);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = stroke(1.0, p.line);
    w.noninteractive.fg_stroke = stroke(1.0, p.text);
    w.noninteractive.corner_radius = control;

    w.inactive.bg_fill = p.inset;
    w.inactive.weak_bg_fill = p.neutral_soft;
    w.inactive.bg_stroke = stroke(1.0, p.control_line);
    w.inactive.fg_stroke = stroke(1.0, p.text);
    w.inactive.corner_radius = control;

    w.hovered.bg_fill = p.card_hover;
    w.hovered.weak_bg_fill = p.card_hover;
    w.hovered.bg_stroke = stroke(1.0, p.text);
    w.hovered.fg_stroke = stroke(1.5, p.text);
    w.hovered.corner_radius = control;
    w.hovered.expansion = 0.0;

    // Pressed and keyboard-focused: the brass focus ring.
    w.active.bg_fill = p.card_hover;
    w.active.weak_bg_fill = p.card_hover;
    w.active.bg_stroke = stroke(2.0, p.accent);
    w.active.fg_stroke = stroke(2.0, p.text);
    w.active.corner_radius = control;
    w.active.expansion = 0.0;

    w.open = w.active;
    v
}

/// Type scale and spacing, the same in both themes.
pub fn style_tweaks(style: &mut egui::Style) {
    use FontFamily::{Monospace, Proportional};
    style.text_styles = [
        (TextStyle::Heading, FontId::new(22.0, Proportional)),
        (TextStyle::Body, FontId::new(14.0, Proportional)),
        (TextStyle::Button, FontId::new(14.0, Proportional)),
        (TextStyle::Small, FontId::new(12.0, Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, Monospace)),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0, 6.0);
    s.button_padding = egui::vec2(12.0, 5.0);
    // WCAG 2.5.8: targets of at least 24 × 24.
    s.interact_size.y = 28.0;
    s.window_margin = egui::Margin::same(16);
}

/// Apply both palettes and the operator's theme choice to `ctx`.
pub fn apply(ctx: &egui::Context, choice: ThemeChoice) {
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&Palette::DARK));
    ctx.set_visuals_of(egui::Theme::Light, visuals(&Palette::LIGHT));
    ctx.all_styles_mut(style_tweaks);
    ctx.set_theme(choice.preference());
}

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTES: [&Palette; 2] = [&Palette::DARK, &Palette::LIGHT];

    fn assert_contrast(what: &str, fg: Color32, bg: Color32, min: f32) {
        let ratio = contrast_ratio(fg, bg);
        assert!(
            ratio >= min,
            "{what}: {fg:?} on {bg:?} is {ratio:.2}:1, below {min}:1"
        );
    }

    #[test]
    fn contrast_follows_the_wcag_formula() {
        assert!((contrast_ratio(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(Color32::WHITE, Color32::WHITE) - 1.0).abs() < 0.001);
        // #767676 on white is the classic 4.54:1.
        assert!((contrast_ratio(hex(0x767676), Color32::WHITE) - 4.54).abs() < 0.01);
    }

    #[test]
    fn text_and_state_colours_meet_aa_on_every_surface() {
        for p in PALETTES {
            for bg in p.surfaces() {
                assert_contrast("text", p.text, bg, TEXT_CONTRAST_MIN);
                assert_contrast("muted", p.muted, bg, TEXT_CONTRAST_MIN);
                for tone in Tone::ALL {
                    assert_contrast(&format!("{tone:?}"), p.tone(tone), bg, TEXT_CONTRAST_MIN);
                }
            }
        }
    }

    #[test]
    fn pills_and_tinted_boxes_keep_their_text_readable() {
        for p in PALETTES {
            for tone in Tone::ALL {
                let bg = p.tone_soft(tone);
                assert_contrast(
                    &format!("{tone:?} on its pill"),
                    p.tone(tone),
                    bg,
                    TEXT_CONTRAST_MIN,
                );
                assert_contrast("text on a pill", p.text, bg, TEXT_CONTRAST_MIN);
            }
            assert_contrast("selected rail", p.text, p.accent_soft, TEXT_CONTRAST_MIN);
            assert_contrast("selected rail", p.accent, p.accent_soft, TEXT_CONTRAST_MIN);
            assert_contrast("primary button", p.on_accent, p.accent, TEXT_CONTRAST_MIN);
        }
    }

    #[test]
    fn the_focus_ring_and_indicators_stand_out() {
        for p in PALETTES {
            for bg in p.surfaces() {
                assert_contrast("focus ring", p.accent, bg, NON_TEXT_CONTRAST_MIN);
                assert_contrast("control border", p.control_line, bg, NON_TEXT_CONTRAST_MIN);
            }
        }
    }

    #[test]
    fn the_visuals_carry_the_palette() {
        for p in PALETTES {
            let v = visuals(p);
            assert_eq!(v.dark_mode, p.dark);
            assert_eq!(v.panel_fill, p.page);
            assert_eq!(v.widgets.noninteractive.fg_stroke.color, p.text);
            assert_eq!(v.widgets.active.bg_stroke.color, p.accent, "focus ring");
            assert_eq!(v.error_fg_color, p.bad);
        }
        assert_eq!(Palette::of(true), &Palette::DARK);
        assert_eq!(Palette::of(false), &Palette::LIGHT);
    }

    #[test]
    fn the_glow_breathes_between_bounds_and_holds_still_on_request() {
        let samples: Vec<f32> = (0..48).map(|i| breath(i as f64 * 0.05, false)).collect();
        let (lo, hi) = samples
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), &s| (lo.min(s), hi.max(s)));
        assert!(lo >= 0.35 - 1e-4 && hi <= 1.0 + 1e-4, "{lo}..{hi}");
        assert!(hi - lo > 0.5, "it visibly breathes");
        assert!((0..48).all(|i| breath(i as f64 * 0.05, true) == 1.0));
    }

    #[test]
    fn a_theme_choice_maps_to_egui_and_has_a_label() {
        assert_eq!(
            ThemeChoice::default().preference(),
            egui::ThemePreference::Dark
        );
        assert_eq!(
            ThemeChoice::Light.preference(),
            egui::ThemePreference::Light
        );
        assert_eq!(
            ThemeChoice::System.preference(),
            egui::ThemePreference::System
        );
        assert_eq!(ThemeChoice::System.label(), "Follow system");
    }

    #[test]
    fn with_alpha_keeps_the_hue() {
        let c = with_alpha(hex(0x102030), 0.5);
        assert_eq!(c.a(), 128);
        assert_eq!(with_alpha(Color32::WHITE, 2.0).a(), 255);
    }

    #[test]
    fn applying_a_theme_sets_both_palettes_and_the_type_scale() {
        let ctx = egui::Context::default();
        apply(&ctx, ThemeChoice::Light);
        assert_eq!(
            ctx.options(|o| o.theme_preference),
            egui::ThemePreference::Light
        );
        ctx.style_mut_of(egui::Theme::Dark, |s| {
            assert_eq!(s.visuals.panel_fill, Palette::DARK.page);
            assert_eq!(s.text_styles[&TextStyle::Body].size, 14.0);
        });
    }
}
