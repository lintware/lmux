//! Box drawing, U+2500..=U+257F, ported from Ghostty's `src/font/sprite/draw/box.zig` (MIT).
//!
//! Straight lines and their junctions are plain rectangles, so neighbouring cells meet exactly. Dashes, rounded
//! corners and diagonals are the only antialiased pieces.

use tiny_skia::{LineCap, PathBuilder};

use super::{Canvas, Thickness, ON};
use crate::gfx::metrics::Metrics;

// A line style from the cell centre towards one edge (`Lines.Style`, box.zig:36).
const NONE: u8 = 0;
const LIGHT: u8 = 1;
const HEAVY: u8 = 2;
const DOUBLE: u8 = 3;

/// The style of each of the four arms of a character (`Lines`, box.zig:32).
#[derive(Clone, Copy, Default)]
struct Lines {
    up: u8,
    right: u8,
    down: u8,
    left: u8,
}

impl Lines {
    /// Unpack `up << 6 | right << 4 | down << 2 | left`.
    fn unpack(v: u8) -> Lines {
        Lines { up: v >> 6 & 3, right: v >> 4 & 3, down: v >> 2 & 3, left: v & 3 }
    }
}

/// `Lines` for every codepoint from U+2500, packed as in [`Lines::unpack`]. The 0xFF entries are placeholders for the
/// dashes, arcs and diagonals, which `draw` handles before the lookup; the one real 0xFF is U+256C.
#[rustfmt::skip]
const TABLE: [u8; 128] = [
    0x11, 0x22, 0x44, 0x88, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x14, 0x24, 0x18, 0x28,
    0x05, 0x06, 0x09, 0x0A, 0x50, 0x60, 0x90, 0xA0, 0x41, 0x42, 0x81, 0x82, 0x54, 0x64, 0x94, 0x58,
    0x98, 0xA4, 0x68, 0xA8, 0x45, 0x46, 0x85, 0x49, 0x89, 0x86, 0x4A, 0x8A, 0x15, 0x16, 0x25, 0x26,
    0x19, 0x1A, 0x29, 0x2A, 0x51, 0x52, 0x61, 0x62, 0x91, 0x92, 0xA1, 0xA2, 0x55, 0x56, 0x65, 0x66,
    0x95, 0x59, 0x99, 0x96, 0xA5, 0x5A, 0x69, 0xA6, 0x6A, 0x9A, 0xA9, 0xAA, 0xFF, 0xFF, 0xFF, 0xFF,
    0x33, 0xCC, 0x34, 0x1C, 0x3C, 0x07, 0x0D, 0x0F, 0x70, 0xD0, 0xF0, 0x43, 0xC1, 0xC3, 0x74, 0xDC,
    0xFC, 0x47, 0xCD, 0xCF, 0x37, 0x1D, 0x3F, 0x73, 0xD1, 0xF3, 0x77, 0xDD, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0x01, 0x40, 0x10, 0x04, 0x02, 0x80, 0x20, 0x08, 0x21, 0x48, 0x12, 0x84,
];

#[derive(Clone, Copy)]
enum Corner {
    Tl,
    Tr,
    Bl,
    Br,
}

/// Draw `cp` if it is a box drawing character; false otherwise (`draw2500_257F`, box.zig:48).
pub fn draw(cp: u32, c: &mut Canvas, _width: u32, _height: u32, m: &Metrics) -> bool {
    if !(0x2500..=0x257F).contains(&cp) {
        return false;
    }
    let light = Thickness::Light.px(m.box_thickness);
    let heavy = Thickness::Heavy.px(m.box_thickness);
    let gap = light.max(4);
    match cp {
        0x2504 => dash_horizontal(m, c, 3, light, gap),
        0x2505 => dash_horizontal(m, c, 3, heavy, gap),
        0x2506 => dash_vertical(m, c, 3, light, gap),
        0x2507 => dash_vertical(m, c, 3, heavy, gap),
        0x2508 => dash_horizontal(m, c, 4, light, gap),
        0x2509 => dash_horizontal(m, c, 4, heavy, gap),
        0x250A => dash_vertical(m, c, 4, light, gap),
        0x250B => dash_vertical(m, c, 4, heavy, gap),
        0x254C => dash_horizontal(m, c, 2, light, light),
        0x254D => dash_horizontal(m, c, 2, heavy, heavy),
        0x254E => dash_vertical(m, c, 2, light, heavy),
        0x254F => dash_vertical(m, c, 2, heavy, heavy),
        0x256D => arc(m, c, Corner::Br, Thickness::Light),
        0x256E => arc(m, c, Corner::Bl, Thickness::Light),
        0x256F => arc(m, c, Corner::Tl, Thickness::Light),
        0x2570 => arc(m, c, Corner::Tr, Thickness::Light),
        0x2571 => light_diagonal_ur_ll(c, m),
        0x2572 => light_diagonal_ul_lr(c, m),
        0x2573 => {
            light_diagonal_ur_ll(c, m);
            light_diagonal_ul_lr(c, m);
        }
        _ => lines_char(m, c, Lines::unpack(TABLE[(cp - 0x2500) as usize])),
    }
    true
}

