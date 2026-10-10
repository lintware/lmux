//! Characters the renderer draws itself instead of taking them from a font,
//! ported from Ghostty's `src/font/sprite/` (MIT): box drawing, blocks,
//! braille, Powerline, legacy computing mosaics, underline styles and the
//! cursor. Drawn to the exact cell size, they join seamlessly with their
//! neighbours whatever the font.
//!
//! Each glyph is drawn into an A8 canvas the size of the cell plus a quarter
//! cell of padding on every side (diagonals overshoot, the bar cursor
//! straddles the left edge), then trimmed to its non-empty pixels.

pub mod block;
pub mod r#box;
pub mod powerline;
pub mod special;

use tiny_skia::{FillRule, LineCap, Paint, Path, Pixmap, Stroke, Transform};

use super::metrics::Metrics;

/// Decorations and cursors, as Ghostty numbers them: past the end of Unicode.
pub const UNDERLINE: u32 = 0x200000;
pub const UNDERLINE_DOUBLE: u32 = 0x200001;
pub const UNDERLINE_DOTTED: u32 = 0x200002;
pub const UNDERLINE_DASHED: u32 = 0x200003;
pub const UNDERLINE_CURLY: u32 = 0x200004;
pub const STRIKETHROUGH: u32 = 0x200005;
pub const OVERLINE: u32 = 0x200006;
pub const CURSOR_RECT: u32 = 0x200007;
pub const CURSOR_HOLLOW_RECT: u32 = 0x200008;
pub const CURSOR_BAR: u32 = 0x200009;
pub const CURSOR_UNDERLINE: u32 = 0x20000A;

/// Light / heavy stroke widths from the box thickness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thickness {
    SuperLight,
    Light,
    Heavy,
}

impl Thickness {
    pub fn px(self, base: u32) -> u32 {
        match self {
            Thickness::SuperLight => (base / 2).max(1),
            Thickness::Light => base,
            Thickness::Heavy => base * 2,
        }
    }
}

/// Ghostty's shade levels.
pub const OFF: u8 = 0x00;
pub const LIGHT: u8 = 0x40;
pub const MEDIUM: u8 = 0x80;
pub const DARK: u8 = 0xC0;
pub const ON: u8 = 0xFF;

/// A fraction of the cell, snapped so mirrored pieces come out the same
/// size (`common.zig` `Fraction.min` / `max`).
pub fn frac_min(f: f64, size: u32) -> i32 {
    let s = size as f64;
    (s - ((1.0 - f) * s).round()) as i32
}

pub fn frac_max(f: f64, size: u32) -> i32 {
    ((f * size as f64).round()) as i32
}

/// An A8 drawing surface. Coordinates are pixels relative to the cell's
/// top-left corner, y down; anything inside the padding is kept, anything
/// beyond it dropped.
pub struct Canvas {
    pub buf: Vec<u8>,
    /// The drawing area (cell width × grid width, cell or cursor height).
    pub width: u32,
    pub height: u32,
    pub pad_x: u32,
    pub pad_y: u32,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Canvas {
        let (pad_x, pad_y) = (width / 4, height / 4);
        let (sw, sh) = (width + 2 * pad_x, height + 2 * pad_y);
        Canvas { buf: vec![0; (sw * sh) as usize], width, height, pad_x, pad_y }
    }

    pub fn surface_width(&self) -> u32 {
        self.width + 2 * self.pad_x
    }

    pub fn surface_height(&self) -> u32 {
        self.height + 2 * self.pad_y
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        let sx = x + self.pad_x as i32;
        let sy = y + self.pad_y as i32;
        if sx < 0 || sy < 0 || sx >= self.surface_width() as i32 || sy >= self.surface_height() as i32 {
            return None;
        }
        Some(sy as usize * self.surface_width() as usize + sx as usize)
    }

    /// The value at a cell-relative pixel (0 outside the surface).
    pub fn get(&self, x: i32, y: i32) -> u8 {
        self.index(x, y).map(|i| self.buf[i]).unwrap_or(0)
    }

    /// Overwrite one pixel (no blending), as Ghostty's `putPixel`.
    pub fn pixel(&mut self, x: i32, y: i32, alpha: u8) {
        if let Some(i) = self.index(x, y) {
            self.buf[i] = alpha;
        }
    }

