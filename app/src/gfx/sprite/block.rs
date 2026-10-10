//! Block elements, braille patterns, the legacy computing mosaics and the
//! supplement's octants and friends, ported from Ghostty's
//! `src/font/sprite/draw/` (`block.zig`, `braille.zig`,
//! `symbols_for_legacy_computing.zig` and its `_supplement.zig`) (MIT).
//!
//! Everything that is a union of rectangles snaps its edges with
//! `frac_min` / `frac_max`, so neighbouring pieces and mirrored halves meet
//! without a gap. Only the diagonals, triangles and circles are antialiased.

use tiny_skia::{LineCap, PathBuilder};

use super::{frac_max, frac_min, Canvas, Thickness, DARK, LIGHT, MEDIUM, ON};
use crate::gfx::metrics::Metrics;

// Utility names for common fractions (block.zig:15).
const ONE_EIGHTH: f64 = 0.125;
const ONE_QUARTER: f64 = 0.25;
const ONE_THIRD: f64 = 1.0 / 3.0;
const THREE_EIGHTHS: f64 = 0.375;
const HALF: f64 = 0.5;
const FIVE_EIGHTHS: f64 = 0.625;
const TWO_THIRDS: f64 = 2.0 / 3.0;
const THREE_QUARTERS: f64 = 0.75;
const SEVEN_EIGHTHS: f64 = 0.875;

#[derive(Clone, Copy, PartialEq, Eq)]
enum H {
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum V {
    Top,
    Bottom,
    Middle,
}

/// Where a figure sits in the cell (`common.zig` `Alignment`).
#[derive(Clone, Copy)]
struct Align(H, V);

impl Align {
    const UPPER: Align = Align(H::Center, V::Top);
    const LOWER: Align = Align(H::Center, V::Bottom);
    const LEFT: Align = Align(H::Left, V::Middle);
    const RIGHT: Align = Align(H::Right, V::Middle);
    const UPPER_LEFT: Align = Align(H::Left, V::Top);
    const UPPER_CENTER: Align = Self::UPPER;
    const UPPER_RIGHT: Align = Align(H::Right, V::Top);
    const MIDDLE_LEFT: Align = Self::LEFT;
    const MIDDLE_CENTER: Align = Align(H::Center, V::Middle);
    const MIDDLE_RIGHT: Align = Self::RIGHT;
    const LOWER_LEFT: Align = Align(H::Left, V::Bottom);
    const LOWER_CENTER: Align = Self::LOWER;
    const LOWER_RIGHT: Align = Align(H::Right, V::Bottom);

    /// The point of the cell this alignment names, in float pixels.
    fn point(self, fw: f64, fh: f64) -> (f64, f64) {
        let x = match self.0 {
            H::Left => 0.0,
            H::Right => fw,
            H::Center => fw / 2.0,
        };
        let y = match self.1 {
            V::Top => 0.0,
            V::Bottom => fh,
            V::Middle => fh / 2.0,
        };
        (x, y)
    }
}

/// Fill a section of the cell given by fractions (`common.zig:355` `fill`).
fn fill(m: &Metrics, c: &mut Canvas, x0: f64, x1: f64, y0: f64, y1: f64) {
    c.r#box(
        frac_min(x0, m.cell_width),
        frac_min(y0, m.cell_height),
        frac_max(x1, m.cell_width),
        frac_max(y1, m.cell_height),
        ON,
    );
}

/// A block of `width` × `height` cells aligned in the cell (block.zig:121).
fn block_shade(m: &Metrics, c: &mut Canvas, align: Align, width: f64, height: f64, shade: u8) {
    let w = (m.cell_width as f64 * width).round() as u32;
    let h = (m.cell_height as f64 * height).round() as u32;
    let x = match align.0 {
        H::Left => 0,
        H::Right => m.cell_width - w,
        H::Center => (m.cell_width - w) / 2,
    };
    let y = match align.1 {
        V::Top => 0,
        V::Bottom => m.cell_height - h,
        V::Middle => (m.cell_height - h) / 2,
    };
    c.rect(x as i32, y as i32, w as i32, h as i32, shade);
}

fn block(m: &Metrics, c: &mut Canvas, align: Align, width: f64, height: f64) {
    block_shade(m, c, align, width, height, ON);
}

fn full_block_shade(m: &Metrics, c: &mut Canvas, shade: u8) {
    c.r#box(0, 0, m.cell_width as i32, m.cell_height as i32, shade);
}

/// Quadrants (block.zig:163), each snapped like a half so they tile.
fn quadrant(m: &Metrics, c: &mut Canvas, tl: bool, tr: bool, bl: bool, br: bool) {
    if tl {
        fill(m, c, 0.0, HALF, 0.0, HALF);
    }
    if tr {
        fill(m, c, HALF, 1.0, 0.0, HALF);
    }
    if bl {
        fill(m, c, 0.0, HALF, HALF, 1.0);
    }
    if br {
        fill(m, c, HALF, 1.0, HALF, 1.0);
    }
}

/// A filled polygon, antialiased.
fn polygon(c: &mut Canvas, pts: &[(f64, f64)], alpha: u8) {
    let mut pb = PathBuilder::new();
    for (i, &(x, y)) in pts.iter().enumerate() {
        if i == 0 {
            pb.move_to(x as f32, y as f32);
        } else {
            pb.line_to(x as f32, y as f32);
        }
    }
    pb.close();
    if let Some(path) = pb.finish() {
        c.fill_path(&path, alpha);
    }
}

fn line(c: &mut Canvas, p0: (f64, f64), p1: (f64, f64), width: f64) {
    c.line(p0.0 as f32, p0.1 as f32, p1.0 as f32, p1.1 as f32, width as f32, ON);
}

/// Draw `cp` if this module handles it; false otherwise.
pub fn draw(cp: u32, c: &mut Canvas, width: u32, height: u32, m: &Metrics) -> bool {
    match cp {
        0x2580..=0x259F => block_elements(cp, c, m),
        0x2800..=0x28FF => braille(cp, c, width, height),
        0x1FB00..=0x1FB3B => sextant(cp, c, m),
        0x1FB3C..=0x1FB67 => smooth_mosaic(cp, c, m),
        0x1FB68..=0x1FB6F => edge_triangles(cp, c, m),
        0x1FB70..=0x1FB75 => {
            let n = (cp + 1 - 0x1FB70) as f64;
            fill(m, c, n * ONE_EIGHTH, (n + 1.0) * ONE_EIGHTH, 0.0, 1.0);
        }
        0x1FB76..=0x1FB7B => horizontal_eighth(m, c, cp + 1 - 0x1FB76),
        0x1FB7C..=0x1FB97 => eighths_and_shades(cp, c, width, height, m),
        0x1FB98 | 0x1FB99 => diagonal_hatch(cp == 0x1FB99, c, m),
        0x1FB9A..=0x1FB9F => triangles_and_shades(cp, c, m),
        0x1FBA0..=0x1FBAE => corner_diagonals(cp, c, m),
        0x1FBAF => {
            let (up, down, left, right) = (Line::Heavy, Line::Heavy, Line::Light, Line::Light);
            lines_char(m, c, Lines { up, down, left, right });
        }
        0x1FBBD => {
            light_diagonal_cross(m, c);
            c.invert();
            c.crop_to_cell();
        }
        0x1FBBE => {
            corner_diagonal_lines(m, c, [false, false, false, true]);
            c.invert();
            c.crop_to_cell();
        }
        0x1FBBF => {
            corner_diagonal_lines(m, c, [true; 4]);
            c.invert();
            c.crop_to_cell();
        }
        0x1FBCE => block(m, c, Align::LEFT, TWO_THIRDS, 1.0),
        0x1FBCF => block(m, c, Align::LEFT, ONE_THIRD, 1.0),
        0x1FBD0..=0x1FBDF => cell_diagonals(cp, c, m),
        0x1FBE0..=0x1FBEF => circles_and_halves(cp, c, m),
        0x1CC1B..=0x1CC1E => box_ticks(cp, c, width, height, m),
        0x1CC21..=0x1CC2F => separated_quadrants(cp, c, width, height),
        0x1CC30..=0x1CC3F => twelfth_circles(cp, c, width, height, m),
        0x1CD00..=0x1CDE5 => octant(cp, c, m),
        0x1CE00 => {
            circle(m, c, Align::LEFT, false);
            circle(m, c, Align::RIGHT, false);
        }
        0x1CE01 => {
            circle(m, c, Align::UPPER, false);
            circle(m, c, Align::LOWER, false);
        }
        0x1CE0B => {
            circle_piece(c, width, height, m, (0.0, 0.0, 1.0, 0.5), Corner::Tl);
            circle_piece(c, width, height, m, (0.0, 0.0, 1.0, 0.5), Corner::Bl);
        }
        0x1CE0C => {
            circle_piece(c, width, height, m, (1.0, 0.0, 1.0, 0.5), Corner::Tr);
            circle_piece(c, width, height, m, (1.0, 0.0, 1.0, 0.5), Corner::Br);
        }
        0x1CE16..=0x1CE19 => vertical_ticks(cp, c, width, height, m),
        0x1CE51..=0x1CE8F => separated_sextants(cp, c, width, height),
        0x1CE90..=0x1CEAF => sixteenths(cp, c, m),
        _ => return false,
    }
    true
}

