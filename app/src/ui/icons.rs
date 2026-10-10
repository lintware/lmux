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

/// A window with its sidebar panel shaded, as the macOS sidebar button.
pub fn sidebar_toggle(p: &Painter, rect: Rect, color: Color32) {
    let r = Rect::from_center_size(rect.center(), Vec2::new(rect.width() * 0.86, rect.height() * 0.70));
    let w = 1.4;
    p.rect_stroke(r, egui::CornerRadius::same(3), Stroke::new(w, color), egui::StrokeKind::Inside);
    let x = r.min.x + r.width() * 0.38;
    p.line_segment([Pos2::new(x, r.min.y + w), Pos2::new(x, r.max.y - w)], Stroke::new(w, color));
    let panel = Rect::from_min_max(Pos2::new(r.min.x + w, r.min.y + w), Pos2::new(x, r.max.y - w));
    p.rect_filled(panel, egui::CornerRadius { nw: 2, sw: 2, ne: 0, se: 0 }, color.gamma_multiply(0.35));
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

/// An unread count in a flame-orange pill centred on `center`.
pub fn badge(p: &Painter, center: Pos2, count: u32) {
    let text = if count > 99 { "99+".to_owned() } else { count.to_string() };
    let w = (7.0 + text.len() as f32 * 6.0).max(14.0);
    let r = Rect::from_center_size(center, Vec2::new(w, 14.0));
    p.rect_filled(r, egui::CornerRadius::same(7), crate::theme::BADGE);
    p.text(center, egui::Align2::CENTER_CENTER, text, crate::theme::ui_semibold(9.5), crate::theme::MAIN_BG);
}

/// The app icon's pixel flame, one string per row (`deploy/macos/make-icon.swift`
/// draws the same grid; keep them in step).
const FLAME: [&str; 20] = [
    "........y.......",
    ".......yo.......",
    ".......oo.......",
    "......roo.......",
    "..o...rroo......",
    "..rr..rroo....o.",
    "..rro.rrooo..ro.",
    ".drrorrroooo.ro.",
    ".drroorroyoorro.",
    ".drroooroyyooro.",
    "ddrrooooyyyooroo",
    "drroooyyyyyyoorr",
    "drrooyyywyyyoorr",
    "drrooyywwwyyoord",
    "ddrooyywwwwyyord",
    ".drrooywwwwyyord",
    ".ddrooyywwyyoodd",
    "..ddrooyyyyoodd.",
    "...dddrooooddd..",
    ".....dddddd.....",
];

/// The pixel flame, `cell` points per pixel, its top-left at `min`. `flicker`
/// in 0..1 lifts the tips a row on alternate columns, so it can burn.
pub fn flame(p: &Painter, min: Pos2, cell: f32, flicker: f32) {
    for (y, row) in FLAME.iter().enumerate() {
        for (x, ch) in row.chars().enumerate() {
            let color = match ch {
                'd' => Color32::from_rgb(0x8a, 0x24, 0x10),
                'r' => Color32::from_rgb(0xd9, 0x43, 0x20),
                'o' => Color32::from_rgb(0xf0, 0x7a, 0x3c),
                'y' => Color32::from_rgb(0xff, 0xc2, 0x7a),
                'w' => Color32::from_rgb(0xff, 0xf4, 0xe4),
                _ => continue,
            };
            // Only the top third moves, a pixel at a time.
            let lift = if y < 7 && (x + (flicker * 4.0) as usize) % 3 == 0 { cell } else { 0.0 };
            let r = Rect::from_min_size(
                Pos2::new(min.x + x as f32 * cell, min.y + y as f32 * cell - lift),
                Vec2::splat(cell),
            );
            p.rect_filled(r, 0.0, color);
        }
    }
    // The burner under it.
    let burner = Rect::from_min_size(
        Pos2::new(min.x + 3.0 * cell, min.y + 21.0 * cell),
        Vec2::new(10.0 * cell, cell),
    );
    p.rect_filled(burner, 0.0, Color32::from_rgba_unmultiplied(0x4a, 0x46, 0x3c, 140));
}

/// The flame over a caption in the pixel face, centred in `rect`: the screen
/// shown while lmux connects or has nothing selected.
pub fn flame_screen(p: &Painter, rect: Rect, caption: &str, detail: &str, time: f64) {
    let cell = (rect.height() / 90.0).clamp(3.0, 6.0).round();
    let size = Vec2::new(16.0 * cell, 22.0 * cell);
    let min = rect.center() - Vec2::new(size.x / 2.0, size.y / 2.0 + 24.0);
    // Warm light around the core, in stepped rings like the icon's.
    let core = min + Vec2::new(8.0 * cell, 13.0 * cell);
    for (radius, a) in [(15.0, 6u8), (11.0, 8), (7.5, 10)] {
        p.circle_filled(core, radius * cell, Color32::from_rgba_unmultiplied(0xd9, 0x43, 0x20, a));
    }
    flame(p, min, cell, ((time * 3.0).floor() % 4.0) as f32 / 4.0);
    let y = min.y + size.y + 28.0;
    p.text(
        Pos2::new(rect.center().x, y),
        egui::Align2::CENTER_TOP,
        caption.to_uppercase(),
        crate::theme::pixel(11.0),
        crate::theme::FLAME,
    );
    if !detail.is_empty() {
        p.text(
            Pos2::new(rect.center().x, y + 20.0),
            egui::Align2::CENTER_TOP,
            detail,
            crate::theme::ui(crate::theme::FS_SMALL),
            crate::theme::DIM,
        );
    }
}
