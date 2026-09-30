//! Latency chart painted with egui primitives.

use crate::model::Sample;
use crate::timefmt::local_hms;
use eframe::egui::{self, epaint::Mesh, Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Vec2};

const NICE_STEPS: [u32; 11] = [50, 100, 200, 300, 500, 750, 1000, 1500, 2000, 3000, 5000];

#[derive(Debug, PartialEq, Default, Clone, Copy)]
pub struct Stats {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
    pub drops: usize,
    pub total: usize,
}

pub fn stats(samples: &[Sample]) -> Stats {
    let vals: Vec<u32> = samples.iter().filter_map(|s| s.ms).collect();
    let total = samples.len();
    let drops = total - vals.len();
    if vals.is_empty() {
        return Stats { drops, total, ..Stats::default() };
    }
    let sum: u64 = vals.iter().map(|&v| v as u64).sum();
    Stats {
        min: *vals.iter().min().unwrap(),
        avg: (sum / vals.len() as u64) as u32,
        max: *vals.iter().max().unwrap(),
        drops,
        total,
    }
}

/// Round the y-axis top up to a readable value (never below 50 ms).
pub fn nice_y_max(max_ms: u32) -> u32 {
    let want = max_ms.max(50);
    NICE_STEPS
        .iter()
        .copied()
        .find(|&s| s >= want)
        .unwrap_or_else(|| want.div_ceil(1000) * 1000)
}

const COL_BG: Color32 = Color32::from_rgb(14, 14, 18);
const COL_GRID: Color32 = Color32::from_rgba_premultiplied(28, 28, 31, 70);
const COL_AXIS: Color32 = Color32::from_rgb(180, 180, 200);
const COL_TEXT: Color32 = Color32::from_rgb(200, 200, 215);
const COL_GOOD: Color32 = Color32::from_rgb(90, 200, 120);
const COL_SLOW: Color32 = Color32::from_rgb(220, 180, 50);
const COL_DROP: Color32 = Color32::from_rgba_premultiplied(150, 40, 40, 170);

pub fn draw(ui: &mut egui::Ui, size: Vec2, samples: &[Sample], slow_avg_ms: u32) {
    let (resp, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, 0.0, Color32::from_rgb(18, 18, 22));
    let font = FontId::monospace(11.0);
    let hud_font = FontId::monospace(12.5);

    if samples.len() < 2 {
        painter.text(rect.center(), Align2::CENTER_CENTER, "Collecting ping samples...", hud_font, COL_TEXT);
        return;
    }

    let st = stats(samples);
    let y_max = nice_y_max(st.max) as f32;
    let (left, right, top, bottom) = (44.0, 12.0, 22.0, 24.0);
    let plot = Rect::from_min_max(
        Pos2::new(rect.left() + left, rect.top() + top),
        Pos2::new(rect.right() - right, rect.bottom() - bottom),
    );
    if plot.width() < 10.0 || plot.height() < 10.0 {
        return;
    }
    painter.rect_filled(plot, 0.0, COL_BG);

    // grid + y labels
    let steps = 5;
    for i in 0..=steps {
        let y = plot.bottom() - plot.height() * i as f32 / steps as f32;
        painter.line_segment([Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)], Stroke::new(1.0, COL_GRID));
        painter.text(
            Pos2::new(plot.left() - 4.0, y),
            Align2::RIGHT_CENTER,
            format!("{}", (y_max * i as f32 / steps as f32) as u32),
            font.clone(),
            COL_TEXT,
        );
    }
    let axis = Stroke::new(1.0, COL_AXIS);
    painter.line_segment([plot.left_top(), plot.left_bottom()], axis);
    painter.line_segment([plot.left_bottom(), plot.right_bottom()], axis);

    // points + drop markers
    let n = samples.len();
    let x_of = |i: usize| plot.left() + plot.width() * i as f32 / (n - 1) as f32;
    let mut points: Vec<Pos2> = Vec::with_capacity(n);
    let mut prev: Option<Pos2> = None;
    let mut area = Mesh::default();
    let area_col = Color32::from_rgba_unmultiplied(90, 200, 120, 40);
    for (i, s) in samples.iter().enumerate() {
        let x = x_of(i);
        match s.ms {
            None => {
                painter.line_segment([Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())], Stroke::new(1.0, COL_DROP));
                prev = None; // a drop breaks the filled area, matching the line
            }
            Some(ms) => {
                let y = (plot.bottom() - plot.height() * (ms as f32 / y_max)).max(plot.top());
                let p = Pos2::new(x, y);
                if let Some(q) = prev {
                    let base = plot.bottom();
                    let i0 = area.vertices.len() as u32;
                    for v in [q, p, Pos2::new(p.x, base), Pos2::new(q.x, base)] {
                        area.colored_vertex(v, area_col);
                    }
                    area.add_triangle(i0, i0 + 1, i0 + 2);
                    area.add_triangle(i0, i0 + 2, i0 + 3);
                }
                prev = Some(p);
                points.push(p);
            }
        }
    }
    painter.add(Shape::mesh(area));
    let line_col = if st.avg > slow_avg_ms { COL_SLOW } else { COL_GOOD };
    if points.len() > 1 {
        painter.add(Shape::line(points.clone(), Stroke::new(1.6, line_col)));
    }
    if let Some(last) = points.last() {
        painter.circle_filled(*last, 3.0, line_col);
    }

    // x labels
    let y_lbl = plot.bottom() + 4.0;
    painter.text(Pos2::new(plot.left(), y_lbl), Align2::LEFT_TOP, local_hms(samples[0].t), font.clone(), COL_TEXT);
    painter.text(Pos2::new(plot.right(), y_lbl), Align2::RIGHT_TOP, local_hms(samples[n - 1].t), font.clone(), COL_TEXT);
    if plot.width() > 240.0 {
        painter.text(Pos2::new(plot.center().x, y_lbl), Align2::CENTER_TOP, local_hms(samples[n / 2].t), font, COL_TEXT);
    }

    // HUD
    painter.text(
        Pos2::new(plot.right(), rect.top() + 4.0),
        Align2::RIGHT_TOP,
        format!("min {}  avg {}  max {}  drops {}/{}", st.min, st.avg, st.max, st.drops, st.total),
        hud_font,
        COL_TEXT,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ms: Option<u32>) -> Sample {
        Sample { t: 0, ms }
    }

    #[test]
    fn stats_ignore_drops_in_latency_but_count_them() {
        let st = stats(&[s(Some(10)), s(None), s(Some(30)), s(Some(20))]);
        assert_eq!(st, Stats { min: 10, avg: 20, max: 30, drops: 1, total: 4 });
    }

    #[test]
    fn stats_of_all_drops_do_not_panic() {
        let st = stats(&[s(None), s(None)]);
        assert_eq!((st.min, st.avg, st.max, st.drops, st.total), (0, 0, 0, 2, 2));
        assert_eq!(stats(&[]), Stats::default());
    }

    #[test]
    fn y_axis_rounds_up_to_nice_values() {
        assert_eq!(nice_y_max(0), 50);
        assert_eq!(nice_y_max(51), 100);
        assert_eq!(nice_y_max(200), 200);
        assert_eq!(nice_y_max(201), 300);
        assert_eq!(nice_y_max(5000), 5000);
        assert_eq!(nice_y_max(5001), 6000);
    }
}