    /// Overwrite `[x, x+w) × [y, y+h)`. Never antialiased.
    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, alpha: u8) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.pixel(xx, yy, alpha);
            }
        }
    }

    /// `rect` from two corners, in any order: fills `[x0,x1) × [y0,y1)`.
    pub fn r#box(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, alpha: u8) {
        let (l, r) = (x0.min(x1), x0.max(x1));
        let (t, b) = (y0.min(y1), y0.max(y1));
        self.rect(l, t, r - l, b - t, alpha);
    }

    /// Composite antialiased coverage from a tiny-skia drawing, "over":
    /// `a = src + dst * (1 - src)`, scaled by `alpha`.
    fn composite(&mut self, draw: impl FnOnce(&mut Pixmap, Transform)) {
        let (sw, sh) = (self.surface_width(), self.surface_height());
        let Some(mut pm) = Pixmap::new(sw.max(1), sh.max(1)) else { return };
        draw(&mut pm, Transform::from_translate(self.pad_x as f32, self.pad_y as f32));
        for (dst, px) in self.buf.iter_mut().zip(pm.pixels()) {
            let src = px.alpha() as u32;
            if src == 0 {
                continue;
            }
            let d = *dst as u32;
            *dst = (src + d * (255 - src) / 255).min(255) as u8;
        }
    }

    fn paint(alpha: u8) -> Paint<'static> {
        let mut paint = Paint::default();
        paint.set_color_rgba8(255, 255, 255, alpha);
        paint.anti_alias = true;
        paint
    }

    /// Fill a path (non-zero rule), antialiased.
    pub fn fill_path(&mut self, path: &Path, alpha: u8) {
        self.composite(|pm, t| pm.fill_path(path, &Self::paint(alpha), FillRule::Winding, t, None));
    }

    /// Stroke a path, antialiased.
    pub fn stroke_path(&mut self, path: &Path, width: f32, cap: LineCap, alpha: u8) {
        let stroke = Stroke { width, line_cap: cap, ..Stroke::default() };
        self.composite(|pm, t| pm.stroke_path(path, &Self::paint(alpha), &stroke, t, None));
    }

    /// A straight line with butt caps.
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, alpha: u8) {
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        if let Some(path) = pb.finish() {
            self.stroke_path(&path, width, LineCap::Butt, alpha);
        }
    }

    /// A filled triangle.
    pub fn triangle(&mut self, p: [(f32, f32); 3], alpha: u8) {
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(p[0].0, p[0].1);
        pb.line_to(p[1].0, p[1].1);
        pb.line_to(p[2].0, p[2].1);
        pb.close();
        if let Some(path) = pb.finish() {
            self.fill_path(&path, alpha);
        }
    }

    /// Invert every pixel of the cell area (padding untouched).
    pub fn invert(&mut self) {
        for y in 0..self.height as i32 {
            for x in 0..self.width as i32 {
                let v = self.get(x, y);
                self.pixel(x, y, 255 - v);
            }
        }
    }

    /// Mirror the whole surface left-right (padding is symmetric).
    pub fn flip_h(&mut self) {
        let w = self.surface_width() as usize;
        for row in self.buf.chunks_mut(w) {
            row.reverse();
        }
    }

    /// Mirror the whole surface top-bottom.
    pub fn flip_v(&mut self) {
        let w = self.surface_width() as usize;
        let h = self.surface_height() as usize;
        for y in 0..h / 2 {
            let (a, b) = self.buf.split_at_mut((h - 1 - y) * w);
            a[y * w..y * w + w].swap_with_slice(&mut b[..w]);
        }
    }

    /// Clear everything outside the cell area, for glyphs that must not
    /// spill (inverted ones, circles).
    pub fn crop_to_cell(&mut self) {
        let (w, h) = (self.width as i32, self.height as i32);
        let (px, py) = (self.pad_x as i32, self.pad_y as i32);
        for y in -py..h + py {
            for x in -px..w + px {
                if x < 0 || y < 0 || x >= w || y >= h {
                    self.pixel(x, y, 0);
                }
            }
        }
    }
}

