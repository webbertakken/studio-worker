//! Line icons painted from geometry, so they stay crisp at any scale,
//! take the theme's colours and need no icon font.

use eframe::egui::{self, vec2, Color32, Pos2, Rect, Shape};

use crate::types::TaskKind;

use super::theme::stroke;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Jobs,
    Models,
    Worker,
    Logs,
    Config,
    Image,
    Chat,
    Microphone,
    Speaker,
    Film,
    Pin,
}

impl Icon {
    pub const ALL: [Icon; 11] = [
        Icon::Jobs,
        Icon::Models,
        Icon::Worker,
        Icon::Logs,
        Icon::Config,
        Icon::Image,
        Icon::Chat,
        Icon::Microphone,
        Icon::Speaker,
        Icon::Film,
        Icon::Pin,
    ];

    /// The glyph of a job kind.
    pub fn of_kind(kind: TaskKind) -> Self {
        match kind {
            TaskKind::Image => Icon::Image,
            TaskKind::Llm => Icon::Chat,
            TaskKind::AudioStt => Icon::Microphone,
            TaskKind::AudioTts => Icon::Speaker,
            TaskKind::Video => Icon::Film,
        }
    }
}

/// The shapes of `icon` drawn inside `rect` in `colour`.  Geometry is laid
/// out on a 24-unit grid with a 2-unit margin, like common line-icon sets.
pub fn shapes(icon: Icon, rect: Rect, colour: Color32) -> Vec<Shape> {
    let unit = rect.width().min(rect.height()) / 24.0;
    let origin = rect.center() - vec2(12.0, 12.0) * unit;
    let p = |x: f32, y: f32| origin + vec2(x, y) * unit;
    let line = stroke((1.6 * unit).max(1.0), colour);
    let seg = |a: Pos2, b: Pos2| Shape::line_segment([a, b], line);
    let path = |points: Vec<Pos2>| Shape::line(points, line);
    let closed = |points: Vec<Pos2>| Shape::closed_line(points, line);
    let rounded = |min: Pos2, max: Pos2, r: f32| {
        Shape::rect_stroke(
            Rect::from_min_max(min, max),
            egui::CornerRadius::same((r * unit) as u8),
            line,
            egui::StrokeKind::Middle,
        )
    };
    let circle = |c: Pos2, r: f32| Shape::circle_stroke(c, r * unit, line);
    let dot = |c: Pos2, r: f32| Shape::circle_filled(c, r * unit, colour);

    match icon {
        Icon::Jobs => vec![
            rounded(p(4.0, 8.0), p(20.0, 20.0), 2.0),
            seg(p(6.0, 5.0), p(18.0, 5.0)),
            seg(p(8.0, 2.5), p(16.0, 2.5)),
        ],
        Icon::Models => vec![
            closed(vec![
                p(12.0, 2.5),
                p(20.5, 7.0),
                p(20.5, 17.0),
                p(12.0, 21.5),
                p(3.5, 17.0),
                p(3.5, 7.0),
            ]),
            path(vec![p(3.5, 7.0), p(12.0, 11.5), p(20.5, 7.0)]),
            seg(p(12.0, 11.5), p(12.0, 21.5)),
        ],
        Icon::Worker => vec![
            rounded(p(2.5, 4.0), p(21.5, 20.0), 3.0),
            path(vec![
                p(5.5, 12.5),
                p(9.0, 12.5),
                p(10.5, 8.0),
                p(13.5, 16.5),
                p(15.0, 12.5),
                p(18.5, 12.5),
            ]),
        ],
        Icon::Logs => vec![
            seg(p(4.0, 5.0), p(20.0, 5.0)),
            seg(p(4.0, 10.0), p(16.0, 10.0)),
            seg(p(4.0, 15.0), p(20.0, 15.0)),
            seg(p(4.0, 20.0), p(12.0, 20.0)),
        ],
        Icon::Config => vec![
            seg(p(3.0, 6.0), p(21.0, 6.0)),
            seg(p(3.0, 12.0), p(21.0, 12.0)),
            seg(p(3.0, 18.0), p(21.0, 18.0)),
            dot(p(8.0, 6.0), 2.4),
            dot(p(16.0, 12.0), 2.4),
            dot(p(10.0, 18.0), 2.4),
        ],
        Icon::Image => vec![
            rounded(p(3.0, 4.0), p(21.0, 20.0), 2.0),
            path(vec![
                p(3.5, 17.0),
                p(9.0, 11.0),
                p(13.0, 15.0),
                p(16.0, 12.5),
                p(20.5, 17.0),
            ]),
            circle(p(15.5, 8.5), 1.8),
        ],
        Icon::Chat => vec![
            closed(vec![
                p(3.0, 4.0),
                p(21.0, 4.0),
                p(21.0, 16.0),
                p(11.0, 16.0),
                p(6.0, 20.5),
                p(6.0, 16.0),
                p(3.0, 16.0),
            ]),
            seg(p(7.0, 8.5), p(17.0, 8.5)),
            seg(p(7.0, 12.0), p(14.0, 12.0)),
        ],
        Icon::Microphone => vec![
            rounded(p(9.0, 2.5), p(15.0, 14.5), 3.0),
            path(vec![
                p(5.5, 11.0),
                p(6.5, 15.0),
                p(9.0, 17.3),
                p(12.0, 18.0),
                p(15.0, 17.3),
                p(17.5, 15.0),
                p(18.5, 11.0),
            ]),
            seg(p(12.0, 18.0), p(12.0, 21.5)),
        ],
        Icon::Speaker => vec![
            closed(vec![
                p(3.0, 9.0),
                p(7.0, 9.0),
                p(12.0, 4.5),
                p(12.0, 19.5),
                p(7.0, 15.0),
                p(3.0, 15.0),
            ]),
            path(vec![p(15.0, 9.0), p(16.2, 12.0), p(15.0, 15.0)]),
            path(vec![p(18.0, 6.0), p(20.5, 12.0), p(18.0, 18.0)]),
        ],
        Icon::Film => vec![
            rounded(p(2.5, 4.5), p(21.5, 19.5), 2.0),
            closed(vec![p(10.0, 8.5), p(16.0, 12.0), p(10.0, 15.5)]),
        ],
        Icon::Pin => vec![
            circle(p(12.0, 8.5), 5.0),
            dot(p(12.0, 8.5), 1.6),
            path(vec![p(8.5, 12.5), p(12.0, 21.5), p(15.5, 12.5)]),
        ],
    }
}