/// Intersection-style characters: each arm has its own style and the arms are cut so that every junction closes
/// (`linesChar`, box.zig:399).
fn lines_char(m: &Metrics, c: &mut Canvas, lines: Lines) {
    let light_px = Thickness::Light.px(m.box_thickness);
    let heavy_px = Thickness::Heavy.px(m.box_thickness);
    let (cw, ch) = (m.cell_width, m.cell_height);

    // Top of light horizontal strokes, and the bottom.
    let h_light_top = ch.saturating_sub(light_px) / 2;
    let h_light_bottom = h_light_top.saturating_add(light_px);

    // Top and bottom of heavy horizontal strokes.
    let h_heavy_top = ch.saturating_sub(heavy_px) / 2;
    let h_heavy_bottom = h_heavy_top.saturating_add(heavy_px);

    // Top of the upper doubled stroke (its bottom is `h_light_top`) and bottom of the lower one (its top is
    // `h_light_bottom`).
    let h_double_top = h_light_top.saturating_sub(light_px);
    let h_double_bottom = h_light_bottom.saturating_add(light_px);

    // Left and right of light vertical strokes.
    let v_light_left = cw.saturating_sub(light_px) / 2;
    let v_light_right = v_light_left.saturating_add(light_px);

    // Left and right of heavy vertical strokes.
    let v_heavy_left = cw.saturating_sub(heavy_px) / 2;
    let v_heavy_right = v_heavy_left.saturating_add(heavy_px);

    // Left of the left doubled stroke (its right is `v_light_left`) and right of the right one (its left is
    // `v_light_right`).
    let v_double_left = v_light_left.saturating_sub(light_px);
    let v_double_right = v_light_right.saturating_add(light_px);

    let Lines { up, right, down, left } = lines;

    // The bottom of the up line.
    let up_bottom = if left == HEAVY || right == HEAVY {
        h_heavy_bottom
    } else if left != right || down == up {
        if left == DOUBLE || right == DOUBLE {
            h_double_bottom
        } else {
            h_light_bottom
        }
    } else if left == NONE && right == NONE {
        h_light_bottom
    } else {
        h_light_top
    };

    // The top of the down line.
    let down_top = if left == HEAVY || right == HEAVY {
        h_heavy_top
    } else if left != right || up == down {
        if left == DOUBLE || right == DOUBLE {
            h_double_top
        } else {
            h_light_top
        }
    } else if left == NONE && right == NONE {
        h_light_top
    } else {
        h_light_bottom
    };

    // The right of the left line.
    let left_right = if up == HEAVY || down == HEAVY {
        v_heavy_right
    } else if up != down || left == right {
        if up == DOUBLE || down == DOUBLE {
            v_double_right
        } else {
            v_light_right
        }
    } else if up == NONE && down == NONE {
        v_light_right
    } else {
        v_light_left
    };

    // The left of the right line.
    let right_left = if up == HEAVY || down == HEAVY {
        v_heavy_left
    } else if up != down || right == left {
        if up == DOUBLE || down == DOUBLE {
            v_double_left
        } else {
            v_light_left
        }
    } else if up == NONE && down == NONE {
        v_light_left
    } else {
        v_light_right
    };

    let b = |c: &mut Canvas, x0: u32, y0: u32, x1: u32, y1: u32| {
        c.r#box(x0 as i32, y0 as i32, x1 as i32, y1 as i32, ON);
    };

    match up {
        LIGHT => b(c, v_light_left, 0, v_light_right, up_bottom),
        HEAVY => b(c, v_heavy_left, 0, v_heavy_right, up_bottom),
        DOUBLE => {
            let left_bottom = if left == DOUBLE { h_light_top } else { up_bottom };
            let right_bottom = if right == DOUBLE { h_light_top } else { up_bottom };
            b(c, v_double_left, 0, v_light_left, left_bottom);
            b(c, v_light_right, 0, v_double_right, right_bottom);
        }
        _ => {}
    }

    match right {
        LIGHT => b(c, right_left, h_light_top, cw, h_light_bottom),
        HEAVY => b(c, right_left, h_heavy_top, cw, h_heavy_bottom),
        DOUBLE => {
            let top_left = if up == DOUBLE { v_light_right } else { right_left };
            let bottom_left = if down == DOUBLE { v_light_right } else { right_left };
            b(c, top_left, h_double_top, cw, h_light_top);
            b(c, bottom_left, h_light_bottom, cw, h_double_bottom);
        }
        _ => {}
    }

    match down {
        LIGHT => b(c, v_light_left, down_top, v_light_right, ch),
        HEAVY => b(c, v_heavy_left, down_top, v_heavy_right, ch),
        DOUBLE => {
            let left_top = if left == DOUBLE { h_light_bottom } else { down_top };
            let right_top = if right == DOUBLE { h_light_bottom } else { down_top };
            b(c, v_double_left, left_top, v_light_left, ch);
            b(c, v_light_right, right_top, v_double_right, ch);
        }
        _ => {}
    }

    match left {
        LIGHT => b(c, 0, h_light_top, left_right, h_light_bottom),
        HEAVY => b(c, 0, h_heavy_top, left_right, h_heavy_bottom),
        DOUBLE => {
            let top_right = if up == DOUBLE { v_light_left } else { left_right };
            let bottom_right = if down == DOUBLE { v_light_left } else { left_right };
            b(c, 0, h_double_top, top_right, h_light_top);
            b(c, 0, h_light_bottom, bottom_right, h_double_bottom);
        }
        _ => {}
    }
}

