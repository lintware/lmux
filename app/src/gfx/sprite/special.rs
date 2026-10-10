//! Underline styles, strikethrough, overline and cursor shapes, ported from Ghostty's
//! `src/font/sprite/draw/special.zig` (MIT). These are the sprites that have no Unicode
//! code point; `mod.rs` numbers them past the end of Unicode.

use std::f64::consts::{FRAC_1_SQRT_2, PI};

use tiny_skia::{LineCap, PathBuilder};

use super::{
    Canvas, CURSOR_BAR, CURSOR_HOLLOW_RECT, CURSOR_RECT, CURSOR_UNDERLINE, OFF, ON, OVERLINE, STRIKETHROUGH,
    UNDERLINE, UNDERLINE_CURLY, UNDERLINE_DASHED, UNDERLINE_DOTTED, UNDERLINE_DOUBLE,
};
use crate::gfx::metrics::Metrics;

/// Draw `cp` if this module handles it; false otherwise.
pub fn draw(cp: u32, c: &mut Canvas, width: u32, height: u32, m: &Metrics) -> bool {
    match cp {
        UNDERLINE => underline(c, width, height, m),
        UNDERLINE_DOUBLE => underline_double(c, width, height, m),
        UNDERLINE_DOTTED => underline_dotted(c, width, height, m),
        UNDERLINE_DASHED => underline_dashed(c, width, height, m),
        UNDERLINE_CURLY => underline_curly(c, width, height, m),
        STRIKETHROUGH => strikethrough(c, width, m),
        OVERLINE => overline(c, width, m),
        CURSOR_RECT => cursor_rect(c, width, height),
        CURSOR_HOLLOW_RECT => cursor_hollow_rect(c, width, height, m),
        CURSOR_BAR => cursor_bar(c, height, m),
        CURSOR_UNDERLINE => cursor_underline(c, width, height, m),
        _ => return false,
    }
    true
}

/// The lowest row a stroke `thickness` tall may start on without leaving the canvas, which extends a
/// quarter cell below the cell (`height +| padding_y -| thickness`).
fn lowest_row(c: &Canvas, height: u32, thickness: u32) -> u32 {
    height.saturating_add(c.pad_y).saturating_sub(thickness)
}

// special.zig:13
fn underline(c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    let y = m.underline_position.min(lowest_row(c, height, m.underline_thickness));
    c.rect(0, y as i32, width as i32, m.underline_thickness as i32, ON);
}

// special.zig:39
fn underline_double(c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    let t = m.underline_thickness;
    let y = m.underline_position.min(lowest_row(c, height, 2 * t));
    // One line above the underline position and one a thickness below it: a "negative" underline where the
    // single one would sit.
    c.rect(0, y.saturating_sub(t) as i32, width as i32, t as i32, ON);
    c.rect(0, y.saturating_add(t) as i32, width as i32, t as i32, ON);
}

// special.zig:74
fn underline_dotted(c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    let (fw, fh) = (width as f64, height as f64);
    let pos = m.underline_position as f64;
    let thick = m.underline_thickness as f64;

    // The diameter is sqrt2 * the usual thickness, since dots at the plain thickness look anemic.
    let radius = FRAC_1_SQRT_2 * thick;

    // The centre of the underline stem, but no lower than the canvas allows.
    let y = (pos + 0.5 * thick).min(fh + c.pad_y as f64 - radius.ceil());

    let dot_count = (fw / (4.0 * radius))
        .ceil()
        // Not so many that the gap between dots is under their radius,
        .min((fw / (3.0 * radius)).floor())
        // nor under a single pixel.
        .min((fw / (2.0 * radius + 1.0)).floor())
        // At least one dot per cell.
        .max(1.0);

    // The cell is divided into `dot_count` areas with a dot centred in each.
    let step = fw / dot_count;
    let mut pb = PathBuilder::new();
    let mut x = step / 2.0;
    for _ in 0..dot_count as usize {
        pb.push_circle(x as f32, y as f32, radius as f32);
        x += step;
    }
    if let Some(path) = pb.finish() {
        c.fill_path(&path, ON);
    }
}

