//! Powerline symbols and the geometric shapes drawn as triangles, ported from Ghostty's
//! `src/font/sprite/draw/powerline.zig` and `geometric_shapes.zig` (MIT). The stylised
//! Powerline glyphs are left to the font; only the geometric ones are drawn here.

use std::f64::consts::SQRT_2;

use tiny_skia::{LineCap, LineJoin, Path, PathBuilder, Stroke};

use super::{Canvas, Thickness, ON};
use crate::gfx::metrics::Metrics;

type Pt = (f64, f64);

/// Draw `cp` if this module handles it; false otherwise.
pub fn draw(cp: u32, c: &mut Canvas, width: u32, height: u32, m: &Metrics) -> bool {
    let (fw, fh) = (width as f64, height as f64);
    match cp {
        // Solid triangles (powerline.zig:24-190). Vertices sit exactly on the cell corners.
        0xE0B0 => triangle(c, [(0.0, 0.0), (fw, fh / 2.0), (0.0, fh)]),
        0xE0B2 => triangle(c, [(fw, 0.0), (0.0, fh / 2.0), (fw, fh)]),
        0xE0B8 => triangle(c, [(0.0, 0.0), (fw, fh), (0.0, fh)]),
        0xE0BA => triangle(c, [(fw, 0.0), (fw, fh), (0.0, fh)]),
        0xE0BC => triangle(c, [(0.0, 0.0), (fw, 0.0), (0.0, fh)]),
        0xE0BE => triangle(c, [(0.0, 0.0), (fw, 0.0), (fw, fh)]),
        0xE0B9 | 0xE0BF => diagonal_upper_left_to_lower_right(c, m),
        0xE0BB | 0xE0BD => diagonal_upper_right_to_lower_left(c, m),
        0xE0B1 => chevron(c, fw, fh, m),
        0xE0B3 => {
            chevron(c, fw, fh, m);
            c.flip_h();
        }
        0xE0B4 => half_circle(c, fw, fh),
        0xE0B6 => {
            half_circle(c, fw, fh);
            c.flip_h();
        }
        0xE0B5 => half_circle_outline(c, fw, fh, m),
        0xE0B7 => {
            half_circle_outline(c, fw, fh, m);
            c.flip_h();
        }
        0xE0D2 => flame_notch(c, fw, fh, m),
        0xE0D4 => {
            flame_notch(c, fw, fh, m);
            c.flip_h();
        }
        // Geometric shapes (geometric_shapes.zig:26-82).
        0x25E2 => corner_triangle_shade(c, Corner::Br, m),
        0x25E3 => corner_triangle_shade(c, Corner::Bl, m),
        0x25E4 => corner_triangle_shade(c, Corner::Tl, m),
        0x25E5 => corner_triangle_shade(c, Corner::Tr, m),
        0x25F8 => corner_triangle_outline(c, Corner::Tl, m),
        0x25F9 => corner_triangle_outline(c, Corner::Tr, m),
        0x25FA => corner_triangle_outline(c, Corner::Bl, m),
        0x25FF => corner_triangle_outline(c, Corner::Br, m),
        _ => return false,
    }
    true
}

fn pt(p: Pt) -> (f32, f32) {
    (p.0 as f32, p.1 as f32)
}

fn triangle(c: &mut Canvas, p: [Pt; 3]) {
    c.triangle([pt(p[0]), pt(p[1]), pt(p[2])], ON);
}