/// Stroke `p0 -> p1` with the light thickness, overshooting each end by half a pixel along the slope.
fn diagonal(c: &mut Canvas, m: &Metrics, p0: (f64, f64), p1: (f64, f64)) {
    c.line(p0.0 as f32, p0.1 as f32, p1.0 as f32, p1.1 as f32, Thickness::Light.px(m.box_thickness) as f32, ON);
}

/// The overshoot per axis that keeps the diagonal's slope while it pokes past the corners.
fn slopes(m: &Metrics) -> (f64, f64) {
    let (w, h) = (m.cell_width as f64, m.cell_height as f64);
    ((w / h).min(1.0), (h / w).min(1.0))
}

/// `╱` (`lightDiagonalUpperRightToLowerLeft`, box.zig:638).
pub fn light_diagonal_ur_ll(c: &mut Canvas, m: &Metrics) {
    let (w, h) = (m.cell_width as f64, m.cell_height as f64);
    let (slope_x, slope_y) = slopes(m);
    diagonal(c, m, (w + 0.5 * slope_x, -0.5 * slope_y), (-0.5 * slope_x, h + 0.5 * slope_y));
}

/// `╲` (`lightDiagonalUpperLeftToLowerRight`, box.zig:662).
pub fn light_diagonal_ul_lr(c: &mut Canvas, m: &Metrics) {
    let (w, h) = (m.cell_width as f64, m.cell_height as f64);
    let (slope_x, slope_y) = slopes(m);
    diagonal(c, m, (-0.5 * slope_x, -0.5 * slope_y), (w + 0.5 * slope_x, h + 0.5 * slope_y));
}

/// A rounded corner: a straight stem from the cell edge, a cubic with control points a quarter of the radius
/// from the centre, and a straight arm to the other edge (`arc`, box.zig:694).
fn arc(m: &Metrics, c: &mut Canvas, corner: Corner, thickness: Thickness) {
    let thick_px = thickness.px(m.box_thickness);
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let thick = thick_px as f64;
    let cx = (m.cell_width.saturating_sub(thick_px) / 2) as f64 + thick / 2.0;
    let cy = (m.cell_height.saturating_sub(thick_px) / 2) as f64 + thick / 2.0;
    let r = fw.min(fh) / 2.0;
    // Fraction away from the centre to place the middle control points.
    let s = 0.25;

    let mut pb = PathBuilder::new();
    let f = |v: f64| v as f32;
    match corner {
        Corner::Tl => {
            pb.move_to(f(cx), 0.0);
            pb.line_to(f(cx), f(cy - r));
            pb.cubic_to(f(cx), f(cy - s * r), f(cx - s * r), f(cy), f(cx - r), f(cy));
            pb.line_to(0.0, f(cy));
        }
        Corner::Tr => {
            pb.move_to(f(cx), 0.0);
            pb.line_to(f(cx), f(cy - r));
            pb.cubic_to(f(cx), f(cy - s * r), f(cx + s * r), f(cy), f(cx + r), f(cy));
            pb.line_to(f(fw), f(cy));
        }
        Corner::Bl => {
            pb.move_to(f(cx), f(fh));
            pb.line_to(f(cx), f(cy + r));
            pb.cubic_to(f(cx), f(cy + s * r), f(cx - s * r), f(cy), f(cx - r), f(cy));
            pb.line_to(0.0, f(cy));
        }
        Corner::Br => {
            pb.move_to(f(cx), f(fh));
            pb.line_to(f(cx), f(cy + r));
            pb.cubic_to(f(cx), f(cy + s * r), f(cx + s * r), f(cy), f(cx + r), f(cy));
            pb.line_to(f(fw), f(cy));
        }
    }
    if let Some(path) = pb.finish() {
        c.stroke_path(&path, thick as f32, LineCap::Butt, ON);
    }
}