// special.zig:135
fn underline_dashed(c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    let y = m.underline_position.min(lowest_row(c, height, m.underline_thickness));
    let dash_width = width / 3 + 1;
    let dash_count = width / dash_width + 1;
    let mut i = 0;
    while i < dash_count {
        c.rect((i * dash_width) as i32, y as i32, dash_width as i32, m.underline_thickness as i32, ON);
        i += 2;
    }
}

// special.zig:167
fn underline_curly(c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    let (fw, fh) = (width as f64, height as f64);
    let pos = m.underline_position as f64;
    let line_width = m.underline_thickness as f64;

    // Empirically this looks good.
    let amplitude = fw / PI;

    // Never exceed the drawable area; this can still be outside the cell by up to a quarter of its height.
    let top = pos.min(fh + c.pad_y as f64 - amplitude - line_width);
    let bottom = top + amplitude;

    // Curvature multiplier; 0.4 makes a smooth wiggle.
    let r = 0.4;
    let center = 0.5 * fw;

    // A single cycle of a wave that peaks at the centre of the cell.
    let mut pb = PathBuilder::new();
    pb.move_to(0.0, bottom as f32);
    pb.cubic_to(
        (center * r) as f32,
        bottom as f32,
        (center - center * r) as f32,
        top as f32,
        center as f32,
        top as f32,
    );
    pb.cubic_to(
        (center + center * r) as f32,
        top as f32,
        (fw - center * r) as f32,
        bottom as f32,
        fw as f32,
        bottom as f32,
    );
    // Round caps: adjacent underlines overlap there and so don't show, but the ends look cleaner.
    if let Some(path) = pb.finish() {
        c.stroke_path(&path, line_width as f32, LineCap::Round, ON);
    }
}

// special.zig:237
fn strikethrough(c: &mut Canvas, width: u32, m: &Metrics) {
    c.rect(0, m.strikethrough_position as i32, width as i32, m.strikethrough_thickness as i32, ON);
}

// special.zig:255
fn overline(c: &mut Canvas, width: u32, m: &Metrics) {
    // May go above the cell a bit, but never beyond the quarter cell of canvas above it.
    let y = m.overline_position.max(-(c.pad_y as i32));
    c.rect(0, y, width as i32, m.overline_thickness as i32, ON);
}

// special.zig:282
fn cursor_rect(c: &mut Canvas, width: u32, height: u32) {
    c.rect(0, 0, width as i32, height as i32, ON);
}

// special.zig:300
fn cursor_hollow_rect(c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    // Fill the whole rect, then hollow out the inside.
    let t = m.cursor_thickness;
    c.rect(0, 0, width as i32, height as i32, ON);
    c.rect(t as i32, t as i32, width.saturating_sub(t * 2) as i32, height.saturating_sub(t * 2) as i32, OFF);
}

// special.zig:325
fn cursor_bar(c: &mut Canvas, height: u32, m: &Metrics) {
    // Half the thickness hangs over the cell's left edge so the bar sits between characters. Rounded up:
    // a 1px bar shifted left a pixel looks better than one that isn't.
    let t = m.cursor_thickness;
    c.rect(-(t.div_ceil(2) as i32), 0, t as i32, height as i32, ON);
}