/// Paint `icon` into `rect`.
pub fn paint(painter: &egui::Painter, icon: Icon, rect: Rect, colour: Color32) {
    painter.extend(shapes(icon, rect, colour));
}

/// Allocate a square of `side` points and paint `icon` in it.
pub fn show(ui: &mut egui::Ui, icon: Icon, side: f32, colour: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(side, side), egui::Sense::hover());
    paint(ui.painter(), icon, rect, colour);
    response
}

/// A square at `center` of `side` points.
pub fn square(center: Pos2, side: f32) -> Rect {
    Rect::from_center_size(center, vec2(side, side))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    #[test]
    fn every_icon_stays_inside_its_box() {
        let rect = square(pos2(40.0, 40.0), 24.0);
        for icon in Icon::ALL {
            let shapes = shapes(icon, rect, Color32::WHITE);
            assert!(!shapes.is_empty(), "{icon:?} draws something");
            for shape in shapes {
                let bounds = shape.visual_bounding_rect();
                assert!(
                    rect.expand(1.0).contains_rect(bounds),
                    "{icon:?} spills out: {bounds:?} outside {rect:?}"
                );
            }
        }
    }

    #[test]
    fn every_job_kind_has_its_own_glyph() {
        use std::collections::HashSet;
        let glyphs: HashSet<_> = TaskKind::ALL.iter().map(|k| Icon::of_kind(*k)).collect();
        assert_eq!(glyphs.len(), TaskKind::ALL.len());
    }

    #[test]
    fn icons_paint_without_panicking() {
        egui::__run_test_ui(|ui| {
            for icon in Icon::ALL {
                show(ui, icon, 20.0, Color32::GRAY);
            }
        });
    }
}