/// `count` dashes with half-sized gaps at both ends, so tiled cells read as one even line
/// (`dashHorizontal`, box.zig:779).
fn dash_horizontal(m: &Metrics, c: &mut Canvas, count: u32, thick_px: u32, desired_gap: u32) {
    let (cell_width, cell_height) = (m.cell_width, m.cell_height);
    // N dashes have N - 1 gaps between them plus half a gap on each side.
    let gap_count = count;

    // Not even a pixel per dash and gap: a solid line instead.
    if cell_width < count + gap_count {
        hline_middle(m, c, Thickness::Light);
        return;
    }

    // Gaps never take more than half the space.
    let gap_width = desired_gap.min(cell_width / (2 * count)) as i32;
    let total_gap_width = gap_count as i32 * gap_width;
    let total_dash_width = cell_width as i32 - total_gap_width;
    let dash_width = total_dash_width.div_euclid(count as i32);
    let remaining = total_dash_width.rem_euclid(count as i32);

    // Dashes are centred vertically.
    let y = (cell_height.saturating_sub(thick_px) / 2) as i32;

    // Start half a gap in, to centre the dashes.
    let mut x = gap_width.div_euclid(2);

    // Leftover pixels go to the dashes one at a time; that is less visible than larger gaps.
    let mut extra = remaining;
    for _ in 0..count {
        let mut x1 = x + dash_width;
        if extra > 0 {
            extra -= 1;
            x1 += 1;
        }
        hline(c, x, x1, y, thick_px);
        x = x1 + gap_width;
    }
}

/// `count` dashes from the top with one full-sized extra gap at the bottom, which joins solid lines better than two
/// half gaps (`dashVertical`, box.zig:866).
fn dash_vertical(m: &Metrics, c: &mut Canvas, count: u32, thick_px: u32, desired_gap: u32) {
    let (cell_width, cell_height) = (m.cell_width, m.cell_height);
    // The extra gap at the bottom makes as many gaps as dashes.
    let gap_count = count;

    if cell_height < count + gap_count {
        vline_middle(m, c, Thickness::Light);
        return;
    }

    let gap_height = desired_gap.min(cell_height / (2 * count)) as i32;
    let total_gap_height = gap_count as i32 * gap_height;
    let total_dash_height = cell_height as i32 - total_gap_height;
    let dash_height = total_dash_height.div_euclid(count as i32);
    let remaining = total_dash_height.rem_euclid(count as i32);

    // Dashes are centred horizontally.
    let x = (cell_width.saturating_sub(thick_px) / 2) as i32;

    let mut y = 0;
    let mut extra = remaining;
    for _ in 0..count {
        let mut y1 = y + dash_height;
        if extra > 0 {
            extra -= 1;
            y1 += 1;
        }
        vline(c, y, y1, x, thick_px);
        y = y1 + gap_height;
    }
}

/// A horizontal line with its top edge at `y` (`common.zig` `hline`).
fn hline(c: &mut Canvas, x1: i32, x2: i32, y: i32, thickness_px: u32) {
    c.r#box(x1, y, x2, y + thickness_px as i32, ON);
}

/// A vertical line with its left edge at `x` (`common.zig` `vline`).
fn vline(c: &mut Canvas, y1: i32, y2: i32, x: i32, thickness_px: u32) {
    c.r#box(x, y1, x + thickness_px as i32, y2, ON);
}

/// A full-width line through the middle of the cell (`common.zig` `hlineMiddle`).
fn hline_middle(m: &Metrics, c: &mut Canvas, thickness: Thickness) {
    let thick_px = thickness.px(m.box_thickness);
    hline(c, 0, m.cell_width as i32, (m.cell_height.saturating_sub(thick_px) / 2) as i32, thick_px);
}

