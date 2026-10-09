//! Small vector glyphs painted by hand so the chrome matches the reference
//! (line icons, not emoji from the fallback font).

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};

fn line(p: &Painter, pts: Vec<Pos2>, w: f32, color: Color32) {
    p.add(Shape::line(pts, Stroke::new(w, color)));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Arrow {
    Left,
    Right,
    Up,
    Down,
}

pub fn chevron(p: &Painter, center: Pos2, half: f32, color: Color32, dir: Arrow) {
    let w = 1.4;
    let (a, b, c) = match dir {
        Arrow::Right => (
            Pos2::new(center.x - half * 0.5, center.y - half),
            Pos2::new(center.x + half * 0.5, center.y),
            Pos2::new(center.x - half * 0.5, center.y + half),
        ),
        Arrow::Down => (
            Pos2::new(center.x - half, center.y - half * 0.5),
            Pos2::new(center.x, center.y + half * 0.5),
            Pos2::new(center.x + half, center.y - half * 0.5),
        ),
        Arrow::Left => (
            Pos2::new(center.x + half * 0.5, center.y - half),
            Pos2::new(center.x - half * 0.5, center.y),
            Pos2::new(center.x + half * 0.5, center.y + half),
        ),
        Arrow::Up => (
            Pos2::new(center.x - half, center.y + half * 0.5),
            Pos2::new(center.x, center.y - half * 0.5),
            Pos2::new(center.x + half, center.y + half * 0.5),
        ),
    };
    line(p, vec![a, b, c], w, color);
}

pub fn arrow(p: &Painter, rect: Rect, color: Color32, dir: Arrow) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.5;
    let w = 1.4;
    match dir {
        Arrow::Left | Arrow::Right => {
            let sign = if dir == Arrow::Left { -1.0 } else { 1.0 };
            let tip = Pos2::new(c.x + sign * s * 0.7, c.y);
            let tail = Pos2::new(c.x - sign * s * 0.7, c.y);
            line(p, vec![tail, tip], w, color);
            line(
                p,
                vec![
                    Pos2::new(tip.x - sign * s * 0.45, c.y - s * 0.45),
                    tip,
                    Pos2::new(tip.x - sign * s * 0.45, c.y + s * 0.45),
                ],
                w,
                color,
            );
        }
        Arrow::Up | Arrow::Down => {
            let sign = if dir == Arrow::Up { -1.0 } else { 1.0 };
            let tip = Pos2::new(c.x, c.y + sign * s * 0.7);
            let tail = Pos2::new(c.x, c.y - sign * s * 0.7);
            line(p, vec![tail, tip], w, color);
            line(
                p,
                vec![
                    Pos2::new(c.x - s * 0.45, tip.y - sign * s * 0.45),
                    tip,
                    Pos2::new(c.x + s * 0.45, tip.y - sign * s * 0.45),
                ],
                w,
                color,
            );
        }
    }
}

pub fn bell(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height());
    let w = 1.3;
    let top = c.y - s * 0.36;
    let bottom = c.y + s * 0.22;
    let half = s * 0.26;
    let body = vec![
        Pos2::new(c.x - half, bottom),
        Pos2::new(c.x - half, top + s * 0.1),
        Pos2::new(c.x - half * 0.4, top),
        Pos2::new(c.x + half * 0.4, top),
        Pos2::new(c.x + half, top + s * 0.1),
        Pos2::new(c.x + half, bottom),
    ];
    line(p, body, w, color);
    line(p, vec![Pos2::new(c.x - half, bottom), Pos2::new(c.x + half, bottom)], w, color);
    line(
        p,
        vec![
            Pos2::new(c.x - s * 0.08, bottom + s * 0.1),
            Pos2::new(c.x, bottom + s * 0.16),
            Pos2::new(c.x + s * 0.08, bottom + s * 0.1),
        ],
        w,
        color,
    );
}

pub fn plus(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.32;
    line(p, vec![Pos2::new(c.x - s, c.y), Pos2::new(c.x + s, c.y)], 1.4, color);
    line(p, vec![Pos2::new(c.x, c.y - s), Pos2::new(c.x, c.y + s)], 1.4, color);
}

pub fn close(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.28;
    line(
        p,
        vec![Pos2::new(c.x - s, c.y - s), Pos2::new(c.x + s, c.y + s)],
        1.3,
        color,
    );
    line(
        p,
        vec![Pos2::new(c.x + s, c.y - s), Pos2::new(c.x - s, c.y + s)],
        1.3,
        color,
    );
}

pub fn sidebar_toggle(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.18);
    p.rect_stroke(
        r,
        egui::CornerRadius::same(3),
        Stroke::new(1.3, color),
        egui::StrokeKind::Inside,
    );
    let x = r.min.x + r.width() * 0.36;
    p.line_segment(
        [Pos2::new(x, r.min.y + 1.0), Pos2::new(x, r.max.y - 1.0)],
        Stroke::new(1.3, color),
    );
}

