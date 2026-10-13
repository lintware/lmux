//! One frame of a terminal pane for the GPU: a background colour per cell
//! and one instance per glyph, decoration and cursor, built the way
//! Ghostty's renderer builds them (`src/renderer/generic.zig` `rebuildRow`,
//! `addCursor`; MIT).
//!
//! Instance order is draw order: a block cursor first (text draws over it),
//! then per cell underline, glyph(s), strikethrough, then any other cursor
//! style last.

use bytemuck::{Pod, Zeroable};

pub use super::config::{BoldColor, CellColor};
use super::config::TermConfig;
use super::constraint::{constraint_width, is_graphics_element};
use super::glyphs::{emoji_presentation, Fonts, Glyph, Presentation, Style};
use super::sprite;
use crate::term::{Cell, Color, CursorShape, Flags, Grid, TermView, Underline, Wide};

/// One glyph quad, as `cell.wgsl`'s `TextIn` reads it (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct CellText {
    pub glyph_pos: [u32; 2],
    pub glyph_size: [u32; 2],
    pub bearings: [i16; 2],
    pub grid_pos: [u16; 2],
    pub color: [u8; 4],
    /// atlas (0 grayscale, 1 colour), flags, 0, 0.
    pub meta: [u8; 4],
}

const NO_MIN_CONTRAST: u8 = 1;
const IS_CURSOR_GLYPH: u8 = 2;

/// Colours the pane draws with: the user's Ghostty theme, or lmux's.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub palette: [[u8; 3]; 256],
    pub cursor_color: Option<CellColor>,
    pub cursor_text: Option<CellColor>,
    pub cursor_opacity: f32,
    pub selection_background: Option<CellColor>,
    pub selection_foreground: Option<CellColor>,
    pub bold_color: Option<BoldColor>,
    pub faint_opacity: f32,
    pub minimum_contrast: f32,
}

impl From<&TermConfig> for Theme {
    fn from(c: &TermConfig) -> Theme {
        Theme {
            background: c.background,
            foreground: c.foreground,
            palette: c.palette,
            cursor_color: c.cursor_color,
            cursor_text: c.cursor_text,
            cursor_opacity: c.cursor_opacity,
            selection_background: c.selection_background,
            selection_foreground: c.selection_foreground,
            bold_color: c.bold_color,
            faint_opacity: c.faint_opacity,
            minimum_contrast: c.minimum_contrast,
        }
    }
}

/// Everything the shaders need for one pane.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    /// RGBA8 per cell, row-major; alpha 0 for the default background.
    pub bg_cells: Vec<u32>,
    pub text: Vec<CellText>,
    /// Block cursor cell, `x | y << 16`, or 0xFFFF_FFFF.
    pub cursor_pos: u32,
    pub cursor_color: u32,
    pub cursor_wide: bool,
    pub bg_color: u32,
    pub min_contrast: f32,
}

pub fn pack(c: [u8; 3], a: u8) -> u32 {
    u32::from_le_bytes([c[0], c[1], c[2], a])
}

/// A cell's raw colours resolved against OSC overrides and the theme.
struct Palette<'a> {
    theme: &'a Theme,
    grid: &'a Grid,
}

impl Palette<'_> {
    fn index(&self, i: usize) -> [u8; 3] {
        self.grid.palette_override(i as u8).unwrap_or(self.theme.palette[i.min(255)])
    }
    fn fg(&self) -> [u8; 3] {
        self.grid.fg_override.unwrap_or(self.theme.foreground)
    }
    fn bg(&self) -> [u8; 3] {
        self.grid.bg_override.unwrap_or(self.theme.background)
    }

    /// Ghostty's `style.fg`: bold brightens only when `bold-color` is set.
    fn foreground(&self, c: Color, bold: bool) -> [u8; 3] {
        match c {
            Color::Rgb(c) => match (bold, self.theme.bold_color) {
                (true, Some(BoldColor::Color(b))) if c == self.fg() => b,
                _ => c,
            },
            Color::Index(i) => match (bold, self.theme.bold_color) {
                (true, Some(_)) if i < 8 => self.index(i as usize + 8),
                _ => self.index(i as usize),
            },
            Color::Default => match (bold, self.theme.bold_color) {
                (true, Some(BoldColor::Color(b))) => b,
                _ => self.fg(),
            },
        }
    }

    /// The background, or `None` for the default one.
    fn background(&self, c: Color) -> Option<[u8; 3]> {
        match c {
            Color::Default => None,
            Color::Rgb(c) => Some(c),
            Color::Index(i) => Some(self.index(i as usize)),
        }
    }
}