// special.zig:348
fn cursor_underline(c: &mut Canvas, width: u32, height: u32, m: &Metrics) {
    let y = m.underline_position.min(lowest_row(c, height, m.underline_thickness));
    c.rect(0, y as i32, width as i32, m.cursor_thickness as i32, ON);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(w: u32, h: u32) -> Metrics {
        Metrics {
            cell_width: w,
            cell_height: h,
            cell_baseline: h / 4,
            underline_position: h - h / 4 + 1,
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

    fn render(cp: u32, w: u32, h: u32, m: &Metrics) -> Canvas {
        let mut c = Canvas::new(w, h);
        assert!(draw(cp, &mut c, w, h, m));
        c
    }

    /// Rows (cell-relative, padding included) that have any ink in column `x`.
    fn rows_at(c: &Canvas, x: i32) -> Vec<i32> {
        let py = c.pad_y as i32;
        (-py..c.height as i32 + py).filter(|&y| c.get(x, y) != 0).collect()
    }

    fn ink_rows(c: &Canvas) -> Vec<i32> {
        let (px, py) = (c.pad_x as i32, c.pad_y as i32);
        (-py..c.height as i32 + py)
            .filter(|&y| (-px..c.width as i32 + px).any(|x| c.get(x, y) != 0))
            .collect()
    }

    #[test]
    fn unhandled_codepoints_are_left_alone() {
        let m = metrics(9, 17);
        let mut c = Canvas::new(9, 17);
        assert!(!draw(0x2500, &mut c, 9, 17, &m));
        assert!(!draw(0xE0B0, &mut c, 9, 17, &m));
        assert!(c.buf.iter().all(|&v| v == 0));
    }

    #[test]
    fn underline_rows_are_position_through_thickness() {
        for (w, h, thick) in [(9, 17, 1), (18, 36, 2), (10, 21, 3)] {
            let mut m = metrics(w, h);
            m.underline_thickness = thick;
            let c = render(UNDERLINE, w, h, &m);
            let want: Vec<i32> = (m.underline_position as i32..(m.underline_position + thick) as i32).collect();
            assert_eq!(ink_rows(&c), want, "{w}x{h}");
            // The whole width, and not a pixel beyond it.
            assert!((0..w as i32).all(|x| c.get(x, want[0]) == ON));
            assert_eq!((c.get(-1, want[0]), c.get(w as i32, want[0])), (0, 0));
        }
    }

    #[test]
    fn underline_is_pulled_back_onto_the_canvas() {
        // A position way below the cell must not vanish: it is clamped to the quarter cell of padding.
        let mut m = metrics(8, 16);
        m.underline_position = 100;
        let c = render(UNDERLINE, 8, 16, &m);
        assert_eq!(ink_rows(&c), vec![16 + 4 - 1]);
    }

    #[test]
    fn double_underline_brackets_the_single_position() {
        let m = metrics(9, 17);
        let c = render(UNDERLINE_DOUBLE, 9, 17, &m);
        let pos = m.underline_position as i32;
        assert_eq!(ink_rows(&c), vec![pos - 1, pos + 1]);
    }

    #[test]
    fn dashed_underline_alternates_and_is_pixel_exact() {
        let m = metrics(9, 17);
        let c = render(UNDERLINE_DASHED, 9, 17, &m);
        let y = m.underline_position as i32;
        // dash_width = 9/3 + 1 = 4, so ink on [0,4) and [8,12), a gap on [4,8). The third dash runs off the
        // cell and is cut at the padding, which is 2px wide here.
        let row: Vec<u8> = (0..11).map(|x| c.get(x, y)).collect();
        assert_eq!(row, [ON, ON, ON, ON, 0, 0, 0, 0, ON, ON, ON]);
        assert_eq!(c.get(11, y), 0);
        assert_eq!(ink_rows(&c), vec![y]);
    }

    #[test]
    fn dotted_underline_has_gaps_and_stays_in_its_band() {
        let m = metrics(10, 20);
        let c = render(UNDERLINE_DOTTED, 10, 20, &m);
        let rows = ink_rows(&c);
        let pos = m.underline_position as i32;
        // Dots are sqrt2 * thickness in radius around the stem's centre.
        assert!(rows.first().unwrap() >= &(pos - 1) && rows.last().unwrap() <= &(pos + 2), "{rows:?}");
        // At least one column in the cell is empty between dots, and at least one has ink.
        let cols: Vec<bool> = (0..10).map(|x| rows.iter().any(|&y| c.get(x, y) != 0)).collect();
        assert!(cols.iter().any(|&v| v) && cols.iter().any(|&v| !v), "{cols:?}");
    }

    #[test]
    fn curly_underline_starts_and_ends_at_the_same_height() {
        for (w, h) in [(9, 17), (18, 36), (10, 21)] {
            let m = metrics(w, h);
            let c = render(UNDERLINE_CURLY, w, h, &m);
            let (left, right) = (rows_at(&c, 0), rows_at(&c, w as i32 - 1));
            assert!(!left.is_empty(), "{w}x{h}");
            assert_eq!(left, right, "{w}x{h}");
            // The wave peaks at the middle, above where it starts.
            let mid = rows_at(&c, w as i32 / 2);
            assert!(mid.first() < left.first(), "{w}x{h}: {mid:?} vs {left:?}");
            // Round caps spill sideways into the padding, but only by half a thickness.
            assert!(c.get(-(c.pad_x as i32) - 1, left[0]) == 0);
        }
    }

    #[test]
    fn strikethrough_and_overline_rows() {
        let mut m = metrics(9, 17);
        m.strikethrough_thickness = 2;
        let c = render(STRIKETHROUGH, 9, 17, &m);
        let pos = m.strikethrough_position as i32;
        assert_eq!(ink_rows(&c), vec![pos, pos + 1]);

        let c = render(OVERLINE, 9, 17, &m);
        assert_eq!(ink_rows(&c), vec![0]);

        // Above the cell, but never beyond the padding.
        m.overline_position = -2;
        assert_eq!(ink_rows(&render(OVERLINE, 9, 17, &m)), vec![-2]);
        m.overline_position = -50;
        assert_eq!(ink_rows(&render(OVERLINE, 9, 17, &m)), vec![-(17 / 4)]);
    }

    #[test]
    fn block_cursor_fills_the_cell_exactly() {
        let m = metrics(9, 17);
        let c = render(CURSOR_RECT, 9, 17, &m);
        for y in 0..17 {
            for x in 0..9 {
                assert_eq!(c.get(x, y), ON);
            }
        }
        assert_eq!(c.buf.iter().filter(|&&v| v != 0).count(), 9 * 17);
    }

    #[test]
    fn hollow_cursor_interior_is_empty() {
        for thick in [1u32, 2] {
            let mut m = metrics(10, 20);
            m.cursor_thickness = thick;
            let c = render(CURSOR_HOLLOW_RECT, 10, 20, &m);
            let t = thick as i32;
            for y in 0..20 {
                for x in 0..10 {
                    let border = x < t || y < t || x >= 10 - t || y >= 20 - t;
                    assert_eq!(c.get(x, y), if border { ON } else { 0 }, "({x},{y}) t={thick}");
                }
            }
        }
    }

    #[test]
    fn bar_cursor_straddles_the_left_edge() {
        let mut m = metrics(9, 17);
        for (thick, first) in [(1u32, -1), (2, -1), (3, -2), (4, -2)] {
            m.cursor_thickness = thick;
            let c = render(CURSOR_BAR, 9, 17, &m);
            let cols: Vec<i32> = (-3..9).filter(|&x| c.get(x, 0) != 0).collect();
            let want: Vec<i32> = (first..first + thick as i32).collect();
            assert_eq!(cols, want, "thickness {thick}");
            assert!((0..17).all(|y| c.get(first, y) == ON), "full height");
            assert_eq!(c.get(first, 17), 0);
        }
    }

    #[test]
    fn underline_cursor_sits_on_the_underline_row() {
        let mut m = metrics(9, 17);
        m.cursor_thickness = 2;
        let c = render(CURSOR_UNDERLINE, 9, 17, &m);
        let pos = m.underline_position as i32;
        assert_eq!(ink_rows(&c), vec![pos, pos + 1]);
    }
}