/// Block Elements U+2580–259F (block.zig:34).
fn block_elements(cp: u32, c: &mut Canvas, m: &Metrics) {
    match cp {
        0x2580 => block(m, c, Align::UPPER, 1.0, HALF),
        0x2581 => block(m, c, Align::LOWER, 1.0, ONE_EIGHTH),
        0x2582 => block(m, c, Align::LOWER, 1.0, ONE_QUARTER),
        0x2583 => block(m, c, Align::LOWER, 1.0, THREE_EIGHTHS),
        0x2584 => block(m, c, Align::LOWER, 1.0, HALF),
        0x2585 => block(m, c, Align::LOWER, 1.0, FIVE_EIGHTHS),
        0x2586 => block(m, c, Align::LOWER, 1.0, THREE_QUARTERS),
        0x2587 => block(m, c, Align::LOWER, 1.0, SEVEN_EIGHTHS),
        0x2588 => full_block_shade(m, c, ON),
        0x2589 => block(m, c, Align::LEFT, SEVEN_EIGHTHS, 1.0),
        0x258A => block(m, c, Align::LEFT, THREE_QUARTERS, 1.0),
        0x258B => block(m, c, Align::LEFT, FIVE_EIGHTHS, 1.0),
        0x258C => block(m, c, Align::LEFT, HALF, 1.0),
        0x258D => block(m, c, Align::LEFT, THREE_EIGHTHS, 1.0),
        0x258E => block(m, c, Align::LEFT, ONE_QUARTER, 1.0),
        0x258F => block(m, c, Align::LEFT, ONE_EIGHTH, 1.0),
        0x2590 => block(m, c, Align::RIGHT, HALF, 1.0),
        0x2591 => full_block_shade(m, c, LIGHT),
        0x2592 => full_block_shade(m, c, MEDIUM),
        0x2593 => full_block_shade(m, c, DARK),
        0x2594 => block(m, c, Align::UPPER, 1.0, ONE_EIGHTH),
        0x2595 => block(m, c, Align::RIGHT, ONE_EIGHTH, 1.0),
        0x2596 => quadrant(m, c, false, false, true, false),
        0x2597 => quadrant(m, c, false, false, false, true),
        0x2598 => quadrant(m, c, true, false, false, false),
        0x2599 => quadrant(m, c, true, false, true, true),
        0x259A => quadrant(m, c, true, false, false, true),
        0x259B => quadrant(m, c, true, true, true, false),
        0x259C => quadrant(m, c, true, true, false, true),
        0x259D => quadrant(m, c, false, true, false, false),
        0x259E => quadrant(m, c, false, true, true, false),
        0x259F => quadrant(m, c, false, true, true, true),
        _ => unreachable!(),
    }
}

/// Braille Patterns U+2800–28FF (braille.zig:62). The dot size, spacing and
/// margins are chosen from the pixel budget so the dots land on whole pixels.
fn braille(cp: u32, c: &mut Canvas, width: u32, height: u32) {
    let (width, height) = (width as i32, height as i32);
    let mut w = (width / 4).min(height / 8);
    let mut x_spacing = width / 4;
    let mut y_spacing = height / 8;
    let mut x_margin = x_spacing / 2;
    let mut y_margin = y_spacing / 2;

    let mut x_px_left = width - 2 * x_margin - x_spacing - 2 * w;
    let mut y_px_left = height - 2 * y_margin - 3 * y_spacing - 4 * w;

    // First, try hard to ensure the dot width is non-zero.
    if x_px_left >= 2 && y_px_left >= 4 && w == 0 {
        w += 1;
        x_px_left -= 2;
        y_px_left -= 4;
    }

    // Second, prefer a non-zero margin.
    if x_px_left >= 2 && x_margin == 0 {
        x_margin = 1;
        x_px_left -= 2;
    }
    if y_px_left >= 2 && y_margin == 0 {
        y_margin = 1;
        y_px_left -= 2;
    }

    // Third, increase spacing.
    if x_px_left >= 1 {
        x_spacing += 1;
        x_px_left -= 1;
    }
    if y_px_left >= 3 {
        y_spacing += 1;
        y_px_left -= 3;
    }

    // Fourth, margins ("spacing", but on the sides).
    if x_px_left >= 2 {
        x_margin += 1;
        x_px_left -= 2;
    }
    if y_px_left >= 2 {
        y_margin += 1;
        y_px_left -= 2;
    }

    // Last, increase the dot width.
    if x_px_left >= 2 && y_px_left >= 4 {
        w += 1;
    }

    let x = [x_margin, x_margin + w + x_spacing];
    let y0 = y_margin;
    let y = [y0, y0 + w + y_spacing, y0 + 2 * (w + y_spacing), y0 + 3 * (w + y_spacing)];

    // Dot bits in codepoint order: tl ul ll tr ur lr bl br.
    const DOTS: [(usize, usize); 8] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
    for (bit, &(col, row)) in DOTS.iter().enumerate() {
        if cp & (1 << bit) != 0 {
            c.r#box(x[col], y[row], x[col] + w, y[row] + w, ON);
        }
    }
}

/// Sextants U+1FB00–1FB3B (symbols_for_legacy_computing.zig:97). The codepoints
/// skip the patterns that exist elsewhere (empty, left half, right half, full).
fn sextant(cp: u32, c: &mut Canvas, m: &Metrics) {
    let idx = cp - 0x1FB00;
    let sex = idx + idx / 0x14 + 1;
    // Bits in order: tl tr ml mr bl br.
    const CELLS: [(f64, f64, f64, f64); 6] = [
        (0.0, HALF, 0.0, ONE_THIRD),
        (HALF, 1.0, 0.0, ONE_THIRD),
        (0.0, HALF, ONE_THIRD, TWO_THIRDS),
        (HALF, 1.0, ONE_THIRD, TWO_THIRDS),
        (0.0, HALF, TWO_THIRDS, 1.0),
        (HALF, 1.0, TWO_THIRDS, 1.0),
    ];
    for (bit, &(x0, x1, y0, y1)) in CELLS.iter().enumerate() {
        if sex & (1 << bit) != 0 {
            fill(m, c, x0, x1, y0, y1);
        }
    }
}

/// Smooth mosaics U+1FB3C–1FB67 (symbols_for_legacy_computing.zig:130). Each
/// table row is a 3x4 grid (`|` separates rows); `#` is a filled point and
/// anything else is empty. Up to ten boundary vertices are walked in order
/// and the polygon is antialiased.
fn smooth_mosaic(cp: u32, c: &mut Canvas, m: &Metrics) {
    let p = MOSAICS[(cp - 0x1FB3C) as usize];
    let on = |i: usize| p[i] == b'#';

    let tl = on(0);
    let ul = on(4) && (!on(0) || !on(8));
    let ll = on(8) && (!on(4) || !on(12));
    let bl = on(12);
    let bc = on(13) && (!on(12) || !on(14));
    let br = on(14);
    let lr = on(10) && (!on(14) || !on(6));
    let ur = on(6) && (!on(10) || !on(2));
    let tr = on(2);
    let tc = on(1) && (!on(2) || !on(0));

    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let (top, upper, lower, bottom) = (0.0, ONE_THIRD * fh, TWO_THIRDS * fh, fh);
    let (left, center, right) = (0.0, HALF * fw, fw);

    let vertices = [
        (tl, (left, top)),
        (ul, (left, upper)),
        (ll, (left, lower)),
        (bl, (left, bottom)),
        (bc, (center, bottom)),
        (br, (right, bottom)),
        (lr, (right, lower)),
        (ur, (right, upper)),
        (tr, (right, top)),
        (tc, (center, top)),
    ];
    let pts: Vec<(f64, f64)> = vertices.iter().filter(|(used, _)| *used).map(|&(_, pt)| pt).collect();
    polygon(c, &pts, ON);
}

/// A triangle from the cell centre to one edge (symbols_for_legacy_computing.zig:1239).
fn edge_triangle(m: &Metrics, c: &mut Canvas, edge: Edge) {
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let (upper, middle, lower) = (0.0, (fh / 2.0).round(), fh);
    let (left, center, right) = (0.0, (fw / 2.0).round(), fw);
    let ((x0, y0), (x1, y1)) = match edge {
        Edge::Top => ((right, upper), (left, upper)),
        Edge::Left => ((left, upper), (left, lower)),
        Edge::Bottom => ((left, lower), (right, lower)),
        Edge::Right => ((right, lower), (right, upper)),
    };
    polygon(c, &[(center, middle), (x0, y0), (x1, y1)], ON);
}

#[derive(Clone, Copy)]
enum Edge {
    Top,
    Left,
    Bottom,
    Right,
}

/// U+1FB68–1FB6F: the four edge triangles, and their inverted notches.
fn edge_triangles(cp: u32, c: &mut Canvas, m: &Metrics) {
    // Left, top, right, bottom in codepoint order.
    let edge = [Edge::Left, Edge::Top, Edge::Right, Edge::Bottom][((cp - 0x1FB68) % 4) as usize];
    edge_triangle(m, c, edge);
    if cp < 0x1FB6C {
        c.invert();
        c.crop_to_cell();
    }
}

