//! Fitting a glyph into its cell, ported from Ghostty's `src/font/Glyph.zig`
//! (MIT). Emoji are scaled to cover the cell and centred; symbols (PUA,
//! dingbats, arrows) shrink to fit and may take two cells when followed by a
//! space. Plain text is left exactly as the font draws it.

use super::metrics::Metrics;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Size {
    None,
    Fit,
    Cover,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Align {
    None,
    Center,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Constraint {
    pub size: Size,
    pub align_h: Align,
    pub align_v: Align,
    pub pad_left: f64,
    pub pad_right: f64,
    pub pad_top: f64,
    pub pad_bottom: f64,
    pub max_constraint_width: u8,
}

impl Constraint {
    pub const NONE: Constraint = Constraint {
        size: Size::None,
        align_h: Align::None,
        align_v: Align::None,
        pad_left: 0.0,
        pad_right: 0.0,
        pad_top: 0.0,
        pad_bottom: 0.0,
        max_constraint_width: 2,
    };
    pub const FIT: Constraint = Constraint { size: Size::Fit, ..Constraint::NONE };
    pub const EMOJI: Constraint = Constraint {
        size: Size::Cover,
        align_h: Align::Center,
        align_v: Align::Center,
        pad_left: 0.025,
        pad_right: 0.025,
        ..Constraint::NONE
    };

    pub fn does_anything(&self) -> bool {
        self.size != Size::None || self.align_h != Align::None || self.align_v != Align::None
    }

    /// A glyph box (x from the cell's left, y from the cell's bottom)
    /// constrained to the cell.
    pub fn constrain(&self, g: GlyphBox, m: &Metrics, constraint_width: u8) -> GlyphBox {
        if !self.does_anything() {
            return g;
        }
        let width_cells = self.max_constraint_width.min(constraint_width).max(1);
        let (wf, hf) = self.scale_factors(&g, m, width_cells);
        let cx = g.x + g.width / 2.0;
        let cy = g.y + g.height / 2.0;
        let mut out = GlyphBox { width: g.width * wf, height: g.height * hf, x: 0.0, y: 0.0 };
        out.x = cx - out.width / 2.0;
        out.y = cy - out.height / 2.0;
        out.y = self.aligned_y(&out, m);
        out.x = self.aligned_x(&out, m, width_cells);
        out
    }

    fn scale_factors(&self, g: &GlyphBox, m: &Metrics, width_cells: u8) -> (f64, f64) {
        if self.size == Size::None {
            return (1.0, 1.0);
        }
        let target_w = (width_cells as f64 - (self.pad_left + self.pad_right)) * m.face_width;
        let target_h = (1.0 - (self.pad_bottom + self.pad_top)) * m.face_height;
        let wf = target_w / g.width;
        let hf = target_h / g.height;
        let f = match self.size {
            Size::Fit => 1.0f64.min(wf).min(hf),
            Size::Cover => wf.min(hf),
            Size::None => 1.0,
        };
        (f, f)
    }

    fn aligned_y(&self, g: &GlyphBox, m: &Metrics) -> f64 {
        if self.size == Size::None && self.align_v == Align::None {
            return g.y;
        }
        let start = m.face_y + self.pad_bottom * m.face_height;
        let end = m.face_y + (m.face_height - g.height - self.pad_top * m.face_height);
        let center = (start + end) / 2.0;
        match self.align_v {
            Align::None if end < start => center,
            Align::None => start.max(g.y.min(end)),
            Align::Center => center,
        }
    }

    fn aligned_x(&self, g: &GlyphBox, m: &Metrics, width_cells: u8) -> f64 {
        if self.size == Size::None && self.align_h == Align::None {
            return g.x;
        }
        let span = m.face_width + ((width_cells as u32 - 1) * m.cell_width) as f64;
        let start = self.pad_left * m.face_width;
        let end = span - g.width - self.pad_right * m.face_width;
        match self.align_h {
            Align::None => start.max(g.x.min(end)),
            Align::Center => start.max((start + end) / 2.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphBox {
    pub width: f64,
    pub height: f64,
    pub x: f64,
    pub y: f64,
}

/// Codepoints Ghostty treats as symbols (`uucode` `is_symbol`): PUA,
/// arrows, dingbats, emoticons, miscellaneous symbols and pictographs.
pub fn is_symbol(cp: u32) -> bool {
    matches!(cp,
        0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD
        | 0x2190..=0x21FF
        | 0x2460..=0x24FF
        | 0x2600..=0x26FF
        | 0x2700..=0x27BF
        | 0x1F100..=0x1F1FF
        | 0x1F300..=0x1F5FF
        | 0x1F600..=0x1F64F
        | 0x1F680..=0x1F6FF)
}

/// Graphics characters that must not be recoloured for contrast and that
/// end a symbol run (box drawing, blocks, legacy computing, Powerline).
pub fn is_graphics_element(cp: u32) -> bool {
    matches!(cp, 0x2500..=0x257F | 0x2580..=0x259F | 0x1FB00..=0x1FBFF | 0x1CC00..=0x1CEBF | 0xE0B0..=0xE0D7)
}

/// How many cells a glyph may use: wide characters take two; a symbol
/// followed by a blank also takes two, unless it follows another symbol.
pub fn constraint_width(cells: &[char], x: usize, grid_width: u8) -> u8 {
    if grid_width > 1 {
        return grid_width;
    }
    let cp = cells[x] as u32;
    if !is_symbol(cp) || x + 1 >= cells.len() {
        return 1;
    }
    if x > 0 {
        let prev = cells[x - 1] as u32;
        if is_symbol(prev) && !is_graphics_element(prev) {
            return 1;
        }
    }
    match cells[x + 1] {
        ' ' | '\u{2002}' | '\0' => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::metrics::FaceMetrics;

    fn metrics() -> Metrics {
        Metrics::calc(&FaceMetrics { px_per_em: 20.0, cell_width: 12.0, ascent: 16.0, descent: -4.0, ..Default::default() })
    }

    #[test]
    fn emoji_covers_two_cells_and_centres() {
        let m = metrics();
        let g = GlyphBox { width: 40.0, height: 40.0, x: 0.0, y: -5.0 };
        let c = Constraint::EMOJI.constrain(g, &m, 2);
        // Height-limited: a square glyph becomes face_height tall.
        assert!((c.height - m.face_height).abs() < 1e-9);
        assert!((c.width - c.height).abs() < 1e-9);
        let span = m.face_width + m.cell_width as f64;
        assert!(((c.x + c.width / 2.0) - span / 2.0).abs() < 1e-9, "centred across both cells");
    }

    #[test]
    fn fit_never_enlarges() {
        let m = metrics();
        let g = GlyphBox { width: 4.0, height: 4.0, x: 2.0, y: 6.0 };
        let c = Constraint::FIT.constrain(g, &m, 1);
        assert_eq!((c.width, c.height), (4.0, 4.0));
    }

    #[test]
    fn symbol_before_space_gets_two_cells() {
        let row: Vec<char> = "\u{E0A0} x\u{E0A0}\u{E0A0} ".chars().collect();
        assert_eq!(constraint_width(&row, 0, 1), 2);
        assert_eq!(constraint_width(&row, 3, 1), 1, "followed by another symbol");
        assert_eq!(constraint_width(&row, 4, 1), 1, "follows a symbol");
        assert_eq!(constraint_width(&row, 2, 1), 1, "not a symbol");
    }
}