fn cell_color(c: Option<CellColor>, fg: [u8; 3], bg: [u8; 3], default: [u8; 3]) -> [u8; 3] {
    match c {
        Some(CellColor::Color(c)) => c,
        Some(CellColor::CellForeground) => fg,
        Some(CellColor::CellBackground) => bg,
        None => default,
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum CursorStyle {
    Block,
    Hollow,
    Bar,
    Underline,
}

/// The pane's state that is not in the terminal itself.
pub struct View {
    pub cols: u16,
    pub rows: u16,
    pub focused: bool,
    /// A link under the pointer with Cmd held: `(row, start col, end col)`.
    pub hover_link: Option<(usize, usize, usize)>,
}

pub fn build(tv: &TermView, fonts: &mut Fonts, theme: &Theme, view: &View) -> Frame {
    let grid = tv.grid();
    let grid: &Grid = &grid;
    let (cols, rows) = (view.cols as usize, view.rows as usize);
    let pal = Palette { theme, grid };
    let default_fg = pal.fg();
    let default_bg = pal.bg();

    let blank = Cell::default();
    // The terminal's cell at a pane position; cells beyond a grid smaller
    // than the pane (another viewer sized it) are blank.
    let at = |row: usize, col: usize| grid.cell(row, col).unwrap_or(&blank);

    let mut frame = Frame {
        cols: view.cols,
        rows: view.rows,
        bg_cells: vec![0; cols * rows],
        text: Vec::with_capacity(cols * rows / 2),
        cursor_pos: u32::MAX,
        cursor_color: 0,
        cursor_wide: false,
        bg_color: pack(default_bg, 255),
        min_contrast: theme.minimum_contrast.max(1.0),
    };

    // The cursor: where, which shape, wide or not.
    let cursor = grid
        .cursor
        .visible
        .then_some(grid.cursor.pos)
        .flatten()
        .filter(|&(row, col)| row < rows && col < cols)
        .map(|(row, mut col)| {
            if col > 0 && at(row, col).wide == Wide::Tail {
                col -= 1;
            }
            let wide = at(row, col).wide == Wide::Head;
            let style = if !view.focused {
                CursorStyle::Hollow
            } else {
                match grid.cursor.shape {
                    CursorShape::Beam => CursorStyle::Bar,
                    CursorShape::Underline => CursorStyle::Underline,
                    CursorShape::Hollow => CursorStyle::Hollow,
                    CursorShape::Block => CursorStyle::Block,
                }
            };
            (row, col, wide, style)
        });

    let mut late: Vec<CellText> = Vec::new();
    if let Some((row, col, wide, style)) = cursor {
        let c = at(row, col);
        let (cfg, cbg) = (pal.foreground(c.fg, c.flags.contains(Flags::BOLD)), pal.background(c.bg).unwrap_or(default_bg));
        let (cfg, cbg) = if c.flags.contains(Flags::INVERSE) { (cbg, cfg) } else { (cfg, cbg) };
        let color = grid.cursor_override.unwrap_or_else(|| cell_color(theme.cursor_color, cfg, cbg, default_fg));
        let alpha = if view.focused { (255.0 * theme.cursor_opacity).ceil().clamp(0.0, 255.0) as u8 } else { 255 };
        let sprite_cp = match style {
            CursorStyle::Block => sprite::CURSOR_RECT,
            CursorStyle::Hollow => sprite::CURSOR_HOLLOW_RECT,
            CursorStyle::Bar => sprite::CURSOR_BAR,
            CursorStyle::Underline => sprite::CURSOR_UNDERLINE,
        };
        if let Some(g) = fonts.sprite(sprite_cp, if wide { 2 } else { 1 }) {
            let inst = instance(&g, col, row, color, alpha, IS_CURSOR_GLYPH);
            if style == CursorStyle::Block {
                frame.text.push(inst);
                frame.cursor_pos = col as u32 | (row as u32) << 16;
                frame.cursor_wide = wide;
                frame.cursor_color = pack(cell_color(theme.cursor_text, cfg, cbg, default_bg), 255);
            } else {
                late.push(inst);
            }
        }
    }

    let faint_alpha = (theme.faint_opacity * 255.0).ceil().clamp(0.0, 255.0) as u8;
    let mut chars: Vec<char> = Vec::with_capacity(cols);
    for row in 0..rows {
        chars.clear();
        chars.extend((0..cols).map(|col| {
            let c = at(row, col);
            if c.is_spacer() {
                ' '
            } else {
                c.ch
            }
        }));
        for col in 0..cols {
            let c = at(row, col);
            let f = c.flags;
            let bold = f.contains(Flags::BOLD);
            let mut fg = pal.foreground(c.fg, bold);
            let mut bg = pal.background(c.bg);
            let inverse = f.contains(Flags::INVERSE);
            if inverse {
                let old_bg = bg.unwrap_or(default_bg);
                bg = Some(fg);
                fg = old_bg;
            }
            // A full block covers its cell: its background is its colour.
            if c.ch == '\u{2588}' && !inverse {
                bg = Some(fg);
            }
            if grid.selected(row, col) {
                let (cell_fg, cell_bg) = (fg, bg.unwrap_or(default_bg));
                bg = Some(cell_color(theme.selection_background, cell_fg, cell_bg, default_fg));
                fg = cell_color(theme.selection_foreground, cell_fg, cell_bg, default_bg);
            }
            if let Some(bg) = bg {
                frame.bg_cells[row * cols + col] = pack(bg, 255);
            }
            if f.contains(Flags::INVISIBLE) || c.is_spacer() {
                continue;
            }
            let alpha = if f.contains(Flags::FAINT) { faint_alpha } else { 255 };
            let gw = if c.wide == Wide::Head { 2 } else { 1 };

            // Underline first, so it sits under the text.
            let link = view.hover_link.is_some_and(|(r, s, e)| r == row && col >= s && col < e);
            let underline = match c.underline {
                Underline::Curly => Some(sprite::UNDERLINE_CURLY),
                Underline::Double => Some(sprite::UNDERLINE_DOUBLE),
                Underline::Single if link => Some(sprite::UNDERLINE_DOUBLE),
                Underline::Dotted => Some(sprite::UNDERLINE_DOTTED),
                Underline::Dashed => Some(sprite::UNDERLINE_DASHED),
                Underline::Single => Some(sprite::UNDERLINE),
                Underline::None if link => Some(sprite::UNDERLINE),
                Underline::None => None,
            };
            if let Some(cp) = underline {
                let ucolor = match c.underline_color {
                    Color::Default => fg,
                    u => pal.foreground(u, false),
                };
                if let Some(g) = fonts.sprite(cp, gw) {
                    frame.text.push(instance(&g, col, row, ucolor, alpha, 0));
                }
            }

            if c.ch != ' ' && c.ch != '\0' {
                let style = match (bold, f.contains(Flags::ITALIC)) {
                    (false, false) => Style::Regular,
                    (true, false) => Style::Bold,
                    (false, true) => Style::Italic,
                    (true, true) => Style::BoldItalic,
                };
                let presentation = if c.extra.contains(&'\u{FE0F}') {
                    Presentation::Emoji
                } else if c.extra.contains(&'\u{FE0E}') {
                    Presentation::Text
                } else if emoji_presentation(c.ch as u32) {
                    Presentation::Emoji
                } else {
                    Presentation::Text
                };
                let cw = constraint_width(&chars, col, gw);
                let flags = if is_graphics_element(c.ch as u32) { NO_MIN_CONTRAST } else { 0 };
                if let Some(g) = fonts.glyph(c.ch, style, presentation, gw, cw) {
                    frame.text.push(instance(&g, col, row, fg, alpha, flags));
                }
                // Combining marks draw over their base, at the same pen position.
                for z in c.overlay_marks() {
                    if let Some(g) = fonts.glyph(z, style, Presentation::Text, 1, 1) {
                        frame.text.push(instance(&g, col, row, fg, alpha, flags));
                    }
                }
            }

            if f.contains(Flags::STRIKE) {
                if let Some(g) = fonts.sprite(sprite::STRIKETHROUGH, gw) {
                    frame.text.push(instance(&g, col, row, fg, alpha, 0));
                }
            }
            if f.contains(Flags::OVERLINE) {
                if let Some(g) = fonts.sprite(sprite::OVERLINE, gw) {
                    frame.text.push(instance(&g, col, row, fg, alpha, 0));
                }
            }
        }
    }
    frame.text.extend(late);
    frame
}

fn instance(g: &Glyph, col: usize, row: usize, color: [u8; 3], alpha: u8, flags: u8) -> CellText {
    CellText {
        glyph_pos: [g.atlas_x, g.atlas_y],
        glyph_size: [g.width, g.height],
        bearings: [g.offset_x as i16, g.offset_y as i16],
        grid_pos: [col as u16, row as u16],
        color: [color[0], color[1], color[2], alpha],
        meta: [g.color as u8, flags, 0, 0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::glyphs::FontConfig;

    pub fn theme() -> Theme {
        let mut palette = [[0u8; 3]; 256];
        for (i, p) in palette.iter_mut().enumerate() {
            *p = [i as u8, 0, 0];
        }
        Theme {
            background: [0x14, 0x14, 0x14],
            foreground: [0xD2, 0xD2, 0xD2],
            palette,
            cursor_color: None,
            cursor_text: None,
            cursor_opacity: 1.0,
            selection_background: None,
            selection_foreground: None,
            bold_color: None,
            faint_opacity: 0.5,
            minimum_contrast: 1.0,
        }
    }

    fn fonts() -> Fonts {
        Fonts::new(FontConfig { families: Default::default(), points: 13.0, scale: 2.0, thicken: false, adjust_cell_height: None })
    }

    fn view(cols: u16, rows: u16) -> View {
        View { cols, rows, focused: true, hover_link: None }
    }

    #[test]
    fn plain_text_is_one_instance_per_glyph_after_the_block_cursor() {
        let mut t = TermView::new(10, 2);
        t.feed(b"ab");
        let f = build(&t, &mut fonts(), &theme(), &view(10, 2));
        // Block cursor first, then a and b.
        assert_eq!(f.text.len(), 3);
        assert_eq!(f.text[0].meta[1] & IS_CURSOR_GLYPH, IS_CURSOR_GLYPH);
        assert_eq!(f.text[1].grid_pos, [0, 0]);
        assert_eq!(f.text[2].grid_pos, [1, 0]);
        assert_eq!(f.cursor_pos, 2);
        assert!(f.bg_cells.iter().all(|&c| c == 0), "default backgrounds are transparent");
    }

    #[test]
    fn colours_come_from_the_palette_and_sgr() {
        let mut t = TermView::new(10, 2);
        t.feed(b"\x1b[31;42mx\x1b[0m\x1b[38;2;1;2;3my");
        let f = build(&t, &mut fonts(), &theme(), &view(10, 2));
        let x = f.text.iter().find(|c| c.grid_pos == [0, 0] && c.meta[1] == 0).unwrap();
        assert_eq!(&x.color[..3], &[1, 0, 0]);
        assert_eq!(f.bg_cells[0], pack([2, 0, 0], 255));
        let y = f.text.iter().find(|c| c.grid_pos == [1, 0] && c.meta[1] == 0).unwrap();
        assert_eq!(&y.color[..3], &[1, 2, 3]);
    }

    #[test]
    fn bold_does_not_brighten_by_default_and_faint_is_translucent() {
        let mut t = TermView::new(10, 2);
        t.feed(b"\x1b[1;31mx\x1b[0m\x1b[2my");
        let f = build(&t, &mut fonts(), &theme(), &view(10, 2));
        let x = f.text.iter().find(|c| c.grid_pos == [0, 0] && c.meta[1] == 0).unwrap();
        assert_eq!(&x.color[..3], &[1, 0, 0]);
        let y = f.text.iter().find(|c| c.grid_pos == [1, 0] && c.meta[1] == 0).unwrap();
        assert_eq!(y.color[3], 128);
    }

    #[test]
    fn unfocused_cursor_is_hollow_and_drawn_last() {
        let mut t = TermView::new(10, 2);
        t.feed(b"ab");
        let mut v = view(10, 2);
        v.focused = false;
        let f = build(&t, &mut fonts(), &theme(), &v);
        assert_eq!(f.cursor_pos, u32::MAX, "no text recolouring without a block cursor");
        assert_eq!(f.text.last().unwrap().meta[1] & IS_CURSOR_GLYPH, IS_CURSOR_GLYPH);
    }

    #[test]
    fn decorations_wrap_the_glyph() {
        let mut t = TermView::new(10, 2);
        t.feed(b"\x1b[4;9mx");
        let f = build(&t, &mut fonts(), &theme(), &view(10, 2));
        let at: Vec<_> = f.text.iter().filter(|c| c.grid_pos == [0, 0] && c.meta[1] == 0).collect();
        assert_eq!(at.len(), 3, "underline, glyph, strikethrough");
    }

    #[test]
    fn underline_styles_pick_their_sprites_and_colour() {
        let mut f = fonts();
        let mut t = TermView::new(10, 2);
        t.feed(b"\x1b[4:3;58;2;9;8;7mc\x1b[0m\x1b[4:4md\x1b[0m\x1b[4:5ma\x1b[0m\x1b[4:2mw\x1b[0m\x1b[53;9mo");
        let frame = build(&t, &mut f, &theme(), &view(10, 2));
        let mut sprite_at = |col: u16, cp: u32| {
            let g = f.sprite(cp, 1).unwrap();
            frame.text.iter().find(|c| c.grid_pos == [col, 0] && c.glyph_pos == [g.atlas_x, g.atlas_y])
        };
        assert_eq!(&sprite_at(0, sprite::UNDERLINE_CURLY).expect("curly").color[..3], &[9, 8, 7]);
        assert!(sprite_at(1, sprite::UNDERLINE_DOTTED).is_some());
        assert!(sprite_at(2, sprite::UNDERLINE_DASHED).is_some());
        assert!(sprite_at(3, sprite::UNDERLINE_DOUBLE).is_some());
        assert!(sprite_at(4, sprite::OVERLINE).is_some());
        assert!(sprite_at(4, sprite::STRIKETHROUGH).is_some());
    }

    #[test]
    fn an_emoji_cluster_is_one_wide_glyph() {
        let mut t = TermView::new(10, 2);
        t.feed("\u{1F44D}\u{1F3FD}x".as_bytes());
        let f = build(&t, &mut fonts(), &theme(), &view(10, 2));
        let glyphs: Vec<_> = f.text.iter().filter(|c| c.meta[1] & IS_CURSOR_GLYPH == 0).map(|c| c.grid_pos).collect();
        assert_eq!(glyphs, vec![[0, 0], [2, 0]], "the skin tone draws no second glyph");
    }

    #[test]
    fn wide_characters_emit_once_at_their_head() {
        let mut t = TermView::new(10, 2);
        t.feed("\u{6F22}x".as_bytes());
        let f = build(&t, &mut fonts(), &theme(), &view(10, 2));
        let glyphs: Vec<_> = f.text.iter().filter(|c| c.meta[1] & IS_CURSOR_GLYPH == 0).map(|c| c.grid_pos).collect();
        assert_eq!(glyphs, vec![[0, 0], [2, 0]]);
    }

    #[test]
    fn selection_inverts_to_the_default_colours() {
        let mut t = TermView::new(10, 2);
        t.feed(b"hello");
        t.start_selection(crate::term::SelectionType::Simple, 0, 0, false);
        t.update_selection(0, 1, true);
        let th = theme();
        let f = build(&t, &mut fonts(), &th, &view(10, 2));
        assert_eq!(f.bg_cells[0], pack(th.foreground, 255));
        let h = f.text.iter().find(|c| c.grid_pos == [0, 0] && c.meta[1] == 0).unwrap();
        assert_eq!(&h.color[..3], &th.background);
    }
}