/// One horizontal eighth, `n` in 0..8, full width.
fn horizontal_eighth(m: &Metrics, c: &mut Canvas, n: u32) {
    fill(m, c, 0.0, 1.0, n as f64 * ONE_EIGHTH, (n + 1) as f64 * ONE_EIGHTH);
}

/// U+1FB7C–1FB97 (symbols_for_legacy_computing.zig:595).
fn eighths_and_shades(cp: u32, c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    match cp {
        0x1FB7C => {
            block(m, c, Align::LEFT, ONE_EIGHTH, 1.0);
            block(m, c, Align::LOWER, 1.0, ONE_EIGHTH);
        }
        0x1FB7D => {
            block(m, c, Align::LEFT, ONE_EIGHTH, 1.0);
            block(m, c, Align::UPPER, 1.0, ONE_EIGHTH);
        }
        0x1FB7E => {
            block(m, c, Align::RIGHT, ONE_EIGHTH, 1.0);
            block(m, c, Align::UPPER, 1.0, ONE_EIGHTH);
        }
        0x1FB7F => {
            block(m, c, Align::RIGHT, ONE_EIGHTH, 1.0);
            block(m, c, Align::LOWER, 1.0, ONE_EIGHTH);
        }
        0x1FB80 => {
            block(m, c, Align::UPPER, 1.0, ONE_EIGHTH);
            block(m, c, Align::LOWER, 1.0, ONE_EIGHTH);
        }
        // Horizontal one eighth block 1358: rows 0, 2, 4 and 7.
        0x1FB81 => {
            for n in [0, 2, 4, 7] {
                horizontal_eighth(m, c, n);
            }
        }
        0x1FB82 => block(m, c, Align::UPPER, 1.0, ONE_QUARTER),
        0x1FB83 => block(m, c, Align::UPPER, 1.0, THREE_EIGHTHS),
        0x1FB84 => block(m, c, Align::UPPER, 1.0, FIVE_EIGHTHS),
        0x1FB85 => block(m, c, Align::UPPER, 1.0, THREE_QUARTERS),
        0x1FB86 => block(m, c, Align::UPPER, 1.0, SEVEN_EIGHTHS),
        0x1FB87 => block(m, c, Align::RIGHT, ONE_QUARTER, 1.0),
        0x1FB88 => block(m, c, Align::RIGHT, THREE_EIGHTHS, 1.0),
        0x1FB89 => block(m, c, Align::RIGHT, FIVE_EIGHTHS, 1.0),
        0x1FB8A => block(m, c, Align::RIGHT, THREE_QUARTERS, 1.0),
        0x1FB8B => block(m, c, Align::RIGHT, SEVEN_EIGHTHS, 1.0),
        0x1FB8C => block_shade(m, c, Align::LEFT, HALF, 1.0, MEDIUM),
        0x1FB8D => block_shade(m, c, Align::RIGHT, HALF, 1.0, MEDIUM),
        0x1FB8E => block_shade(m, c, Align::UPPER, 1.0, HALF, MEDIUM),
        0x1FB8F => block_shade(m, c, Align::LOWER, 1.0, HALF, MEDIUM),
        0x1FB90 => full_block_shade(m, c, MEDIUM),
        0x1FB91 => {
            full_block_shade(m, c, MEDIUM);
            block(m, c, Align::UPPER, 1.0, HALF);
        }
        0x1FB92 => {
            full_block_shade(m, c, MEDIUM);
            block(m, c, Align::LOWER, 1.0, HALF);
        }
        // Unallocated: a hole in the Unicode block, drawn empty.
        0x1FB93 => {}
        0x1FB94 => {
            full_block_shade(m, c, MEDIUM);
            block(m, c, Align::RIGHT, HALF, 1.0);
        }
        0x1FB95 => checkerboard_fill(m, c, 0),
        0x1FB96 => checkerboard_fill(m, c, 1),
        0x1FB97 => {
            let (w, h) = (width as i32, height as i32);
            c.r#box(0, h / 4, w, 2 * h / 4, ON);
            c.r#box(0, 3 * h / 4, w, h, ON);
        }
        _ => unreachable!(),
    }
}

/// Four columns by `round(4 h / w)` rows, with integer edges (symbols_for_legacy_computing.zig:1341).
fn checkerboard_fill(m: &Metrics, c: &mut Canvas, parity: usize) {
    let (cw, ch) = (m.cell_width as usize, m.cell_height as usize);
    let x_size = 4usize;
    let y_size = (4.0 * (ch as f64 / cw as f64)).round() as usize;
    for x in 0..x_size {
        let x0 = cw * x / x_size;
        let x1 = cw * (x + 1) / x_size;
        for y in 0..y_size {
            let y0 = ch * y / y_size;
            let y1 = ch * (y + 1) / y_size;
            if (x + y) % 2 == parity {
                c.rect(x0 as i32, y0 as i32, x1.saturating_sub(x0) as i32, y1.saturating_sub(y0) as i32, ON);
            }
        }
    }
}

/// Diagonal hatch fills U+1FB98 (upper left to lower right) and U+1FB99
/// (mirrored); symbols_for_legacy_computing.zig:722. Ghostty notes these do
/// not align properly for most cell sizes; they are ported as they are.
fn diagonal_hatch(mirrored: bool, c: &mut Canvas, m: &Metrics) {
    let thick_px = Thickness::Light.px(m.box_thickness);
    let line_count = m.cell_width / (2 * thick_px);
    if line_count == 0 {
        return;
    }
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let stride = (fw / line_count as f64).round();
    for i in 0..line_count * 2 + 1 {
        let n = (i as i32 - line_count as i32) as f64 * stride;
        let (top_x, bottom_x) = if mirrored { (fw + n, n) } else { (n, fw + n) };
        line(c, (top_x, 0.0), (bottom_x, fh), thick_px as f64);
    }
    c.crop_to_cell();
}

#[derive(Clone, Copy)]
enum Corner {
    Tl,
    Tr,
    Bl,
    Br,
}

/// A filled corner triangle (geometric_shapes.zig:85).
fn corner_triangle_shade(m: &Metrics, c: &mut Canvas, corner: Corner, shade: u8) {
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let pts = match corner {
        Corner::Tl => [(0.0, 0.0), (0.0, fh), (fw, 0.0)],
        Corner::Tr => [(0.0, 0.0), (fw, fh), (fw, 0.0)],
        Corner::Bl => [(0.0, 0.0), (0.0, fh), (fw, fh)],
        Corner::Br => [(0.0, fh), (fw, fh), (fw, 0.0)],
    };
    polygon(c, &pts, shade);
}

/// U+1FB9A–1FB9F.
fn triangles_and_shades(cp: u32, c: &mut Canvas, m: &Metrics) {
    match cp {
        0x1FB9A => {
            edge_triangle(m, c, Edge::Top);
            edge_triangle(m, c, Edge::Bottom);
        }
        0x1FB9B => {
            edge_triangle(m, c, Edge::Left);
            edge_triangle(m, c, Edge::Right);
        }
        0x1FB9C => corner_triangle_shade(m, c, Corner::Tl, MEDIUM),
        0x1FB9D => corner_triangle_shade(m, c, Corner::Tr, MEDIUM),
        0x1FB9E => corner_triangle_shade(m, c, Corner::Br, MEDIUM),
        0x1FB9F => corner_triangle_shade(m, c, Corner::Bl, MEDIUM),
        _ => unreachable!(),
    }
}

/// Light lines from the top / bottom centre to the mid-left / mid-right
/// (symbols_for_legacy_computing.zig:1267). `corners` is tl, tr, bl, br.
fn corner_diagonal_lines(m: &Metrics, c: &mut Canvas, corners: [bool; 4]) {
    let thick = Thickness::Light.px(m.box_thickness) as f64;
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let center_x = (m.cell_width / 2 + m.cell_width % 2) as f64;
    let center_y = (m.cell_height / 2 + m.cell_height % 2) as f64;
    if corners[0] {
        line(c, (center_x, 0.0), (0.0, center_y), thick);
    }
    if corners[1] {
        line(c, (center_x, 0.0), (fw, center_y), thick);
    }
    if corners[2] {
        line(c, (center_x, fh), (0.0, center_y), thick);
    }
    if corners[3] {
        line(c, (center_x, fh), (fw, center_y), thick);
    }
}

/// U+1FBA0–1FBAE.
fn corner_diagonals(cp: u32, c: &mut Canvas, m: &Metrics) {
    // tl = 1, tr = 2, bl = 4, br = 8, in codepoint order.
    const MASKS: [u8; 15] = [
        1,
        2,
        4,
        8,
        1 | 4,
        2 | 8,
        4 | 8,
        1 | 2,
        1 | 8,
        2 | 4,
        2 | 4 | 8,
        1 | 4 | 8,
        1 | 2 | 8,
        1 | 2 | 4,
        15,
    ];
    let mask = MASKS[(cp - 0x1FBA0) as usize];
    corner_diagonal_lines(m, c, [mask & 1 != 0, mask & 2 != 0, mask & 4 != 0, mask & 8 != 0]);
}

/// A light line between two named points of the cell (symbols_for_legacy_computing.zig:1301).
fn cell_diagonal(m: &Metrics, c: &mut Canvas, from: Align, to: Align) {
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let thick = Thickness::Light.px(m.box_thickness) as f64;
    line(c, from.point(fw, fh), to.point(fw, fh), thick);
}