/// A drawn sprite trimmed to its pixels, with Ghostty's bearings.
pub struct Trimmed {
    pub width: u32,
    pub height: u32,
    /// Glyph left minus cell left.
    pub offset_x: i32,
    /// Cell bottom up to glyph top.
    pub offset_y: i32,
    pub data: Vec<u8>,
}

/// Whether `cp` is drawn here rather than taken from a font.
pub fn covers(cp: u32) -> bool {
    matches!(cp,
        0x2500..=0x259F
        | 0x25E2..=0x25E5 | 0x25F8..=0x25FA | 0x25FF
        | 0x2800..=0x28FF
        | 0xE0B0..=0xE0BF | 0xE0D2 | 0xE0D4
        | 0x1CC1B..=0x1CC1E | 0x1CC21..=0x1CC2F | 0x1CC30..=0x1CC3F
        | 0x1CD00..=0x1CDE5 | 0x1CE00..=0x1CE01 | 0x1CE0B..=0x1CE0C | 0x1CE16..=0x1CE19
        | 0x1CE51..=0x1CEAF
        | 0x1FB00..=0x1FBAF | 0x1FBBD..=0x1FBBF | 0x1FBCE..=0x1FBCF | 0x1FBD0..=0x1FBEF
        | UNDERLINE..=CURSOR_UNDERLINE)
}

/// Draw `cp` for a grid with metrics `m`, `grid_width` cells wide.
/// `None` when nothing here draws it, or it came out empty.
pub fn render(cp: u32, grid_width: u32, m: &Metrics) -> Option<Trimmed> {
    let width = m.cell_width * grid_width.max(1);
    let height = match cp {
        CURSOR_RECT | CURSOR_HOLLOW_RECT | CURSOR_BAR => m.cursor_height,
        _ => m.cell_height,
    };
    let mut c = Canvas::new(width, height);
    let drawn = r#box::draw(cp, &mut c, width, height, m)
        || block::draw(cp, &mut c, width, height, m)
        || powerline::draw(cp, &mut c, width, height, m)
        || special::draw(cp, &mut c, width, height, m);
    if !drawn {
        return None;
    }
    trim(&c, m, height)
}

fn trim(c: &Canvas, m: &Metrics, height: u32) -> Option<Trimmed> {
    let (sw, sh) = (c.surface_width() as usize, c.surface_height() as usize);
    let (mut x0, mut y0, mut x1, mut y1) = (sw, sh, 0usize, 0usize);
    for y in 0..sh {
        for x in 0..sw {
            if c.buf[y * sw + x] != 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = (x1 - x0, y1 - y0);
    let mut data = Vec::with_capacity(w * h);
    for y in y0..y1 {
        data.extend_from_slice(&c.buf[y * sw + x0..y * sw + x1]);
    }
    let offset_x = x0 as i32 - c.pad_x as i32;
    // Cell bottom to glyph top; a short cursor is centred vertically.
    let top_in_cell = y0 as i32 - c.pad_y as i32 - (m.cell_height as i32 - height as i32) / 2;
    let offset_y = m.cell_height as i32 - top_in_cell;
    Some(Trimmed { width: w as u32, height: h as u32, offset_x, offset_y, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions_mirror() {
        // A 7px cell: both halves are 4px, overlapping by one.
        assert_eq!((frac_min(0.0, 7), frac_max(0.5, 7)), (0, 4));
        assert_eq!((frac_min(0.5, 7), frac_max(1.0, 7)), (3, 7));
    }

    #[test]
    fn canvas_keeps_padding_and_drops_beyond() {
        let mut c = Canvas::new(8, 16);
        c.rect(-2, -4, 12, 24, ON);
        c.rect(-10, 0, 3, 1, ON);
        assert_eq!(c.get(-2, -4), ON);
        assert_eq!(c.get(9, 19), ON);
        assert!(c.buf.iter().all(|&v| v == ON), "the rect covers exactly the padded surface");
    }

    #[test]
    fn flips_round_trip() {
        let mut c = Canvas::new(8, 16);
        c.pixel(0, 0, 7);
        c.flip_h();
        c.flip_v();
        c.flip_h();
        c.flip_v();
        assert_eq!(c.get(0, 0), 7);
    }
}
