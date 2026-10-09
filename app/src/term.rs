//! Terminal state: an `alacritty_terminal` VT parser + grid per surface.
//!
//! We never hand-write the emulator; we own a `Term`, feed it bytes from the
//! daemon, and paint its grid ourselves with egui's painter (see `ui::terminal`).

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, RenderableCursor, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

/// Grid geometry handed to `Term`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

impl TermSize {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self { cols: cols.max(2), rows: rows.max(1) }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct TermView {
    term: Term<VoidListener>,
    processor: Processor,
    size: TermSize,
    /// Wheel remainder: points that did not add up to a whole line yet, so a
    /// trackpad's small deltas still scroll instead of rounding away to zero.
    scroll_accum: f32,
    /// Wanted by the UI as visible (drives subscribe/unsubscribe).
    pub wants_visible: bool,
    /// Currently subscribed on the daemon.
    pub subscribed: bool,
    /// True once a `term_snapshot` has been written.
    pub have_snapshot: bool,
    /// The surface reported exit.
    pub exited: bool,
}

impl TermView {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = TermSize::new(cols as usize, rows as usize);
        Self {
            term: new_term(size),
            processor: Processor::new(),
            size,
            scroll_accum: 0.0,
            wants_visible: false,
            subscribed: false,
            have_snapshot: false,
            exited: false,
        }
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    pub fn cols(&self) -> u16 {
        self.size.cols as u16
    }

    pub fn rows(&self) -> u16 {
        self.size.rows as u16
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let size = TermSize::new(cols as usize, rows as usize);
        if size == self.size {
            return false;
        }
        self.term.resize(size);
        self.size = size;
        true
    }

    /// Replace the grid with a fresh terminal of the snapshot's size, then play
    /// the ANSI text into it (the daemon guarantees this reproduces the screen).
    pub fn apply_snapshot(&mut self, cols: u16, rows: u16, data: &str) {
        let size = TermSize::new(cols as usize, rows as usize);
        self.term = new_term(size);
        self.processor = Processor::new();
        self.size = size;
        self.feed(data.as_bytes());
        self.have_snapshot = true;
    }

    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    /// Turn a raw wheel delta in points into whole lines, keeping the remainder
    /// for the next frame. Positive means the content moved down, i.e. the user
    /// is scrolling back into history, which is `Scroll::Delta`'s positive
    /// direction too.
    pub fn accumulate_scroll(&mut self, points: f32, line_height: f32) -> i32 {
        if line_height <= 0.0 {
            return 0;
        }
        self.scroll_accum += points / line_height;
        let whole = self.scroll_accum.trunc();
        self.scroll_accum -= whole;
        whole as i32
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn mode(&self) -> alacritty_terminal::term::TermMode {
        *self.term.mode()
    }

    pub fn cursor(&self) -> RenderableCursor {
        self.term.renderable_content().cursor
    }

    pub fn term(&self) -> &Term<VoidListener> {
        &self.term
    }
}

fn new_term(size: TermSize) -> Term<VoidListener> {
    let config = Config { scrolling_history: 10_000, ..Config::default() };
    Term::new(config, &size, VoidListener)
}

// --- colours ---------------------------------------------------------------

/// A compact dark palette modelled on the reference screenshot / macOS Terminal.
pub const PALETTE: [[u8; 3]; 16] = [
    [0x1C, 0x1C, 0x1E], // black
    [0xFF, 0x45, 0x3A], // red
    [0x32, 0xD7, 0x4B], // green
    [0xFF, 0xD6, 0x0A], // yellow
    [0x0A, 0x84, 0xFF], // blue
    [0xBF, 0x5A, 0xF2], // magenta
    [0x64, 0xD2, 0xFF], // cyan
    [0xD8, 0xD8, 0xDC], // white
    [0x6A, 0x6A, 0x70], // bright black
    [0xFF, 0x69, 0x61], // bright red
    [0x5B, 0xE8, 0x6E], // bright green
    [0xFF, 0xE4, 0x5C], // bright yellow
    [0x4F, 0xA6, 0xFF], // bright blue
    [0xD0, 0x8C, 0xFF], // bright magenta
    [0x8A, 0xE6, 0xFF], // bright cyan
    [0xFF, 0xFF, 0xFF], // bright white
];

fn cube_idx(v: u8) -> u8 {
    if v == 0 {
        0
    } else {
        55 + v * 40
    }
}

fn indexed_rgb(i: u8) -> Rgb {
    match i {
        0..=15 => {
            let c = PALETTE[i as usize];
            Rgb { r: c[0], g: c[1], b: c[2] }
        }
        16..=231 => {
            let i = i - 16;
            let r = cube_idx(i / 36);
            let g = cube_idx((i % 36) / 6);
            let b = cube_idx(i % 6);
            Rgb { r, g, b }
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            Rgb { r: v, g: v, b: v }
        }
    }
}

/// Resolve a cell colour to an RGB triple, given the surface's default fg/bg.
pub fn resolve(color: Color, default_fg: Rgb, default_bg: Rgb) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Indexed(i) => indexed_rgb(i),
        Color::Named(named) => match named {
            NamedColor::Foreground | NamedColor::BrightForeground => default_fg,
            NamedColor::Background => default_bg,
            NamedColor::DimForeground => dim_rgb(default_fg),
            NamedColor::Cursor => default_fg,
            NamedColor::DimBlack => dim(PALETTE[0]),
            NamedColor::DimRed => dim(PALETTE[1]),
            NamedColor::DimGreen => dim(PALETTE[2]),
            NamedColor::DimYellow => dim(PALETTE[3]),
            NamedColor::DimBlue => dim(PALETTE[4]),
            NamedColor::DimMagenta => dim(PALETTE[5]),
            NamedColor::DimCyan => dim(PALETTE[6]),
            NamedColor::DimWhite => dim(PALETTE[7]),
            other => {
                let i = (other as usize).min(15);
                let c = PALETTE[i];
                Rgb { r: c[0], g: c[1], b: c[2] }
            }
        },
    }
}