/// Cell diagonals U+1FBD0–1FBDF (symbols_for_legacy_computing.zig:999).
fn cell_diagonals(cp: u32, c: &mut Canvas, m: &Metrics) {
    use Align as A;
    const TABLE: [&[(A, A)]; 16] = [
        &[(A::MIDDLE_RIGHT, A::LOWER_LEFT)],
        &[(A::UPPER_RIGHT, A::MIDDLE_LEFT)],
        &[(A::UPPER_LEFT, A::MIDDLE_RIGHT)],
        &[(A::MIDDLE_LEFT, A::LOWER_RIGHT)],
        &[(A::UPPER_LEFT, A::LOWER_CENTER)],
        &[(A::UPPER_CENTER, A::LOWER_RIGHT)],
        &[(A::UPPER_RIGHT, A::LOWER_CENTER)],
        &[(A::UPPER_CENTER, A::LOWER_LEFT)],
        &[(A::UPPER_LEFT, A::MIDDLE_CENTER), (A::MIDDLE_CENTER, A::UPPER_RIGHT)],
        &[(A::UPPER_RIGHT, A::MIDDLE_CENTER), (A::MIDDLE_CENTER, A::LOWER_RIGHT)],
        &[(A::LOWER_LEFT, A::MIDDLE_CENTER), (A::MIDDLE_CENTER, A::LOWER_RIGHT)],
        &[(A::UPPER_LEFT, A::MIDDLE_CENTER), (A::MIDDLE_CENTER, A::LOWER_LEFT)],
        &[(A::UPPER_LEFT, A::LOWER_CENTER), (A::LOWER_CENTER, A::UPPER_RIGHT)],
        &[(A::UPPER_RIGHT, A::MIDDLE_LEFT), (A::MIDDLE_LEFT, A::LOWER_RIGHT)],
        &[(A::LOWER_LEFT, A::UPPER_CENTER), (A::UPPER_CENTER, A::LOWER_RIGHT)],
        &[(A::UPPER_LEFT, A::MIDDLE_RIGHT), (A::MIDDLE_RIGHT, A::LOWER_LEFT)],
    ];
    for &(from, to) in TABLE[(cp - 0x1FBD0) as usize] {
        cell_diagonal(m, c, from, to);
    }
}

/// A circle centred on an edge or corner point of the cell, radius half the
/// shorter side, cropped to the cell (symbols_for_legacy_computing.zig:1368).
fn circle(m: &Metrics, c: &mut Canvas, position: Align, filled: bool) {
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let (x, y) = position.point(fw, fh);
    let r = 0.5 * fw.min(fh);
    let thick = Thickness::Light.px(m.box_thickness) as f64;
    if filled {
        if let Some(path) = PathBuilder::from_circle(x as f32, y as f32, r as f32) {
            c.fill_path(&path, ON);
        }
    } else if let Some(path) = PathBuilder::from_circle(x as f32, y as f32, (r - thick / 2.0) as f32) {
        c.stroke_path(&path, thick as f32, LineCap::Butt, ON);
    }
    c.crop_to_cell();
}