/// A full-height line through the middle of the cell (`common.zig` `vlineMiddle`).
fn vline_middle(m: &Metrics, c: &mut Canvas, thickness: Thickness) {
    let thick_px = thickness.px(m.box_thickness);
    vline(c, 0, m.cell_height as i32, (m.cell_width.saturating_sub(thick_px) / 2) as i32, thick_px);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [(u32, u32, u32); 4] = [(9, 17, 1), (11, 21, 2), (12, 24, 3), (18, 36, 4)];

    fn metrics(w: u32, h: u32, t: u32) -> Metrics {
        Metrics {
            cell_width: w,
            cell_height: h,
            cell_baseline: h / 4,
            underline_position: h - h / 4,
            underline_thickness: t,
            strikethrough_position: h / 2,
            strikethrough_thickness: t,
            overline_position: 0,
            overline_thickness: t,
            box_thickness: t,
            cursor_thickness: 1,
            cursor_height: h,
            icon_height: h as f64,
            icon_height_single: h as f64,
            face_width: w as f64,
            face_height: h as f64,
            face_y: 0.0,
        }
    }

    fn render(cp: u32, w: u32, h: u32, t: u32) -> Canvas {
        let m = metrics(w, h, t);
        let mut c = Canvas::new(w, h);
        assert!(draw(cp, &mut c, w, h, &m));
        c
    }

    fn on(c: &Canvas, x: i32, y: i32) -> bool {
        c.get(x, y) == ON
    }

    /// The rows where column `x` is lit.
    fn rows(c: &Canvas, x: i32) -> Vec<i32> {
        (0..c.height as i32).filter(|&y| on(c, x, y)).collect()
    }

    /// The columns where row `y` is lit.
    fn cols(c: &Canvas, y: i32) -> Vec<i32> {
        (0..c.width as i32).filter(|&x| on(c, x, y)).collect()
    }

    /// The (first, last) lit rows of the cell's middle column and the (first, last) lit columns of its middle row
    /// of a light line.
    fn light_h_rows(w: u32, h: u32, t: u32) -> Vec<i32> {
        rows(&render(0x2500, w, h, t), 0)
    }

    fn light_v_cols(w: u32, h: u32, t: u32) -> Vec<i32> {
        cols(&render(0x2502, w, h, t), 0)
    }

    #[test]
    fn only_the_box_block_is_claimed() {
        let m = metrics(9, 17, 1);
        let mut c = Canvas::new(9, 17);
        assert!(!draw(0x24FF, &mut c, 9, 17, &m));
        assert!(!draw(0x2580, &mut c, 9, 17, &m));
        assert!(c.buf.iter().all(|&v| v == 0));
        assert!(draw(0x2500, &mut c, 9, 17, &m));
        assert!(draw(0x257F, &mut c, 9, 17, &m));
    }

    #[test]
    fn horizontal_line_touches_both_edges_on_the_same_rows() {
        for (w, h, t) in SIZES {
            let c = render(0x2500, w, h, t);
            let left = rows(&c, 0);
            assert_eq!(left.len() as u32, t, "{w}x{h}");
            assert_eq!(left, rows(&c, w as i32 - 1), "{w}x{h}");
            // Every column in between carries the same rows.
            for x in 0..w as i32 {
                assert_eq!(rows(&c, x), left, "{w}x{h} x={x}");
            }
            let top = ((h - t) / 2) as i32;
            assert_eq!(left, (top..top + t as i32).collect::<Vec<_>>());
        }
    }

    #[test]
    fn vertical_line_touches_both_edges_on_the_same_columns() {
        for (w, h, t) in SIZES {
            let c = render(0x2502, w, h, t);
            let top = cols(&c, 0);
            assert_eq!(top.len() as u32, t, "{w}x{h}");
            assert_eq!(top, cols(&c, h as i32 - 1), "{w}x{h}");
            for y in 0..h as i32 {
                assert_eq!(cols(&c, y), top, "{w}x{h} y={y}");
            }
            let left = ((w - t) / 2) as i32;
            assert_eq!(top, (left..left + t as i32).collect::<Vec<_>>());
        }
    }

    #[test]
    fn heavy_lines_are_twice_as_thick_and_centred() {
        for (w, h, t) in SIZES {
            let c = render(0x2501, w, h, t);
            let r = rows(&c, 0);
            assert_eq!(r.len() as u32, 2 * t);
            assert_eq!(r[0] as u32, (h - 2 * t) / 2);
            let c = render(0x2503, w, h, t);
            let k = cols(&c, 0);
            assert_eq!(k.len() as u32, 2 * t);
            assert_eq!(k[0] as u32, (w - 2 * t) / 2);
        }
    }

    #[test]
    fn cross_arms_are_centred_like_the_plain_lines() {
        for (w, h, t) in SIZES {
            let cross = render(0x253C, w, h, t);
            let hrows = light_h_rows(w, h, t);
            let vcols = light_v_cols(w, h, t);
            // Left and right arms: the same rows as ─ at both edges.
            assert_eq!(rows(&cross, 0), hrows, "{w}x{h}");
            assert_eq!(rows(&cross, w as i32 - 1), hrows, "{w}x{h}");
            // Top and bottom arms: the same columns as │ at both edges.
            let top_cols = cols(&cross, 0);
            assert_eq!(top_cols, vcols, "{w}x{h}");
            assert_eq!(cols(&cross, h as i32 - 1), vcols, "{w}x{h}");
            // And the whole row of the horizontal arm is lit.
            for &y in &hrows {
                assert_eq!(cols(&cross, y).len() as u32, w);
            }
            for &x in &vcols {
                assert_eq!(rows(&cross, x).len() as u32, h);
            }
        }
    }

    #[test]
    fn corners_meet_the_straight_pieces() {
        for (w, h, t) in SIZES {
            let hrows = light_h_rows(w, h, t);
            let vcols = light_v_cols(w, h, t);
            // ┌ has its right end where ─ does, and its bottom end where │ does.
            let c = render(0x250C, w, h, t);
            assert_eq!(rows(&c, w as i32 - 1), hrows);
            assert_eq!(cols(&c, h as i32 - 1), vcols);
            // ┘ mirrors it.
            let c = render(0x2518, w, h, t);
            assert_eq!(rows(&c, 0), hrows);
            assert_eq!(cols(&c, 0), vcols);
            // Half lines are the matching halves of the full one.
            let left = render(0x2574, w, h, t);
            let right = render(0x2576, w, h, t);
            assert_eq!(rows(&left, 0), hrows);
            assert_eq!(rows(&right, w as i32 - 1), hrows);
            for x in 0..w as i32 {
                let lit = on(&left, x, hrows[0]) || on(&right, x, hrows[0]);
                assert!(lit, "{w}x{h}: x={x} is covered by one of the halves");
            }
        }
    }

    #[test]
    fn double_lines_have_a_gap_of_one_light_stroke() {
        for (w, h, t) in SIZES {
            let c = render(0x2550, w, h, t);
            let r = rows(&c, 0);
            assert_eq!(r.len() as u32, 2 * t);
            let light_top = ((h - t) / 2) as i32;
            // Two light strokes with the middle light-width free.
            let expect: Vec<i32> = (light_top - t as i32..light_top)
                .chain(light_top + t as i32..light_top + 2 * t as i32)
                .collect();
            assert_eq!(r, expect, "{w}x{h}");
            assert_eq!(r, rows(&c, w as i32 - 1));

            let c = render(0x2551, w, h, t);
            let k = cols(&c, 0);
            let light_left = ((w - t) / 2) as i32;
            let expect: Vec<i32> = (light_left - t as i32..light_left)
                .chain(light_left + t as i32..light_left + 2 * t as i32)
                .collect();
            assert_eq!(k, expect, "{w}x{h}");
            assert_eq!(k, cols(&c, h as i32 - 1));
        }
    }

    #[test]
    fn double_cross_has_gaps_and_open_centre() {
        for (w, h, t) in SIZES {
            let c = render(0x256C, w, h, t);
            let light_top = ((h - t) / 2) as i32;
            let light_left = ((w - t) / 2) as i32;
            let t = t as i32;
            let tw = t;
            // The centre of the junction is empty.
            for y in light_top..light_top + t {
                for x in light_left..light_left + tw {
                    assert!(!on(&c, x, y), "{w}x{h}: centre ({x},{y}) is lit");
                }
            }
            // The gap between the doubled strokes runs from each edge to the junction on both axes.
            assert!(!on(&c, 0, light_top));
            assert!(on(&c, 0, light_top - 1));
            assert!(on(&c, 0, light_top + t));
            assert!(!on(&c, light_left, 0));
            assert!(on(&c, light_left - 1, 0));
            assert!(on(&c, light_left + tw, 0));
            // The quadrants close: the inner corner pieces stop at the other strokes.
            assert!(on(&c, light_left - 1, light_top - 1));
            assert!(on(&c, light_left + tw, light_top + t));
        }
    }

    #[test]
    fn double_corner_closes_its_inner_corner() {
        for (w, h, t) in SIZES {
            // ╔: right and down doubled.
            let c = render(0x2554, w, h, t);
            let light_top = ((h - t) / 2) as i32;
            let light_left = ((w - t) / 2) as i32;
            let t = t as i32;
            // The outer corner stroke is solid ...
            assert!(on(&c, light_left - t, light_top - t));
            // ... the inner one turns at the second stroke, with the centre light-width cell open.
            assert!(!on(&c, light_left, light_top));
            assert!(on(&c, light_left + t, light_top + t));
            // Nothing above or to the left of the outer corner.
            assert!(!on(&c, light_left - t - 1, light_top));
            assert!(!on(&c, light_left, light_top - t - 1));
        }
    }

    #[test]
    fn mixed_weights_use_the_heavy_extent() {
        for (w, h, t) in SIZES {
            // ┍: light down, heavy right.
            let c = render(0x250D, w, h, t);
            let heavy_top = ((h - 2 * t) / 2) as i32;
            let r = rows(&c, w as i32 - 1);
            assert_eq!(r, (heavy_top..heavy_top + 2 * t as i32).collect::<Vec<_>>());
            // The light vertical reaches the heavy stroke's top edge (not its own).
            let vcols = light_v_cols(w, h, t);
            let col = vcols[0];
            let rows_at_col = rows(&c, col);
            assert_eq!(*rows_at_col.last().unwrap(), h as i32 - 1);
            assert_eq!(rows_at_col[0], heavy_top);
        }
    }

    #[test]
    fn every_codepoint_draws_something_inside_the_padded_cell() {
        for (w, h, t) in SIZES {
            let m = metrics(w, h, t);
            for cp in 0x2500..=0x257F {
                let mut c = Canvas::new(w, h);
                assert!(draw(cp, &mut c, w, h, &m));
                assert!(c.buf.iter().any(|&v| v != 0), "U+{cp:04X} at {w}x{h} is empty");
            }
        }
    }

    #[test]
    fn straight_pieces_stay_inside_the_cell() {
        for (w, h, t) in SIZES {
            let m = metrics(w, h, t);
            for cp in (0x2500..=0x254B).chain(0x2550..=0x256C).chain(0x2574..=0x257F) {
                let mut c = Canvas::new(w, h);
                draw(cp, &mut c, w, h, &m);
                for y in -(c.pad_y as i32)..(h + c.pad_y) as i32 {
                    for x in -(c.pad_x as i32)..(w + c.pad_x) as i32 {
                        let inside = x >= 0 && y >= 0 && x < w as i32 && y < h as i32;
                        assert!(inside || c.get(x, y) == 0, "U+{cp:04X} spills at ({x},{y})");
                    }
                }
            }
        }
    }

    #[test]
    fn dashes_tile_into_an_even_line() {
        for (w, h, t) in SIZES {
            for (cp, count) in [(0x2504, 3), (0x2508, 4), (0x254C, 2)] {
                let c = render(cp, w, h, t);
                let y = ((h - t) / 2) as i32;
                let row = cols(&c, y);
                // Runs of lit pixels.
                let mut runs = 0;
                let mut prev = -2;
                for &x in &row {
                    if x != prev + 1 {
                        runs += 1;
                    }
                    prev = x;
                }
                assert_eq!(runs, count, "U+{cp:04X} at {w}x{h}");
                // The thickness is right and the dashes are vertically centred.
                assert_eq!(rows(&c, row[0]).len() as u32, t);
                // The first dash starts and the last ends symmetric: equal gap at both edges within a pixel of
                // the half-gap rule.
                let left = row[0];
                let right = w as i32 - 1 - row[row.len() - 1];
                assert!((left - right).abs() <= 1, "U+{cp:04X} at {w}x{h}: {left} vs {right}");
            }
            for (cp, count) in [(0x2506, 3), (0x250A, 4), (0x254E, 2)] {
                let c = render(cp, w, h, t);
                let x = ((w - t) / 2) as i32;
                let col = rows(&c, x);
                let mut runs = 0;
                let mut prev = -2;
                for &y in &col {
                    if y != prev + 1 {
                        runs += 1;
                    }
                    prev = y;
                }
                assert_eq!(runs, count, "U+{cp:04X} at {w}x{h}");
                // The dashes start at the top edge.
                assert_eq!(col[0], 0);
            }
        }
    }

    #[test]
    fn heavy_dashes_are_thicker() {
        let (w, h, t) = (12, 24, 3);
        let light = render(0x2504, w, h, t);
        let heavy = render(0x2505, w, h, t);
        let count = |c: &Canvas| c.buf.iter().filter(|&&v| v == ON).count();
        assert!(count(&heavy) > count(&light));
        let x = heavy_first_col(&heavy);
        assert_eq!(rows(&heavy, x).len() as u32, 2 * t);
    }

    fn heavy_first_col(c: &Canvas) -> i32 {
        (0..c.width as i32).find(|&x| !rows(c, x).is_empty()).unwrap()
    }

    #[test]
    fn narrow_cells_fall_back_to_a_solid_dash_line() {
        // 5 px wide: fewer than 3 dashes + 3 gaps.
        let c = render(0x2504, 5, 11, 1);
        assert_eq!(cols(&c, 5), vec![0, 1, 2, 3, 4]);
        let c = render(0x2506, 5, 5, 1);
        assert_eq!(rows(&c, 2), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn arc_ends_line_up_with_the_straight_lines() {
        for (w, h, t) in SIZES {
            let hrows = light_h_rows(w, h, t);
            let vcols = light_v_cols(w, h, t);
            let lit = |c: &Canvas, x: i32, y: i32| c.get(x, y) > 0x80;
            // ╭: arm to the right, stem going down.
            let c = render(0x256D, w, h, t);
            // The straight ends sit on the cell edges, centred like ─ and │.
            let mid_y = hrows[hrows.len() / 2];
            let mid_x = vcols[vcols.len() / 2];
            assert!(lit(&c, w as i32 - 1, mid_y), "{w}x{h}: ╭ right end");
            assert!(lit(&c, mid_x, h as i32 - 1), "{w}x{h}: ╭ bottom end");
            assert_eq!(c.get(w as i32 - 1, hrows[0] - t as i32 - 1), 0, "{w}x{h}: nothing above the arm");
            // Nothing on the far side of the corner.
            assert_eq!(c.get(0, 0), 0);
            assert_eq!(c.get(0, h as i32 - 1), 0);
            assert_eq!(c.get(w as i32 - 1, 0), 0);

            // ╯: arm to the left, stem going up.
            let c = render(0x256F, w, h, t);
            assert!(lit(&c, 0, mid_y), "{w}x{h}: ╯ left end");
            assert!(lit(&c, mid_x, 0), "{w}x{h}: ╯ top end");
            assert_eq!(c.get(w as i32 - 1, h as i32 - 1), 0);
        }
    }

    #[test]
    fn arcs_mirror_each_other() {
        for (w, h, t) in SIZES {
            let tl = render(0x256F, w, h, t);
            let tr = render(0x2570, w, h, t);
            let bl = render(0x256E, w, h, t);
            let br = render(0x256D, w, h, t);
            // The curve's centre is `floor((cell - thickness) / 2) + thickness / 2`, which is off the cell's own
            // centre by up to half a pixel, so the corners agree only to within that much coverage.
            let sum = |c: &Canvas| c.buf.iter().map(|&v| v as i64).sum::<i64>();
            let (a, b, c2, d) = (sum(&tl), sum(&tr), sum(&bl), sum(&br));
            let max = a.max(b).max(c2).max(d);
            let min = a.min(b).min(c2).min(d);
            assert!((max - min) * 8 <= max, "{w}x{h}: {a} {b} {c2} {d}");
        }
    }

    #[test]
    fn diagonals_cross_the_cell_and_reach_both_corners() {
        for (w, h, t) in SIZES {
            let (iw, ih) = (w as i32, h as i32);
            // ╱: lower-left to upper-right.
            let c = render(0x2571, w, h, t);
            assert!(c.get(iw - 1, 0) > 0, "{w}x{h}: ╱ upper right");
            assert!(c.get(0, ih - 1) > 0, "{w}x{h}: ╱ lower left");
            assert_eq!(c.get(0, 0), 0, "{w}x{h}: ╱ leaves the upper left corner clear");
            assert_eq!(c.get(iw - 1, ih - 1), 0, "{w}x{h}: ╱ leaves the lower right corner clear");
            // ╲.
            let c = render(0x2572, w, h, t);
            assert!(c.get(0, 0) > 0, "{w}x{h}: ╲ upper left");
            assert!(c.get(iw - 1, ih - 1) > 0, "{w}x{h}: ╲ lower right");
            assert_eq!(c.get(iw - 1, 0), 0);
            assert_eq!(c.get(0, ih - 1), 0);
            // ╳ is both, and symmetric about the vertical axis.
            let x = render(0x2573, w, h, t);
            let (a, b) = (render(0x2571, w, h, t), render(0x2572, w, h, t));
            for y in 0..ih {
                for xx in 0..iw {
                    let want = a.get(xx, y).max(b.get(xx, y));
                    assert!(x.get(xx, y) >= want, "{w}x{h}: ╳ covers ╱ and ╲ at ({xx},{y})");
                }
            }
            // ╲ is ╱ mirrored, up to the antialiasing's quantisation.
            let mut mirrored = render(0x2571, w, h, t);
            mirrored.flip_h();
            let lhs = render(0x2572, w, h, t);
            for (p, q) in lhs.buf.iter().zip(&mirrored.buf) {
                assert!(p.abs_diff(*q) <= 32, "{w}x{h}: ╲ is ╱ mirrored ({p} vs {q})");
            }
        }
    }

    #[test]
    fn public_diagonals_match_the_codepoints() {
        let m = metrics(11, 21, 2);
        let mut c = Canvas::new(11, 21);
        light_diagonal_ur_ll(&mut c, &m);
        assert_eq!(c.buf, render(0x2571, 11, 21, 2).buf);
        let mut c = Canvas::new(11, 21);
        light_diagonal_ul_lr(&mut c, &m);
        assert_eq!(c.buf, render(0x2572, 11, 21, 2).buf);
    }

    #[test]
    fn lookup_table_matches_the_unicode_names() {
        // A sample of the packed table against the code chart: up<<6 | right<<4 | down<<2 | left.
        let at = |cp: u32| Lines::unpack(TABLE[(cp - 0x2500) as usize]);
        let l = at(0x250D); // ┍ down light, right heavy
        assert_eq!((l.up, l.right, l.down, l.left), (NONE, HEAVY, LIGHT, NONE));
        let l = at(0x2554); // ╔ down double, right double
        assert_eq!((l.up, l.right, l.down, l.left), (NONE, DOUBLE, DOUBLE, NONE));
        let l = at(0x256C); // ╬
        assert_eq!((l.up, l.right, l.down, l.left), (DOUBLE, DOUBLE, DOUBLE, DOUBLE));
        let l = at(0x257F); // ╿ up heavy, down light
        assert_eq!((l.up, l.right, l.down, l.left), (HEAVY, NONE, LIGHT, NONE));
        let l = at(0x2523); // ┣ all heavy but left
        assert_eq!((l.up, l.right, l.down, l.left), (HEAVY, HEAVY, HEAVY, NONE));
        let l = at(0x2577); // ╷
        assert_eq!((l.up, l.right, l.down, l.left), (NONE, NONE, LIGHT, NONE));
    }

    #[test]
    fn neighbouring_cells_join_without_a_seam() {
        // Two ─ cells side by side: the stroke is one unbroken run of rows.
        for (w, h, t) in SIZES {
            let c = render(0x2500, w, h, t);
            let mut row = Vec::new();
            for _ in 0..2 {
                for x in 0..w as i32 {
                    row.push(on(&c, x, ((h - t) / 2) as i32));
                }
            }
            assert!(row.iter().all(|&v| v));
        }
    }
}