fn dim(c: [u8; 3]) -> Rgb {
    Rgb { r: c[0] / 2, g: c[1] / 2, b: c[2] / 2 }
}

fn dim_rgb(c: Rgb) -> Rgb {
    Rgb { r: c.r / 2, g: c.g / 2, b: c.b / 2 }
}

/// Brighten a base ANSI colour, used for the BOLD flag.
fn brighten(rgb: Rgb) -> Rgb {
    for (i, c) in PALETTE.iter().enumerate().take(8) {
        if c == &[rgb.r, rgb.g, rgb.b] {
            let b = PALETTE[i + 8];
            return Rgb { r: b[0], g: b[1], b: b[2] };
        }
    }
    rgb
}

/// Viewport-relative row/column for a terminal point, or `None` if it is
/// scrolled off.
pub fn viewport_point(display_offset: usize, point: Point) -> Option<(usize, usize)> {
    let line = point.line.0 + display_offset as i32;
    if line < 0 {
        return None;
    }
    Some((line as usize, point.column.0))
}

/// Foreground/background for a cell after applying INVERSE/BOLD/DIM and the
/// default colour handling. Returns `(fg, bg)`.
pub fn cell_colors(
    fg: Color,
    bg: Color,
    flags: Flags,
    default_fg: Rgb,
    default_bg: Rgb,
) -> (Rgb, Rgb) {
    let mut fg = resolve(fg, default_fg, default_bg);
    let bg = resolve(bg, default_fg, default_bg);
    if flags.contains(Flags::BOLD) {
        fg = brighten(fg);
    }
    if flags.contains(Flags::DIM) {
        fg = Rgb { r: fg.r / 2, g: fg.g / 2, b: fg.b / 2 };
    }
    if flags.contains(Flags::INVERSE) {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

pub fn is_wide_spacer(flags: Flags) -> bool {
    flags.contains(Flags::WIDE_CHAR_SPACER) || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feeds_and_reads_grid() {
        let mut t = TermView::new(20, 4);
        t.feed(b"hello\r\nworld");
        let content = t.term().renderable_content();
        let text: String = content
            .display_iter
            .filter(|c| c.point.line.0 == 0)
            .map(|c| c.cell.c)
            .collect();
        assert!(text.starts_with("hello"));
    }

    #[test]
    fn snapshot_replaces_screen() {
        let mut t = TermView::new(20, 4);
        t.feed(b"old contents");
        t.apply_snapshot(20, 4, "\x1b[2J\x1b[Hfresh");
        let content = t.term().renderable_content();
        let line0: String = content
            .display_iter
            .filter(|c| c.point.line.0 == 0)
            .map(|c| c.cell.c)
            .collect();
        assert!(line0.starts_with("fresh"));
        assert!(t.have_snapshot);
    }

    #[test]
    fn resize_changes_dims() {
        let mut t = TermView::new(80, 24);
        assert!(t.resize(100, 30));
        assert_eq!(t.cols(), 100);
        assert_eq!(t.rows(), 30);
        assert!(!t.resize(100, 30));
    }

    #[test]
    fn indexed_palette_math() {
        assert_eq!(indexed_rgb(0).r, PALETTE[0][0]);
        assert_eq!(indexed_rgb(196), Rgb { r: 255, g: 0, b: 0 }); // bright red in cube
        assert_eq!(indexed_rgb(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(indexed_rgb(255), Rgb { r: 238, g: 238, b: 238 });
    }

    #[test]
    fn inverse_swaps() {
        let dfg = Rgb { r: 200, g: 200, b: 200 };
        let dbg = Rgb { r: 10, g: 10, b: 10 };
        let (fg, bg) = cell_colors(
            Color::Named(NamedColor::Foreground),
            Color::Named(NamedColor::Background),
            Flags::INVERSE,
            dfg,
            dbg,
        );
        assert_eq!(fg, dbg);
        assert_eq!(bg, dfg);
    }

    #[test]
    fn scrollback_accumulates() {
        let mut t = TermView::new(10, 2);
        for i in 0..10 {
            t.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(t.term().grid().history_size() > 0);
    }
}