/// U+1FBE0–1FBEF.
fn circles_and_halves(cp: u32, c: &mut Canvas, m: &Metrics) {
    match cp {
        0x1FBE0 => circle(m, c, Align::UPPER, false),
        0x1FBE1 => circle(m, c, Align::RIGHT, false),
        0x1FBE2 => circle(m, c, Align::LOWER, false),
        0x1FBE3 => circle(m, c, Align::LEFT, false),
        0x1FBE4 => block(m, c, Align::UPPER_CENTER, 0.5, 0.5),
        0x1FBE5 => block(m, c, Align::LOWER_CENTER, 0.5, 0.5),
        0x1FBE6 => block(m, c, Align::MIDDLE_LEFT, 0.5, 0.5),
        0x1FBE7 => block(m, c, Align::MIDDLE_RIGHT, 0.5, 0.5),
        0x1FBE8 => circle(m, c, Align::UPPER, true),
        0x1FBE9 => circle(m, c, Align::RIGHT, true),
        0x1FBEA => circle(m, c, Align::LOWER, true),
        0x1FBEB => circle(m, c, Align::LEFT, true),
        0x1FBEC => circle(m, c, Align::UPPER_RIGHT, true),
        0x1FBED => circle(m, c, Align::LOWER_LEFT, true),
        0x1FBEE => circle(m, c, Align::LOWER_RIGHT, true),
        0x1FBEF => circle(m, c, Align::UPPER_LEFT, true),
        _ => unreachable!(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Line {
    None,
    Light,
    Heavy,
}

#[derive(Clone, Copy)]
struct Lines {
    up: Line,
    down: Line,
    left: Line,
    right: Line,
}

/// Straight box-drawing strokes (box.zig:399), without the double lines this
/// module never asks for. The joins are chosen so strokes meet cleanly.
fn lines_char(m: &Metrics, c: &mut Canvas, l: Lines) {
    let light_px = Thickness::Light.px(m.box_thickness);
    let heavy_px = Thickness::Heavy.px(m.box_thickness);
    let (cw, ch) = (m.cell_width, m.cell_height);

    let h_light_top = ch.saturating_sub(light_px) / 2;
    let h_light_bottom = h_light_top.saturating_add(light_px);
    let h_heavy_top = ch.saturating_sub(heavy_px) / 2;
    let h_heavy_bottom = h_heavy_top.saturating_add(heavy_px);
    let v_light_left = cw.saturating_sub(light_px) / 2;
    let v_light_right = v_light_left.saturating_add(light_px);
    let v_heavy_left = cw.saturating_sub(heavy_px) / 2;
    let v_heavy_right = v_heavy_left.saturating_add(heavy_px);

    let (none, heavy) = (Line::None, Line::Heavy);
    let up_bottom = if l.left == heavy || l.right == heavy {
        h_heavy_bottom
    } else if l.left != l.right || l.down == l.up {
        h_light_bottom
    } else if l.left == none && l.right == none {
        h_light_bottom
    } else {
        h_light_top
    };
    let down_top = if l.left == heavy || l.right == heavy {
        h_heavy_top
    } else if l.left != l.right || l.up == l.down {
        h_light_top
    } else if l.left == none && l.right == none {
        h_light_top
    } else {
        h_light_bottom
    };
    let left_right = if l.up == heavy || l.down == heavy {
        v_heavy_right
    } else if l.up != l.down || l.left == l.right {
        v_light_right
    } else if l.up == none && l.down == none {
        v_light_right
    } else {
        v_light_left
    };
    let right_left = if l.up == heavy || l.down == heavy {
        v_heavy_left
    } else if l.up != l.down || l.right == l.left {
        v_light_left
    } else if l.up == none && l.down == none {
        v_light_left
    } else {
        v_light_right
    };

    let b = |c: &mut Canvas, x0: u32, y0: u32, x1: u32, y1: u32| {
        c.r#box(x0 as i32, y0 as i32, x1 as i32, y1 as i32, ON);
    };
    match l.up {
        Line::None => {}
        Line::Light => b(c, v_light_left, 0, v_light_right, up_bottom),
        Line::Heavy => b(c, v_heavy_left, 0, v_heavy_right, up_bottom),
    }
    match l.right {
        Line::None => {}
        Line::Light => b(c, right_left, h_light_top, cw, h_light_bottom),
        Line::Heavy => b(c, right_left, h_heavy_top, cw, h_heavy_bottom),
    }
    match l.down {
        Line::None => {}
        Line::Light => b(c, v_light_left, down_top, v_light_right, ch),
        Line::Heavy => b(c, v_heavy_left, down_top, v_heavy_right, ch),
    }
    match l.left {
        Line::None => {}
        Line::Light => b(c, 0, h_light_top, left_right, h_light_bottom),
        Line::Heavy => b(c, 0, h_heavy_top, left_right, h_heavy_bottom),
    }
}

/// Both light diagonals, overshooting the corners by half a pixel along their
/// slope so neighbouring cells meet (box.zig:638).
fn light_diagonal_cross(m: &Metrics, c: &mut Canvas) {
    let (fw, fh) = (m.cell_width as f64, m.cell_height as f64);
    let slope_x = (fw / fh).min(1.0);
    let slope_y = (fh / fw).min(1.0);
    let thick = Thickness::Light.px(m.box_thickness) as f64;
    line(c, (fw + 0.5 * slope_x, -0.5 * slope_y), (-0.5 * slope_x, fh + 0.5 * slope_y), thick);
    line(c, (-0.5 * slope_x, -0.5 * slope_y), (fw + 0.5 * slope_x, fh + 0.5 * slope_y), thick);
}

/// Octants U+1CD00–1CDE5 (symbols_for_legacy_computing_supplement.zig:70):
/// two columns by four rows, numbered 1 2 / 3 4 / 5 6 / 7 8.
fn octant(cp: u32, c: &mut Canvas, m: &Metrics) {
    let oct = OCTANTS[(cp - 0x1CD00) as usize];
    for bit in 0..8 {
        if oct & (1 << bit) != 0 {
            let (x0, x1) = if bit % 2 == 0 { (0.0, HALF) } else { (HALF, 1.0) };
            let row = (bit / 2) as f64;
            fill(m, c, x0, x1, row * ONE_QUARTER, (row + 1.0) * ONE_QUARTER);
        }
    }
}

/// Separated block quadrants U+1CC21–1CC2F (…_supplement.zig:143).
fn separated_quadrants(cp: u32, c: &mut Canvas, width: u32, height: u32) {
    // Bits: tl tr bl br.
    let quad = (cp - 0x1CC20) & 0xF;
    let (width, height) = (width as i32, height as i32);
    let gap = (width / 12).max(1);
    let mid_gap_x = gap * 2 + width % 2;
    let mid_gap_y = gap * 2 + height % 2;
    let w = (width - gap * 2 - mid_gap_x) / 2;
    let h = (height - gap * 2 - mid_gap_y) / 2;

    let (x1, y1) = (gap + w + mid_gap_x, gap + h + mid_gap_y);
    if quad & 1 != 0 {
        c.r#box(gap, gap, gap + w, gap + h, ON);
    }
    if quad & 2 != 0 {
        c.r#box(x1, gap, x1 + w, gap + h, ON);
    }
    if quad & 4 != 0 {
        c.r#box(gap, y1, gap + w, y1 + h, ON);
    }
    if quad & 8 != 0 {
        c.r#box(x1, y1, x1 + w, y1 + h, ON);
    }
}

/// Separated block sextants U+1CE51–1CE8F (…_supplement.zig:386).
fn separated_sextants(cp: u32, c: &mut Canvas, width: u32, height: u32) {
    // Bits: tl tr ml mr bl br.
    let sex = (cp - 0x1CE50) & 0x3F;
    let (width, height) = (width as i32, height as i32);
    let gap = (width / 12).max(1);
    let mid_gap_x = gap * 2 + width % 2;
    let y_extra = height % 3;
    let mid_gap_y = gap * 2 + y_extra / 2;
    let w = (width - gap * 2 - mid_gap_x) / 2;
    let h = (height - gap * 2 - mid_gap_y * 2).div_euclid(3);
    // Any leftover height goes to the middle row.
    let h_m = height - gap * 2 - mid_gap_y * 2 - h * 2;

    let x1 = gap + w + mid_gap_x;
    let y_mid = gap + h + mid_gap_y;
    let y_bot = y_mid + h_m + mid_gap_y;
    let cells = [
        (gap, gap, h),
        (x1, gap, h),
        (gap, y_mid, h_m),
        (x1, y_mid, h_m),
        (gap, y_bot, h),
        (x1, y_bot, h),
    ];
    for (bit, &(x, y, hh)) in cells.iter().enumerate() {
        if sex & (1 << bit) != 0 {
            c.r#box(x, y, x + w, y + hh, ON);
        }
    }
}

/// Sixteenth blocks and quarter strips U+1CE90–1CEAF (…_supplement.zig:466).
fn sixteenths(cp: u32, c: &mut Canvas, m: &Metrics) {
    // (x0, x1, y0, y1) in quarters.
    const Q: [(u8, u8, u8, u8); 32] = [
        (0, 1, 0, 1),
        (1, 2, 0, 1),
        (2, 3, 0, 1),
        (3, 4, 0, 1),
        (0, 1, 1, 2),
        (1, 2, 1, 2),
        (2, 3, 1, 2),
        (3, 4, 1, 2),
        (0, 1, 2, 3),
        (1, 2, 2, 3),
        (2, 3, 2, 3),
        (3, 4, 2, 3),
        (0, 1, 3, 4),
        (1, 2, 3, 4),
        (2, 3, 3, 4),
        (3, 4, 3, 4),
        (2, 4, 3, 4),
        (1, 4, 3, 4),
        (0, 3, 3, 4),
        (0, 2, 3, 4),
        (0, 1, 2, 4),
        (0, 1, 1, 4),
        (0, 1, 0, 3),
        (0, 1, 0, 2),
        (0, 2, 0, 1),
        (0, 3, 0, 1),
        (1, 4, 0, 1),
        (2, 4, 0, 1),
        (3, 4, 0, 2),
        (3, 4, 0, 3),
        (3, 4, 1, 4),
        (3, 4, 2, 4),
    ];
    let (x0, x1, y0, y1) = Q[(cp - 0x1CE90) as usize];
    let q = |n: u8| n as f64 * ONE_QUARTER;
    fill(m, c, q(x0), q(x1), q(y0), q(y1));
}

/// A quarter-ellipse arc stroked at the box thickness (…_supplement.zig:547).
/// `(x, y, w, h)` place and size the ellipse in cells, as Ghostty passes them.
fn circle_piece(
    c: &mut Canvas,
    width: u32,
    height: u32,
    m: &Metrics,
    (x, y, w, h): (f64, f64, f64, f64),
    corner: Corner,
) {
    // Radii and position of the arc in pixels.
    let wdth = width as f64 * w;
    let hght = height as f64 * h;
    let xp = width as f64 * x;
    let yp = height as f64 * y;

    // Coefficient for approximating a circular arc.
    let k = (std::f64::consts::SQRT_2 - 1.0) * 4.0 / 3.0;
    let cw = k * wdth;
    let ch = k * hght;

    let thick = m.box_thickness as f64;
    let ht = thick * 0.5;

    let mut pb = PathBuilder::new();
    let mut curve = |start: (f64, f64), c1: (f64, f64), c2: (f64, f64), end: (f64, f64)| {
        pb.move_to(start.0 as f32, start.1 as f32);
        pb.cubic_to(c1.0 as f32, c1.1 as f32, c2.0 as f32, c2.1 as f32, end.0 as f32, end.1 as f32);
    };
    match corner {
        Corner::Tl => curve(
            (wdth - xp, ht - yp),
            (wdth - cw - xp, ht - yp),
            (ht - xp, hght - ch - yp),
            (ht - xp, hght - yp),
        ),
        Corner::Tr => curve(
            (wdth - xp, ht - yp),
            (wdth + cw - xp, ht - yp),
            (wdth * 2.0 - ht - xp, hght - ch - yp),
            (wdth * 2.0 - ht - xp, hght - yp),
        ),
        Corner::Bl => curve(
            (ht - xp, hght - yp),
            (ht - xp, hght + ch - yp),
            (wdth - cw - xp, hght * 2.0 - ht - yp),
            (wdth - xp, hght * 2.0 - ht - yp),
        ),
        Corner::Br => curve(
            (wdth * 2.0 - ht - xp, hght - yp),
            (wdth * 2.0 - ht - xp, hght + ch - yp),
            (wdth + cw - xp, hght * 2.0 - ht - yp),
            (wdth - xp, hght * 2.0 - ht - yp),
        ),
    }
    if let Some(path) = pb.finish() {
        c.stroke_path(&path, thick as f32, LineCap::Butt, ON);
    }
    c.crop_to_cell();
}

/// Twelfth and quarter circle pieces U+1CC30–1CC3F (…_supplement.zig:210).
/// They are ellipses sized to touch the edge of their enclosing set of cells.
fn twelfth_circles(cp: u32, c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    use Corner::*;
    // (x, y, w, h, corner) in codepoint order.
    const PIECES: [(f64, f64, f64, f64, Corner); 16] = [
        (0.0, 0.0, 2.0, 2.0, Tl),
        (1.0, 0.0, 2.0, 2.0, Tl),
        (2.0, 0.0, 2.0, 2.0, Tr),
        (3.0, 0.0, 2.0, 2.0, Tr),
        (0.0, 1.0, 2.0, 2.0, Tl),
        (0.0, 0.0, 1.0, 1.0, Tl),
        (1.0, 0.0, 1.0, 1.0, Tr),
        (3.0, 1.0, 2.0, 2.0, Tr),
        (0.0, 2.0, 2.0, 2.0, Bl),
        (0.0, 1.0, 1.0, 1.0, Bl),
        (1.0, 1.0, 1.0, 1.0, Br),
        (3.0, 2.0, 2.0, 2.0, Br),
        (0.0, 3.0, 2.0, 2.0, Bl),
        (1.0, 3.0, 2.0, 2.0, Bl),
        (2.0, 3.0, 2.0, 2.0, Br),
        (3.0, 3.0, 2.0, 2.0, Br),
    ];
    let (x, y, w, h, corner) = PIECES[(cp - 0x1CC30) as usize];
    circle_piece(c, width, height, m, (x, y, w, h), corner);
}

/// Box drawing with corner ticks U+1CC1B–1CC1E (…_supplement.zig:259).
fn box_ticks(cp: u32, c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    let (w, h, t) = (width as i32, height as i32, m.box_thickness as i32);
    let horizontal = Lines { up: Line::None, down: Line::None, left: Line::Light, right: Line::Light };
    match cp {
        0x1CC1B => {
            lines_char(m, c, horizontal);
            c.r#box(w - t, 0, w, h / 2, ON);
        }
        0x1CC1C => {
            lines_char(m, c, horizontal);
            c.r#box(w - t, h / 2, w, h, ON);
        }
        0x1CC1D => {
            c.r#box(0, 0, w, t, ON);
            c.r#box(0, 0, t, h / 2, ON);
        }
        0x1CC1E => {
            c.r#box(0, h - t, w, h, ON);
            c.r#box(0, h / 2, t, h, ON);
        }
        _ => unreachable!(),
    }
}

/// Vertical line with corner ticks U+1CE16–1CE19 (…_supplement.zig:350).
fn vertical_ticks(cp: u32, c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    let (w, h, t) = (width as i32, height as i32, m.box_thickness as i32);
    lines_char(m, c, Lines { up: Line::Light, down: Line::Light, left: Line::None, right: Line::None });
    match cp {
        0x1CE16 => c.r#box(w / 2, 0, w, t, ON),
        0x1CE17 => c.r#box(w / 2, h - t, w, h, ON),
        0x1CE18 => c.r#box(0, 0, w / 2, t, ON),
        0x1CE19 => c.r#box(0, h - t, w / 2, h, ON),
        _ => unreachable!(),
    }
}

const OCTANTS: [u8; 230] = [
    0x04, 0x06, 0x07, 0x08, 0x09, 0x0B, 0x0C, 0x0D, 0x0E, 0x10, 0x11, 0x12,
    0x13, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x29, 0x2A, 0x2B, 0x2C,
    0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38,
    0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46,
    0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E, 0x4F, 0x51, 0x52, 0x53,
    0x54, 0x56, 0x57, 0x58, 0x59, 0x5B, 0x5C, 0x5D, 0x5E, 0x60, 0x61, 0x62,
    0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E,
    0x6F, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A,
    0x7B, 0x7C, 0x7D, 0x7E, 0x7F, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8A, 0x8B, 0x8C, 0x8D, 0x8E, 0x8F, 0x90, 0x91, 0x92, 0x93,
    0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9E, 0x9F,
    0xA1, 0xA2, 0xA3, 0xA4, 0xA6, 0xA7, 0xA8, 0xA9, 0xAB, 0xAC, 0xAD, 0xAE,
    0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xBB,
    0xBC, 0xBD, 0xBE, 0xBF, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8,
    0xC9, 0xCA, 0xCB, 0xCC, 0xCD, 0xCE, 0xCF, 0xD0, 0xD1, 0xD2, 0xD3, 0xD4,
    0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xDB, 0xDC, 0xDD, 0xDE, 0xDF, 0xE0,
    0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xEB, 0xEC,
    0xED, 0xEE, 0xEF, 0xF1, 0xF2, 0xF3, 0xF4, 0xF6, 0xF7, 0xF8, 0xF9, 0xFB,
    0xFD, 0xFE,
];

const MOSAICS: [&[u8; 15]; 44] = [
    b"...|...|#..|##.", // 1fb3c
    b"...|...|#\\.|###", // 1fb3d
    b"...|#..|#\\.|##.", // 1fb3e
    b"...|#..|##.|###", // 1fb3f
    b"#..|#..|##.|##.", // 1fb40
    b"/##|###|###|###", // 1fb41
    b"./#|###|###|###", // 1fb42
    b".##|.##|###|###", // 1fb43
    b"..#|.##|###|###", // 1fb44
    b".##|.##|.##|###", // 1fb45
    b"...|./#|###|###", // 1fb46
    b"...|...|..#|.##", // 1fb47
    b"...|...|./#|###", // 1fb48
    b"...|..#|./#|.##", // 1fb49
    b"...|..#|.##|###", // 1fb4a
    b"..#|..#|.##|.##", // 1fb4b
    b"##\\|###|###|###", // 1fb4c
    b"#\\.|###|###|###", // 1fb4d
    b"##.|##.|###|###", // 1fb4e
    b"#..|##.|###|###", // 1fb4f
    b"##.|##.|##.|###", // 1fb50
    b"...|#\\.|###|###", // 1fb51
    b"###|###|###|\\##", // 1fb52
    b"###|###|###|.\\#", // 1fb53
    b"###|###|.##|.##", // 1fb54
    b"###|###|.##|..#", // 1fb55
    b"###|.##|.##|.##", // 1fb56
    b"##.|#..|...|...", // 1fb57
    b"###|#/.|...|...", // 1fb58
    b"##.|#/.|#..|...", // 1fb59
    b"###|##.|#..|...", // 1fb5a
    b"##.|##.|#..|#..", // 1fb5b
    b"###|###|#/.|...", // 1fb5c
    b"###|###|###|##/", // 1fb5d
    b"###|###|###|#/.", // 1fb5e
    b"###|###|##.|##.", // 1fb5f
    b"###|###|##.|#..", // 1fb60
    b"###|##.|##.|##.", // 1fb61
    b".##|..#|...|...", // 1fb62
    b"###|.\\#|...|...", // 1fb63
    b".##|.\\#|..#|...", // 1fb64
    b"###|.##|..#|...", // 1fb65
    b".##|.##|..#|..#", // 1fb66
    b"###|###|.\\#|...", // 1fb67
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::sprite::covers;

    /// Cell sizes of the common cases: odd and even, narrow and tall.
    const SIZES: [(u32, u32); 5] = [(9, 17), (10, 21), (8, 16), (18, 36), (7, 15)];

    fn metrics(w: u32, h: u32) -> Metrics {
        Metrics {
            cell_width: w,
            cell_height: h,
            cell_baseline: 0,
            underline_position: 0,
            underline_thickness: 1,
            strikethrough_position: 0,
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
        assert!(draw(cp, &mut c, w, h, &m), "{cp:#X} not drawn");
        c
    }

    /// The cell area as `w * h` row-major values.
    fn cell(c: &Canvas) -> Vec<u8> {
        let mut v = Vec::new();
        for y in 0..c.height as i32 {
            for x in 0..c.width as i32 {
                v.push(c.get(x, y));
            }
        }
        v
    }

    fn lit(c: &Canvas) -> usize {
        cell(c).iter().filter(|&&v| v != 0).count()
    }

    /// Everything in the padding is empty.
    fn stays_in_cell(c: &Canvas) -> bool {
        let (w, h) = (c.width as i32, c.height as i32);
        let (px, py) = (c.pad_x as i32, c.pad_y as i32);
        (-py..h + py).all(|y| (-px..w + px).all(|x| (0..w).contains(&x) && (0..h).contains(&y) || c.get(x, y) == 0))
    }

    #[test]
    fn halves_cover_the_cell_at_odd_heights() {
        // Both halves round up at an odd height, so the middle row is shared
        // (as in Ghostty); nothing is left uncovered and nothing is doubled
        // beyond that one row.
        for (w, h) in SIZES {
            let (up, down) = (render(0x2580, w, h), render(0x2584, w, h));
            let mut doubled = 0;
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let n = (up.get(x, y) == ON) as u32 + (down.get(x, y) == ON) as u32;
                    assert!(n >= 1, "{w}x{h}: hole at {x},{y}");
                    doubled += (n == 2) as u32;
                }
            }
            let expect = if h % 2 == 1 { w } else { 0 };
            assert_eq!(doubled, expect, "{w}x{h}");
        }
    }

    #[test]
    fn block_sizes_round() {
        let c = render(0x2581, 10, 21);
        // 21 / 8 = 2.625 rounds to 3 rows at the bottom.
        assert_eq!(lit(&c), 3 * 10);
        assert_eq!(c.get(0, 17), 0);
        assert_eq!(c.get(0, 18), ON);
        let c = render(0x258F, 10, 21);
        assert_eq!(lit(&c), 21); // 10 / 8 = 1.25 rounds to 1 column
        let c = render(0x2595, 10, 21);
        assert_eq!(c.get(9, 0), ON);
        assert_eq!(c.get(8, 0), 0);
    }

    #[test]
    fn shades_are_flat() {
        for (cp, shade) in [(0x2591, LIGHT), (0x2592, MEDIUM), (0x2593, DARK), (0x2588, ON)] {
            let c = render(cp, 9, 17);
            assert!(cell(&c).iter().all(|&v| v == shade), "{cp:#X}");
        }
        let c = render(0x1FB8C, 10, 20);
        assert_eq!((c.get(0, 5), c.get(4, 5), c.get(5, 5)), (MEDIUM, MEDIUM, 0));
    }

    #[test]
    fn quadrants_mirror() {
        for (w, h) in SIZES {
            let n = |cp| lit(&render(cp, w, h));
            // Single quadrants are all the same size, whichever corner.
            assert_eq!(n(0x2598), n(0x259D), "{w}x{h}");
            assert_eq!(n(0x2598), n(0x2596), "{w}x{h}");
            assert_eq!(n(0x2598), n(0x2597), "{w}x{h}");
            // The diagonal pairs are two of them, sharing at most the
            // centre pixel an odd cell rounds up into.
            for cp in [0x259A, 0x259E] {
                let two = 2 * n(0x2598);
                assert!([two, two - 1].contains(&n(cp)), "{cp:#X} at {w}x{h}");
            }
        }
        // 7 px wide: both halves are 4 px, overlapping in column 3.
        let (l, r) = (render(0x2598, 7, 14), render(0x259D, 7, 14));
        assert_eq!((l.get(3, 0), r.get(3, 0)), (ON, ON));
        assert_eq!((l.get(4, 0), r.get(2, 0)), (0, 0));
    }

    #[test]
    fn quadrant_composites_match_their_parts() {
        for (w, h) in SIZES {
            let whole = cell(&render(0x259B, w, h)); // ▛ = tl + tr + bl
            let mut parts = vec![0u8; whole.len()];
            for cp in [0x2598, 0x259D, 0x2596] {
                for (p, v) in parts.iter_mut().zip(cell(&render(cp, w, h))) {
                    *p = (*p).max(v);
                }
            }
            assert_eq!(whole, parts, "{w}x{h}");
        }
    }

    #[test]
    fn braille_dots_stay_inside() {
        for (w, h) in [(9, 17), (18, 36), (7, 15), (10, 21), (4, 8), (3, 6)] {
            for cp in 0x2800..=0x28FF {
                let c = render(cp, w, h);
                assert!(stays_in_cell(&c), "{cp:#X} in {w}x{h}");
            }
            assert_eq!(lit(&render(0x2800, w, h)), 0);
            // Eight identical square dots, none touching.
            let full = lit(&render(0x28FF, w, h));
            assert_eq!(full % 8, 0, "{w}x{h}");
            assert_eq!(lit(&render(0x2801, w, h)), full / 8, "{w}x{h}");
            let side = ((full / 8) as f64).sqrt() as usize;
            assert_eq!(side * side, full / 8, "{w}x{h}: dots are squares");
            assert!(side >= 1, "{w}x{h}: dots exist");
        }
    }

    #[test]
    fn braille_bits_map_to_dots() {
        // ⠁ top-left, ⠈ top-right, ⡀ bottom-left, ⢀ bottom-right (18x36).
        let tl = render(0x2801, 18, 36);
        let tr = render(0x2808, 18, 36);
        let bl = render(0x2840, 18, 36);
        let br = render(0x2880, 18, 36);
        let centroid = |c: &Canvas| {
            let (mut sx, mut sy, mut n) = (0, 0, 0);
            for y in 0..36 {
                for x in 0..18 {
                    if c.get(x, y) != 0 {
                        sx += x;
                        sy += y;
                        n += 1;
                    }
                }
            }
            (sx / n, sy / n)
        };
        let (a, b, d, e) = (centroid(&tl), centroid(&tr), centroid(&bl), centroid(&br));
        assert!(a.0 < 9 && a.1 < 18);
        assert!(b.0 >= 9 && b.1 < 18);
        assert!(d.0 < 9 && d.1 >= 18);
        assert!(e.0 >= 9 && e.1 >= 18);
        assert_eq!(a.1, b.1);
        assert_eq!(d.1, e.1);
    }

    #[test]
    fn sextants_are_distinct_and_not_halves() {
        for (w, h) in SIZES {
            let left = cell(&render(0x258C, w, h));
            let right = cell(&render(0x2590, w, h));
            let full = cell(&render(0x2588, w, h));
            let mut seen: Vec<Vec<u8>> = Vec::new();
            for cp in 0x1FB00..=0x1FB3B {
                let s = cell(&render(cp, w, h));
                assert!(s.iter().any(|&v| v != 0), "{cp:#X} empty at {w}x{h}");
                assert_ne!(s, left, "{cp:#X} is ▌");
                assert_ne!(s, right, "{cp:#X} is ▐");
                assert_ne!(s, full, "{cp:#X} is █");
                assert!(!seen.contains(&s), "{cp:#X} repeats at {w}x{h}");
                seen.push(s);
            }
            assert_eq!(seen.len(), 60);
        }
    }

    #[test]
    fn sextant_positions() {
        // 🬀 is the top-left sextant, 🬻 (1FB3B) everything but the top-left.
        let c = render(0x1FB00, 8, 18);
        assert_eq!((c.get(0, 0), c.get(3, 5), c.get(4, 0), c.get(0, 6)), (ON, ON, 0, 0));
        let c = render(0x1FB3B, 8, 18);
        assert_eq!((c.get(0, 0), c.get(3, 5), c.get(4, 0), c.get(0, 6)), (0, 0, ON, ON));
    }

    #[test]
    fn octant_bits_map_to_eighths() {
        // 8x16: columns split at 4, rows at 4, 8, 12.
        let region = |c: &Canvas, x0, x1, y0, y1| {
            (0..16).all(|y| {
                (0..8).all(|x| {
                    let inside = (x0..x1).contains(&x) && (y0..y1).contains(&y);
                    (c.get(x, y) == ON) == inside
                })
            })
        };
        // BLOCK OCTANT-3: left column, second row.
        assert!(region(&render(0x1CD00, 8, 16), 0, 4, 4, 8));
        // OCTANT-4: right column, second row.
        assert!(region(&render(0x1CD03, 8, 16), 4, 8, 4, 8));
        // OCTANT-23 sits at index 1: top-right and middle-left.
        let c = render(0x1CD01, 8, 16);
        assert_eq!((c.get(4, 0), c.get(0, 4), c.get(0, 0), c.get(4, 4)), (ON, ON, 0, 0));
        // OCTANT-2345678: everything but the top-left eighth.
        let c = render(0x1CDE5, 8, 16);
        assert_eq!((c.get(0, 0), c.get(3, 3), c.get(4, 0), c.get(0, 4), c.get(7, 15)), (0, 0, ON, ON, ON));
        assert_eq!(lit(&c), 8 * 16 - 4 * 4);
    }

    #[test]
    fn octants_are_distinct() {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for cp in 0x1CD00..=0x1CDE5 {
            let s = cell(&render(cp, 8, 16));
            assert!(s.iter().any(|&v| v != 0), "{cp:#X} empty");
            assert!(!seen.contains(&s), "{cp:#X} repeats");
            seen.push(s);
        }
        assert_eq!(seen.len(), 230);
        assert_eq!(OCTANTS.len(), 230);
    }

    #[test]
    fn eighth_strips_tile_the_width() {
        for (w, h) in SIZES {
            let mut cols = vec![false; w as usize];
            let mut mark = |c: &Canvas| {
                for x in 0..w as i32 {
                    if (0..h as i32).all(|y| c.get(x, y) == ON) {
                        cols[x as usize] = true;
                    }
                }
            };
            mark(&render(0x258F, w, h));
            for cp in 0x1FB70..=0x1FB75 {
                mark(&render(cp, w, h));
            }
            mark(&render(0x2595, w, h));
            assert!(cols.iter().all(|&b| b), "{w}x{h}: {cols:?}");
        }
    }

    #[test]
    fn horizontal_strips_hit_both_edges() {
        for (w, h) in SIZES {
            for cp in 0x1FB76..=0x1FB7B {
                let c = render(cp, w, h);
                let rows: Vec<i32> = (0..h as i32).filter(|&y| c.get(0, y) == ON).collect();
                assert!(!rows.is_empty(), "{cp:#X} {w}x{h}");
                for &y in &rows {
                    assert_eq!(c.get(w as i32 - 1, y), ON);
                }
            }
        }
        // 1FB81: eighths 0, 2, 4 and 7 of a 16 px cell.
        let c = render(0x1FB81, 8, 16);
        let rows: Vec<i32> = (0..16).filter(|&y| c.get(0, y) == ON).collect();
        assert_eq!(rows, vec![0, 1, 4, 5, 8, 9, 14, 15]);
    }

    #[test]
    fn corner_eighth_combos() {
        let c = render(0x1FB7C, 8, 16); // left + lower
        assert_eq!((c.get(0, 0), c.get(1, 0), c.get(7, 15), c.get(7, 13)), (ON, 0, ON, 0));
        let c = render(0x1FB7E, 8, 16); // right + upper
        assert_eq!((c.get(7, 15), c.get(7, 0), c.get(0, 0), c.get(0, 2)), (ON, ON, ON, 0));
    }

    #[test]
    fn checkerboards_are_complementary() {
        for (w, h) in SIZES {
            let (a, b) = (cell(&render(0x1FB95, w, h)), cell(&render(0x1FB96, w, h)));
            for (i, (x, y)) in a.iter().zip(&b).enumerate() {
                assert_eq!(*x as u32 + *y as u32, ON as u32, "{w}x{h} pixel {i}");
            }
        }
    }

    #[test]
    fn mosaics_draw_inside_the_cell() {
        for (w, h) in SIZES {
            for cp in 0x1FB3C..=0x1FB67 {
                let c = render(cp, w, h);
                assert!(lit(&c) > 0, "{cp:#X} empty at {w}x{h}");
                assert!(stays_in_cell(&c), "{cp:#X} spills at {w}x{h}");
            }
        }
        // 🬼: the lower-left corner is filled, the upper-right is not.
        let c = render(0x1FB3C, 12, 24);
        assert_eq!((c.get(0, 23), c.get(11, 0), c.get(11, 23), c.get(0, 0)), (ON, 0, 0, 0));
    }

    #[test]
    fn mosaics_mirror() {
        // 🬼 lower-left and 🭇 lower-right are each other's reflection.
        for (w, h) in [(12, 24), (10, 20), (9, 18)] {
            let mut a = render(0x1FB3C, w, h);
            let b = render(0x1FB47, w, h);
            a.flip_h();
            let diff = a.buf.iter().zip(&b.buf).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
            assert!(diff <= 1, "{w}x{h}: {diff}");
        }
    }

    #[test]
    fn mosaic_table_is_well_formed() {
        for p in MOSAICS {
            assert_eq!(p.len(), 15);
            assert!([3, 7, 11].iter().all(|&i| p[i] == b'|'));
            assert!(p.iter().all(|b| b"#./\\|".contains(b)));
        }
    }

    #[test]
    fn inverted_edge_triangles_complement() {
        for (w, h) in SIZES {
            for i in 0..4 {
                let notch = cell(&render(0x1FB68 + i, w, h));
                let tri = cell(&render(0x1FB6C + i, w, h));
                for (a, b) in notch.iter().zip(&tri) {
                    assert_eq!(*a as u32 + *b as u32, 255, "{w}x{h} #{i}");
                }
                assert!(stays_in_cell(&render(0x1FB68 + i, w, h)));
            }
        }
    }

    #[test]
    fn edge_triangle_touches_its_edge() {
        let c = render(0x1FB6C, 10, 20); // left
        assert!(c.get(0, 0) > 0 && c.get(0, 19) > 0 && c.get(9, 0) == 0 && c.get(9, 19) == 0);
        assert_eq!(c.get(0, 10), ON);
        let c = render(0x1FB6D, 10, 20); // top
        assert!(c.get(0, 0) > 0 && c.get(9, 0) > 0 && c.get(0, 19) == 0 && c.get(9, 19) == 0);
        assert_eq!(c.get(5, 0), ON);
    }

    #[test]
    fn diagonals_draw() {
        for (w, h) in SIZES {
            for cp in (0x1FBA0..=0x1FBAF).chain(0x1FBD0..=0x1FBDF).chain([0x1FB98, 0x1FB99, 0x1FB9A, 0x1FB9B]) {
                assert!(lit(&render(cp, w, h)) > 0, "{cp:#X} empty at {w}x{h}");
            }
            for cp in [0x1FBBD, 0x1FBBE, 0x1FBBF, 0x1FB98, 0x1FB99] {
                assert!(stays_in_cell(&render(cp, w, h)), "{cp:#X} spills at {w}x{h}");
            }
        }
        // 🯐 runs from the middle of the right edge to the lower-left corner.
        let c = render(0x1FBD0, 20, 20);
        assert!(c.get(0, 19) > 0 && c.get(19, 10) > 0 && c.get(0, 0) == 0);
    }

    #[test]
    fn corner_triangles_cover_a_half() {
        let c = render(0x1FB9C, 10, 20); // upper-left, medium
        assert_eq!(c.get(0, 0), MEDIUM);
        assert!(c.get(0, 19) > 0 && c.get(0, 19) < MEDIUM); // the thin tip
        assert_eq!(c.get(9, 19), 0);
        // The two triangles of a cell tile it: the hypotenuse pixels split their coverage.
        let d = render(0x1FB9E, 10, 20); // lower-right
        for y in 0..20 {
            for x in 0..10 {
                let sum = c.get(x, y) as u32 + d.get(x, y) as u32;
                assert!((MEDIUM as u32 - 8..=MEDIUM as u32 + 8).contains(&sum), "{x},{y}: {sum}");
            }
        }
    }

    #[test]
    fn circles_are_cropped_and_anchored() {
        for (w, h) in SIZES {
            for cp in 0x1FBE0..=0x1FBEF {
                if (0x1FBE4..=0x1FBE7).contains(&cp) {
                    continue;
                }
                let c = render(cp, w, h);
                assert!(lit(&c) > 0, "{cp:#X} empty at {w}x{h}");
                assert!(stays_in_cell(&c), "{cp:#X} spills at {w}x{h}");
            }
        }
        // 🯨 filled, centred on the top edge: the top middle is on, the bottom corners are off.
        let c = render(0x1FBE8, 10, 20);
        assert_eq!((c.get(5, 0), c.get(0, 19), c.get(9, 19)), (ON, 0, 0));
        // 🯬 filled, centred on the top-right corner.
        let c = render(0x1FBEC, 10, 20);
        assert_eq!((c.get(9, 0), c.get(0, 19)), (ON, 0));
    }

    #[test]
    fn centred_half_blocks() {
        let c = render(0x1FBE4, 10, 20);
        assert_eq!((c.get(2, 0), c.get(6, 9), c.get(1, 0), c.get(7, 0)), (ON, ON, 0, 0));
        assert_eq!(c.get(5, 10), 0);
        assert_eq!(lit(&c), 5 * 10);
    }

    #[test]
    fn thirds_blocks() {
        assert_eq!(lit(&render(0x1FBCE, 9, 17)), 6 * 17);
        assert_eq!(lit(&render(0x1FBCF, 9, 17)), 3 * 17);
    }

    #[test]
    fn heavy_vertical_light_horizontal() {
        // 🮯 with a 1 px box thickness: a 2 px vertical and a 1 px horizontal.
        let c = render(0x1FBAF, 10, 20);
        assert_eq!(c.get(4, 0), ON);
        assert_eq!(c.get(5, 0), ON);
        assert_eq!(c.get(3, 0), 0);
        assert_eq!(c.get(0, 9), ON);
        assert_eq!(c.get(0, 8), 0);
    }

    #[test]
    fn supplement_extras_draw_inside_the_cell() {
        for (w, h) in [(9, 17), (10, 21), (8, 16), (18, 36)] {
            for cp in (0x1CC1B..=0x1CC1E)
                .chain(0x1CC21..=0x1CC3F)
                .chain([0x1CE00, 0x1CE01, 0x1CE0B, 0x1CE0C])
                .chain(0x1CE16..=0x1CE19)
                .chain(0x1CE51..=0x1CEAF)
            {
                let c = render(cp, w, h);
                assert!(lit(&c) > 0, "{cp:#X} empty at {w}x{h}");
                assert!(stays_in_cell(&c), "{cp:#X} spills at {w}x{h}");
            }
        }
    }

    #[test]
    fn sixteenth_blocks_are_quarters_of_quarters() {
        // 🺐 upper-left 1/16 of a 16x16 cell is a 4x4 square at the corner.
        let c = render(0x1CE90, 16, 16);
        assert_eq!(lit(&c), 16);
        assert_eq!((c.get(0, 0), c.get(3, 3), c.get(4, 0)), (ON, ON, 0));
        // 🺯 lower half of the right quarter.
        let c = render(0x1CEAF, 16, 16);
        assert_eq!(lit(&c), 4 * 8);
        assert_eq!((c.get(12, 8), c.get(15, 15), c.get(12, 7)), (ON, ON, 0));
    }

    #[test]
    fn separated_pieces_leave_gaps() {
        // All four separated quadrants of 12x24: a 1 px margin, 4x10 pieces, a 2 px gutter.
        let c = render(0x1CC2F, 12, 24);
        assert_eq!(lit(&c), 4 * 4 * 10);
        let cols: Vec<i32> = (0..12).filter(|&x| c.get(x, 1) == ON).collect();
        assert_eq!(cols, vec![1, 2, 3, 4, 7, 8, 9, 10]);
        let rows: Vec<i32> = (0..24).filter(|&y| c.get(1, y) == ON).collect();
        assert_eq!(rows, (1..11).chain(13..23).collect::<Vec<_>>());
        // All six separated sextants: three 6-row bands.
        let s = render(0x1CE8F, 12, 24);
        assert_eq!(lit(&s), 6 * 4 * 6);
        let rows: Vec<i32> = (0..24).filter(|&y| s.get(1, y) == ON).collect();
        assert_eq!(rows, (1..7).chain(9..15).chain(17..23).collect::<Vec<_>>());
    }

    #[test]
    fn corner_ticks() {
        // 🰛 horizontal line with a tick at the upper right.
        let c = render(0x1CC1B, 10, 20);
        assert_eq!((c.get(0, 9), c.get(9, 0), c.get(9, 8), c.get(0, 0)), (ON, ON, ON, 0));
        // 🸖 vertical line with a tick across the top right.
        let c = render(0x1CE16, 10, 20);
        assert_eq!((c.get(4, 19), c.get(9, 0), c.get(9, 1)), (ON, ON, 0));
    }

    #[test]
    fn unimplemented_codepoints_fall_through() {
        let m = metrics(9, 17);
        for cp in [
            0x25FF, 0x2500, 0x257F, 0x25A0, 0x1CC1F, 0x1CC20, 0x1CC40, 0x1CCFF, 0x1CDE6, 0x1CE02, 0x1CE0D, 0x1CE1A,
            0x1CE50, 0x1CEB0, 0x1FAFF, 0x1FBB0, 0x1FBBC, 0x1FBC0, 0x1FBCD, 0x1FBF0, 0x1FBFF, 0x41, 0xE0B0,
        ] {
            let mut c = Canvas::new(9, 17);
            assert!(!draw(cp, &mut c, 9, 17, &m), "{cp:#X}");
            assert!(c.buf.iter().all(|&v| v == 0), "{cp:#X} drew while declining");
        }
    }

    #[test]
    fn draws_exactly_what_covers_claims_here() {
        let m = metrics(9, 17);
        for cp in (0x2580..=0x259F).chain(0x2800..=0x28FF).chain(0x1CC00..=0x1CEFF).chain(0x1FB00..=0x1FBFF) {
            let mut c = Canvas::new(9, 17);
            assert_eq!(draw(cp, &mut c, 9, 17, &m), covers(cp), "{cp:#X}");
        }
    }
}