fn fill_polygon(c: &mut Canvas, points: &[Pt]) {
    let mut pb = PathBuilder::new();
    for (i, &p) in points.iter().enumerate() {
        let (x, y) = pt(p);
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.close();
    if let Some(path) = pb.finish() {
        c.fill_path(&path, ON);
    }
}

/// Stroke with butt caps and miter joins limited as z2d's defaults are (`miter_limit` 10), where
/// tiny-skia's own default is 4: the point of a thin chevron would otherwise come out bevelled.
fn stroke_path(c: &mut Canvas, path: &Path, width: f32) {
    let stroke = Stroke {
        width,
        line_cap: LineCap::Butt,
        line_join: LineJoin::Miter,
        miter_limit: 10.0,
        ..Stroke::default()
    };
    c.composite(|pm, t| pm.stroke_path(path, &Canvas::paint(ON), &stroke, t, None));
}

// box.zig:638: the diagonals overshoot the cell corners by half a pixel along the slope, so that they
// join the neighbouring cell's diagonal without a notch. The cell size comes from the metrics.
fn diagonal_upper_right_to_lower_left(c: &mut Canvas, m: &Metrics) {
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let slope_x = (fw / fh).min(1.0);
    let slope_y = (fh / fw).min(1.0);
    let light = Thickness::Light.px(m.box_thickness) as f32;
    let (x0, y0) = pt((fw + 0.5 * slope_x, -0.5 * slope_y));
    let (x1, y1) = pt((-0.5 * slope_x, fh + 0.5 * slope_y));
    c.line(x0, y0, x1, y1, light, ON);
}

// box.zig:662
fn diagonal_upper_left_to_lower_right(c: &mut Canvas, m: &Metrics) {
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let slope_x = (fw / fh).min(1.0);
    let slope_y = (fh / fw).min(1.0);
    let light = Thickness::Light.px(m.box_thickness) as f32;
    let (x0, y0) = pt((-0.5 * slope_x, -0.5 * slope_y));
    let (x1, y1) = pt((fw + 0.5 * slope_x, fh + 0.5 * slope_y));
    c.line(x0, y0, x1, y1, light, ON);
}

// powerline.zig:194
fn chevron(c: &mut Canvas, fw: f64, fh: f64, m: &Metrics) {
    let mut pb = PathBuilder::new();
    pb.move_to(0.0, 0.0);
    pb.line_to(fw as f32, (fh / 2.0) as f32);
    pb.line_to(0.0, fh as f32);
    if let Some(path) = pb.finish() {
        stroke_path(c, &path, Thickness::Light.px(m.box_thickness) as f32);
    }
}

/// Coefficient for approximating a circular arc with a cubic.
const ARC: f64 = (SQRT_2 - 1.0) * 4.0 / 3.0;

// powerline.zig:235
fn half_circle(c: &mut Canvas, fw: f64, fh: f64) {
    let k = ARC;
    let radius = fw.min(fh / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(0.0, 0.0);
    pb.cubic_to(
        (radius * k) as f32,
        0.0,
        radius as f32,
        (radius - radius * k) as f32,
        radius as f32,
        radius as f32,
    );
    pb.line_to(radius as f32, (fh - radius) as f32);
    pb.cubic_to(
        radius as f32,
        (fh - radius + radius * k) as f32,
        (radius * k) as f32,
        fh as f32,
        0.0,
        fh as f32,
    );
    pb.close();
    if let Some(path) = pb.finish() {
        c.fill_path(&path, ON);
    }
}

fn cubic_point(p: [Pt; 4], t: f64) -> Pt {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * p[0].0 + b * p[1].0 + c * p[2].0 + d * p[3].0,
        a * p[0].1 + b * p[1].1 + c * p[2].1 + d * p[3].1,
    )
}

fn cubic_tangent(p: [Pt; 4], t: f64) -> Pt {
    let u = 1.0 - t;
    let (a, b, c) = (3.0 * u * u, 6.0 * u * t, 3.0 * t * t);
    (
        a * (p[1].0 - p[0].0) + b * (p[2].0 - p[1].0) + c * (p[3].0 - p[2].0),
        a * (p[1].1 - p[0].1) + b * (p[2].1 - p[1].1) + c * (p[3].1 - p[2].1),
    )
}

// powerline.zig:278: a thin outline of the half circle, drawn with `innerStrokePath` (canvas.zig:380), which
// offsets the path inward by half the line width and then strokes it, so the outer edge of the stroke lies
// on the original path and touches the cell edges.
fn half_circle_outline(c: &mut Canvas, fw: f64, fh: f64, m: &Metrics) {
    const STEPS: usize = 32;
    let thick = m.box_thickness as f64;
    let radius = fw.min(fh / 2.0);
    let k = ARC;

    // The path is walked clockwise on screen, so the inside of the shape is to the right of the direction of
    // travel: for a heading (dx, dy) in y-down coordinates that is (-dy, dx).
    let mut points: Vec<Pt> = Vec::with_capacity(2 * STEPS + 6);
    let mut heading: Pt = (1.0, 0.0);
    let mut push = |p: Pt, tangent: Pt| {
        let len = tangent.0.hypot(tangent.1);
        // A degenerate or reversed tangent (a tiny cell) keeps the previous heading.
        let backwards = tangent.0 * heading.0 + tangent.1 * heading.1 < 0.0;
        if len > 1e-9 && !backwards {
            heading = (tangent.0 / len, tangent.1 / len);
        }
        points.push((p.0 - heading.1 * thick / 2.0, p.1 + heading.0 * thick / 2.0));
    };

    // The first and last one-pixel lines enforce horizontal ends, so the offset stroke is butt-capped
    // exactly perpendicular; the curves' own end tangents would give a very slight slope.
    push((0.0, 0.0), (1.0, 0.0));
    let top = [(1.0, 0.0), (radius * k, 0.0), (radius, radius - radius * k), (radius, radius)];
    push(top[0], (1.0, 0.0));
    for i in 1..=STEPS {
        let t = i as f64 / STEPS as f64;
        push(cubic_point(top, t), cubic_tangent(top, t));
    }
    let bottom = [
        (radius, fh - radius),
        (radius, fh - radius + radius * k),
        (radius * k, fh),
        (1.0, fh),
    ];
    push(bottom[0], (0.0, 1.0));
    for i in 1..=STEPS {
        let t = i as f64 / STEPS as f64;
        push(cubic_point(bottom, t), cubic_tangent(bottom, t));
    }
    push((0.0, fh), (-1.0, 0.0));

    let mut pb = PathBuilder::new();
    for (i, &p) in points.iter().enumerate() {
        let (x, y) = pt(p);
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    if let Some(path) = pb.finish() {
        stroke_path(c, &path, thick as f32);
    }
}

// powerline.zig:352: two filled quads leaving a horizontal notch box_thickness tall.
fn flame_notch(c: &mut Canvas, fw: f64, fh: f64, m: &Metrics) {
    let thick = m.box_thickness as f64;
    let (upper, lower) = (fh / 2.0 - thick / 2.0, fh / 2.0 + thick / 2.0);
    fill_polygon(c, &[(0.0, 0.0), (fw, 0.0), (fw / 2.0, upper), (0.0, upper)]);
    fill_polygon(c, &[(0.0, fh), (fw, fh), (fw / 2.0, lower), (0.0, lower)]);
}

#[derive(Clone, Copy)]
enum Corner {
    Tl,
    Tr,
    Bl,
    Br,
}

/// The triangle that fills `corner`'s half of the cell (geometric_shapes.zig:85).
fn corner_triangle(corner: Corner, m: &Metrics) -> [Pt; 3] {
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    match corner {
        Corner::Tl => [(0.0, 0.0), (0.0, fh), (fw, 0.0)],
        Corner::Tr => [(0.0, 0.0), (fw, fh), (fw, 0.0)],
        Corner::Bl => [(0.0, 0.0), (0.0, fh), (fw, fh)],
        Corner::Br => [(0.0, fh), (fw, fh), (fw, 0.0)],
    }
}

// geometric_shapes.zig:85
fn corner_triangle_shade(c: &mut Canvas, corner: Corner, m: &Metrics) {
    triangle(c, corner_triangle(corner, m));
}

// geometric_shapes.zig:143: `innerStrokePath` of the closed triangle, light thickness, butt caps. The path is
// inset by half the width and stroked, so the band runs from the triangle's edge to `thickness` inside it:
// the triangle minus the triangle inset by the full thickness. Both are wound oppositely in one path.
fn corner_triangle_outline(c: &mut Canvas, corner: Corner, m: &Metrics) {
    let thick = Thickness::Light.px(m.box_thickness) as f64;
    let outer = corner_triangle(corner, m);
    let mut pb = PathBuilder::new();
    let (x, y) = pt(outer[0]);
    pb.move_to(x, y);
    for p in &outer[1..] {
        let (x, y) = pt(*p);
        pb.line_to(x, y);
    }
    pb.close();
    // A band thicker than the triangle is wide fills it.
    if let Some(inner) = inset_triangle(outer, thick) {
        let (x, y) = pt(inner[2]);
        pb.move_to(x, y);
        for p in [inner[1], inner[0]] {
            let (x, y) = pt(p);
            pb.line_to(x, y);
        }
        pb.close();
    }
    if let Some(path) = pb.finish() {
        c.fill_path(&path, ON);
    }
}

/// `p` moved inward by `d`: each edge shifted along its inward normal, adjacent edges intersected (a mitre).
/// None when `d` reaches the incircle and nothing is left.
fn inset_triangle(p: [Pt; 3], d: f64) -> Option<[Pt; 3]> {
    let cross = |a: Pt, b: Pt| a.0 * b.1 - a.1 * b.0;
    let area2 = cross((p[1].0 - p[0].0, p[1].1 - p[0].1), (p[2].0 - p[0].0, p[2].1 - p[0].1));
    let perimeter: f64 = (0..3).map(|i| (p[(i + 1) % 3].0 - p[i].0).hypot(p[(i + 1) % 3].1 - p[i].1)).sum();
    if area2 == 0.0 || d >= area2.abs() / perimeter {
        return None;
    }
    // Edge i runs p[i] -> p[i+1]; its offset line passes through `origin` with direction `dir`.
    let lines: Vec<(Pt, Pt)> = (0..3)
        .map(|i| {
            let (a, b) = (p[i], p[(i + 1) % 3]);
            let len = (b.0 - a.0).hypot(b.1 - a.1);
            let dir = ((b.0 - a.0) / len, (b.1 - a.1) / len);
            // The interior is on the left of a counter-clockwise edge and on the right of a clockwise one.
            let n = if area2 > 0.0 { (-dir.1, dir.0) } else { (dir.1, -dir.0) };
            ((a.0 + n.0 * d, a.1 + n.1 * d), dir)
        })
        .collect();
    // Vertex i is where edge i-1 meets edge i.
    let mut out = [(0.0, 0.0); 3];
    for (i, v) in out.iter_mut().enumerate() {
        let (po, pd) = lines[(i + 2) % 3];
        let (qo, qd) = lines[i];
        let a = cross((qo.0 - po.0, qo.1 - po.1), qd) / cross(pd, qd);
        *v = (po.0 + pd.0 * a, po.1 + pd.1 * a);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::sprite::{covers, UNDERLINE};

    fn metrics(w: u32, h: u32) -> Metrics {
        Metrics {
            cell_width: w,
            cell_height: h,
            cell_baseline: h / 4,
            underline_position: h - h / 4,
            underline_thickness: 1,
            strikethrough_position: h / 2,
            strikethrough_thickness: 1,
            overline_position: 0,
            overline_thickness: 1,
            box_thickness: 1,
            cursor_thickness: 1,
            cursor_height: h,
            icon_height: h as f64,
            icon_height_single: h as f64,
            face_width: w as f64,
            face_height: h as f64,
            face_y: 0.0,
        }
    }

    fn render(cp: u32, w: u32, h: u32) -> Canvas {
        let m = metrics(w, h);
        let mut c = Canvas::new(w, h);
        assert!(draw(cp, &mut c, w, h, &m), "U+{cp:04X} not drawn");
        c
    }

    const SIZES: [(u32, u32); 3] = [(9, 17), (18, 36), (10, 21)];

    /// Whether the cell area (not the padding) has anything drawn outside `[0,w)x[0,h)`.
    fn spills(c: &Canvas) -> bool {
        let (w, h) = (c.width as i32, c.height as i32);
        let (px, py) = (c.pad_x as i32, c.pad_y as i32);
        (-py..h + py).any(|y| (-px..w + px).any(|x| (x < 0 || y < 0 || x >= w || y >= h) && c.get(x, y) != 0))
    }

    fn mirrored(a: &Canvas, b: &Canvas, tolerance: i32) -> bool {
        let (w, h) = (a.width as i32, a.height as i32);
        (0..h).all(|y| (0..w).all(|x| (a.get(x, y) as i32 - b.get(w - 1 - x, y) as i32).abs() <= tolerance))
    }

    /// Mirror images up to the rasteriser's coverage quantisation, which is not symmetric: a pixel an edge
    /// splits exactly in half (a 45 degree diagonal) can come out 159 on one side and 96 on the other. The
    /// total coverage still agrees closely.
    fn mirrored_approx(a: &Canvas, b: &Canvas) -> bool {
        let (w, h) = (a.width as i32, a.height as i32);
        let (mut sum_a, mut sum_b, mut worst) = (0i64, 0i64, 0i32);
        for y in 0..h {
            for x in 0..w {
                let (va, vb) = (a.get(x, y) as i32, b.get(w - 1 - x, y) as i32);
                sum_a += va as i64;
                sum_b += vb as i64;
                worst = worst.max((va - vb).abs());
            }
        }
        worst <= 64 && (sum_a - sum_b).abs() * 100 <= sum_a * 3
    }

    #[test]
    fn right_triangle_touches_left_edge_and_reaches_the_tip() {
        for (w, h) in SIZES {
            let c = render(0xE0B0, w, h);
            let (wi, hi) = (w as i32, h as i32);
            assert!(c.get(0, 0) > 0, "{w}x{h}: top-left");
            assert!(c.get(0, hi - 1) > 0, "{w}x{h}: bottom-left");
            assert_eq!(c.get(0, hi / 2), ON, "{w}x{h}: left edge is solid at mid height");
            let tip = c.get(wi - 1, hi / 2).max(c.get(wi - 1, (hi - 1) / 2));
            assert!(tip > 0, "{w}x{h}: tip at mid height");
            // The corners opposite the tip are empty.
            assert_eq!(c.get(wi - 1, 0), 0, "{w}x{h}");
            assert_eq!(c.get(wi - 1, hi - 1), 0, "{w}x{h}");
            assert!(!spills(&c), "{w}x{h}");
        }
    }

    #[test]
    fn left_triangle_mirrors_the_right_one() {
        for (w, h) in SIZES {
            let (a, b) = (render(0xE0B0, w, h), render(0xE0B2, w, h));
            assert!(mirrored_approx(&a, &b), "{w}x{h}");
            assert_eq!(b.get(w as i32 - 1, h as i32 / 2), ON);
            assert!(b.get(w as i32 - 1, 0) > 0 && b.get(w as i32 - 1, h as i32 - 1) > 0);
        }
    }

    #[test]
    fn corner_triangles_fill_their_corner_and_leave_the_opposite_one() {
        for (w, h) in SIZES {
            let (wi, hi) = (w as i32, h as i32);
            let cases = [
                (0xE0B8, (0, hi - 1), (wi - 1, 0)),
                (0xE0BA, (wi - 1, hi - 1), (0, 0)),
                (0xE0BC, (0, 0), (wi - 1, hi - 1)),
                (0xE0BE, (wi - 1, 0), (0, hi - 1)),
            ];
            for (cp, on, off) in cases {
                let c = render(cp, w, h);
                assert_eq!(c.get(on.0, on.1), ON, "U+{cp:04X} {w}x{h}");
                assert_eq!(c.get(off.0, off.1), 0, "U+{cp:04X} {w}x{h}");
                assert!(!spills(&c));
            }
        }
    }

    #[test]
    fn round_cap_fills_the_left_edge_and_mirrors() {
        for (w, h) in SIZES {
            let (wi, hi) = (w as i32, h as i32);
            let c = render(0xE0B4, w, h);
            assert!((0..hi).all(|y| c.get(0, y) == ON), "{w}x{h}: the flat side is solid");
            // It reaches the full radius at mid height, which is min(w, h/2).
            let r = (w as f64).min(h as f64 / 2.0);
            assert!(c.get(r as i32 - 1, hi / 2) > 0, "{w}x{h}");
            assert_eq!(c.get(wi - 1, 0), 0, "{w}x{h}: top-right is outside the curve");
            let d = render(0xE0B6, w, h);
            assert!(mirrored(&c, &d, 0), "{w}x{h}");
            assert!(!spills(&c));
        }
    }

    #[test]
    fn round_outline_hugs_the_cell_edges_and_leaves_the_inside_empty() {
        for (w, h) in SIZES {
            let hi = h as i32;
            let c = render(0xE0B5, w, h);
            // A one-pixel stroke touches the top and bottom rows from the left edge.
            for (x, least) in [(0, 250), (1, 150)] {
                assert!(c.get(x, 0) >= least, "{w}x{h}: top ({x},0) = {}", c.get(x, 0));
                assert!(c.get(x, hi - 1) >= least, "{w}x{h}: bottom ({x},{}) = {}", hi - 1, c.get(x, hi - 1));
            }
            // Nothing in the interior beside the left edge, and the stroke stays out of the padding.
            assert_eq!(c.get(0, hi / 2), 0, "{w}x{h}");
            assert_eq!(c.get(1, hi / 2), 0, "{w}x{h}");
            assert!(!spills(&c), "{w}x{h}");
            // The straight run of the stroke sits just left of the radius.
            let r = (w as f64).min(h as f64 / 2.0).round() as i32;
            assert!(c.get(r - 1, hi / 2) > 0, "{w}x{h}");
            assert!(c.get(r - 1, 0) == 0 || r > 2, "{w}x{h}");
            let d = render(0xE0B7, w, h);
            assert!(mirrored(&c, &d, 0), "{w}x{h}");
        }
    }

    #[test]
    fn thick_outline_gets_thicker_inwards_only() {
        let mut m = metrics(18, 36);
        m.box_thickness = 3;
        let mut c = Canvas::new(18, 36);
        assert!(draw(0xE0B5, &mut c, 18, 36, &m));
        for y in 0..3 {
            assert!(c.get(0, y) >= 250, "row {y}");
        }
        assert_eq!(c.get(0, 3), 0);
        assert!(!spills(&c));
    }

    #[test]
    fn chevron_runs_corner_to_tip_and_mirrors() {
        for (w, h) in SIZES {
            let (wi, hi) = (w as i32, h as i32);
            let c = render(0xE0B1, w, h);
            assert!(c.get(0, 0) > 0, "{w}x{h}");
            assert!(c.get(0, hi - 1) > 0, "{w}x{h}");
            let tip = (0..hi).map(|y| c.get(wi - 1, y) as u32).sum::<u32>();
            assert!(tip > 0, "{w}x{h}: reaches the right edge");
            // Left of the line at mid height is empty.
            assert_eq!(c.get(0, hi / 2), 0, "{w}x{h}");
            let d = render(0xE0B3, w, h);
            assert!(mirrored(&c, &d, 0), "{w}x{h}");
        }
    }

    #[test]
    fn diagonals_join_the_corners() {
        for (w, h) in SIZES {
            let (wi, hi) = (w as i32, h as i32);
            let down = render(0xE0B9, w, h); // upper left to lower right
            let up = render(0xE0BB, w, h); // upper right to lower left
            assert!(down.get(0, 0) > 0 && down.get(wi - 1, hi - 1) > 0, "{w}x{h}");
            assert!(up.get(wi - 1, 0) > 0 && up.get(0, hi - 1) > 0, "{w}x{h}");
            assert_eq!(down.get(wi - 1, 0), 0, "{w}x{h}");
            assert_eq!(up.get(0, 0), 0, "{w}x{h}");
            // The two cross at the middle of the cell and are mirror images.
            assert!(mirrored(&down, &up, 8), "{w}x{h}");
            // Same lines under the other two code points.
            assert_eq!(render(0xE0BF, w, h).buf, down.buf);
            assert_eq!(render(0xE0BD, w, h).buf, up.buf);
        }
    }

    #[test]
    fn notched_shape_leaves_a_gap_of_box_thickness() {
        for (w, h) in SIZES {
            let (wi, hi) = (w as i32, h as i32);
            let c = render(0xE0D2, w, h);
            // Down the left edge exactly one thickness of coverage is missing, split over two rows when
            // the cell height is even.
            let missing: i32 = (0..hi).map(|y| 255 - c.get(0, y) as i32).sum();
            assert!((missing - 255).abs() <= 8, "{w}x{h}: {missing}");
            assert_eq!(c.get(0, 0), ON);
            assert_eq!(c.get(0, hi - 1), ON);
            // The pieces run out to the right edge on the top and bottom rows.
            assert!(c.get(wi - 1, 0) > 0, "{w}x{h}");
            assert!(c.get(wi - 1, hi - 1) > 0, "{w}x{h}");
            // The notch narrows both pieces to a point at the middle of the right edge.
            assert_eq!(c.get(wi - 1, hi / 2), 0);
            let d = render(0xE0D4, w, h);
            assert!(mirrored(&c, &d, 0), "{w}x{h}");
        }
    }

    #[test]
    fn solid_corner_triangles_cover_half_the_cell() {
        for (w, h) in SIZES {
            let (wi, hi) = (w as i32, h as i32);
            let br = render(0x25E2, w, h);
            let bl = render(0x25E3, w, h);
            let tl = render(0x25E4, w, h);
            let tr = render(0x25E5, w, h);
            assert_eq!(br.get(wi - 1, hi - 1), ON, "{w}x{h}");
            assert_eq!(br.get(0, 0), 0, "{w}x{h}");
            assert_eq!(bl.get(0, hi - 1), ON, "{w}x{h}");
            assert_eq!(tl.get(0, 0), ON, "{w}x{h}");
            assert_eq!(tr.get(wi - 1, 0), ON, "{w}x{h}");
            assert_eq!(tr.get(0, hi - 1), 0, "{w}x{h}");
            // A triangle and its complement along the same diagonal add up to the full cell.
            for y in 0..hi {
                for x in 0..wi {
                    let sum = br.get(x, y) as i32 + tl.get(x, y) as i32;
                    assert!((sum - 255).abs() <= 24, "{w}x{h} ({x},{y}): {sum}");
                }
            }
            assert!(mirrored_approx(&br, &bl), "{w}x{h}");
            assert!(!spills(&br));
        }
    }

    #[test]
    fn outlined_corner_triangles_are_hollow() {
        for (w, h) in SIZES {
            let (wi, hi) = (w as i32, h as i32);
            let tl = render(0x25F8, w, h);
            // Legs along the top and left edge, solid; the hypotenuse crosses the middle.
            assert!(tl.get(0, 0) >= 250, "{w}x{h}");
            assert!((0..wi / 2).all(|x| tl.get(x, 0) >= 250), "{w}x{h}: top leg");
            assert!((0..hi / 2).all(|y| tl.get(0, y) >= 250), "{w}x{h}: left leg");
            assert!(tl.get(0, hi - 1) > 0 && tl.get(wi - 1, 0) > 0, "{w}x{h}: ends");
            assert!(tl.get(wi / 2, hi / 2) > 0 || tl.get(wi / 2 - 1, hi / 2) > 0, "{w}x{h}: hypotenuse");
            // Hollow: just inside the corner, away from every edge.
            assert_eq!(tl.get(1, 1), 0, "{w}x{h}");
            assert_eq!(tl.get(wi / 4, hi / 4), 0, "{w}x{h}");
            // Nothing below the hypotenuse.
            assert_eq!(tl.get(wi - 1, hi - 1), 0, "{w}x{h}");
            assert!(!spills(&tl), "{w}x{h}");

            // The other corners are the same outline moved.
            let br = render(0x25FF, w, h);
            assert!(br.get(wi - 1, hi - 1) >= 250 && br.get(wi - 1, 0) > 0 && br.get(0, hi - 1) > 0);
            assert_eq!(br.get(wi - 2, hi - 2), 0, "{w}x{h}");
            let tr = render(0x25F9, w, h);
            assert!(tr.get(wi - 1, 0) >= 250 && tr.get(0, 0) > 0 && tr.get(wi - 1, hi - 1) > 0);
            let bl = render(0x25FA, w, h);
            assert!(bl.get(0, hi - 1) >= 250 && bl.get(0, 0) > 0 && bl.get(wi - 1, hi - 1) > 0);
        }
    }

    #[test]
    fn outline_thickness_follows_box_thickness() {
        let mut m = metrics(18, 36);
        m.box_thickness = 2;
        let mut c = Canvas::new(18, 36);
        assert!(draw(0x25F8, &mut c, 18, 36, &m));
        assert!(c.get(0, 0) >= 250 && c.get(5, 1) >= 250);
        assert_eq!(c.get(5, 2), 0);
    }

    #[test]
    fn unhandled_codepoints_are_left_alone() {
        let m = metrics(9, 17);
        for cp in [0x2500, 0x25E1, 0x25F7, 0x25FE, 0xE0B0 - 1, 0xE0C0, 0xE0D3, 0xF5D0, UNDERLINE] {
            let mut c = Canvas::new(9, 17);
            assert!(!draw(cp, &mut c, 9, 17, &m), "U+{cp:04X}");
            assert!(c.buf.iter().all(|&v| v == 0));
        }
    }

    #[test]
    fn everything_drawn_here_is_covered_by_the_dispatcher() {
        let m = metrics(9, 17);
        let mut drawn = 0;
        for cp in 0x2500..=0xE0FF_u32 {
            let mut c = Canvas::new(9, 17);
            if draw(cp, &mut c, 9, 17, &m) {
                assert!(covers(cp), "U+{cp:04X} is drawn here but not covered");
                assert!(c.buf.iter().any(|&v| v != 0), "U+{cp:04X} came out empty");
                drawn += 1;
            }
        }
        // E0B0..E0BF and E0D2 and E0D4, plus 25E2..25E5, 25F8..25FA, 25FF.
        assert_eq!(drawn, 16 + 2 + 4 + 3 + 1);
    }

    #[test]
    fn inset_triangle_is_the_edge_shifted_inward() {
        // A right triangle with legs 8 and 6 (hypotenuse 10): inradius 2. Inset by 1 it keeps its shape,
        // scaled by (2-1)/2 around the incentre.
        let p = [(0.0, 0.0), (0.0, 6.0), (8.0, 0.0)];
        let q = inset_triangle(p, 1.0).unwrap();
        let near = |a: Pt, b: Pt| (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9;
        assert!(near(q[0], (1.0, 1.0)), "{q:?}");
        assert!(near(q[1], (1.0, 4.0)), "{q:?}");
        assert!(near(q[2], (5.0, 1.0)), "{q:?}");
        assert!(inset_triangle(p, 2.0).is_none());
    }
}