/// A rounded rect with a divider: "split right" or "split down".
pub fn split(p: &Painter, rect: Rect, color: Color32, vertical: bool) {
    let r = rect.shrink(rect.width() * 0.16);
    p.rect_stroke(
        r,
        egui::CornerRadius::same(3),
        Stroke::new(1.2, color),
        egui::StrokeKind::Inside,
    );
    if vertical {
        let y = r.center().y;
        p.line_segment([Pos2::new(r.min.x, y), Pos2::new(r.max.x, y)], Stroke::new(1.2, color));
    } else {
        let x = r.center().x;
        p.line_segment([Pos2::new(x, r.min.y), Pos2::new(x, r.max.y)], Stroke::new(1.2, color));
    }
}

pub fn folder(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.16);
    let tab_w = r.width() * 0.42;
    let body = Rect::from_min_max(Pos2::new(r.min.x, r.min.y + r.height() * 0.18), r.max);
    p.rect_filled(body, egui::CornerRadius::same(2), color);
    p.rect_filled(
        Rect::from_min_size(r.min, Vec2::new(tab_w, r.height() * 0.34)),
        egui::CornerRadius { nw: 2, ne: 2, sw: 0, se: 0 },
        color,
    );
}

pub fn terminal(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.16);
    p.rect_stroke(
        r,
        egui::CornerRadius::same(3),
        Stroke::new(1.2, color),
        egui::StrokeKind::Inside,
    );
    let s = r.width() * 0.22;
    line(
        p,
        vec![
            Pos2::new(r.center().x - s, r.center().y - s * 0.7),
            Pos2::new(r.center().x - s * 0.2, r.center().y),
            Pos2::new(r.center().x - s, r.center().y + s * 0.7),
        ],
        1.2,
        color,
    );
    p.line_segment(
        [
            Pos2::new(r.center().x + s * 0.1, r.center().y + s * 0.7),
            Pos2::new(r.center().x + s, r.center().y + s * 0.7),
        ],
        Stroke::new(1.2, color),
    );
}

pub fn globe(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let r = rect.width().min(rect.height()) * 0.34;
    p.circle_stroke(c, r, Stroke::new(1.2, color));
    p.line_segment([Pos2::new(c.x - r, c.y), Pos2::new(c.x + r, c.y)], Stroke::new(1.0, color));
    p.add(Shape::line(
        vec![
            Pos2::new(c.x, c.y - r),
            Pos2::new(c.x - r * 0.5, c.y),
            Pos2::new(c.x, c.y + r),
            Pos2::new(c.x + r * 0.5, c.y),
            Pos2::new(c.x, c.y - r),
        ],
        Stroke::new(1.0, color),
    ));
}

pub fn dot(p: &Painter, center: Pos2, r: f32, color: Color32) {
    p.circle_filled(center, r, color);
}

pub fn ellipsis(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let step = rect.width().min(rect.height()) * 0.26;
    for i in -1..=1 {
        p.circle_filled(Pos2::new(c.x + i as f32 * step, c.y), 1.1, color);
    }
}

pub fn spinner(p: &Painter, center: Pos2, radius: f32, color: Color32, phase: f32) {
    use std::f32::consts::TAU;
    let start = phase * TAU;
    let steps = 18;
    let mut pts = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let a = start + (i as f32 / steps as f32) * TAU * 0.7;
        pts.push(Pos2::new(center.x + radius * a.cos(), center.y + radius * a.sin()));
    }
    p.add(Shape::line(pts, Stroke::new(1.4, color)));
}

/// A git-branch mark: two stacked nodes joining a third.
pub fn branch(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.34;
    let w = 1.2;
    p.circle_filled(Pos2::new(c.x - s * 0.45, c.y - s * 0.65), 1.2, color);
    p.circle_filled(Pos2::new(c.x - s * 0.45, c.y + s * 0.65), 1.2, color);
    p.circle_filled(Pos2::new(c.x + s * 0.55, c.y + s * 0.65), 1.2, color);
    p.line_segment(
        [
            Pos2::new(c.x - s * 0.45, c.y - s * 0.65),
            Pos2::new(c.x - s * 0.45, c.y + s * 0.65),
        ],
        Stroke::new(w, color),
    );
    p.add(Shape::line(
        vec![
            Pos2::new(c.x - s * 0.45, c.y - s * 0.1),
            Pos2::new(c.x + s * 0.55, c.y - s * 0.1),
            Pos2::new(c.x + s * 0.55, c.y + s * 0.65),
        ],
        Stroke::new(w, color),
    ));
}

/// A tiny mark for status rows.
pub fn check(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.3;
    line(
        p,
        vec![
            Pos2::new(c.x - s, c.y),
            Pos2::new(c.x - s * 0.2, c.y + s * 0.8),
            Pos2::new(c.x + s, c.y - s * 0.8),
        ],
        1.4,
        color,
    );
}
