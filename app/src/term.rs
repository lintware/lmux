//! Terminal state: a `libghostty-vt` (Ghostty's emulator) terminal per surface.
//!
//! This is the only module that talks to the emulator. The rest of the app
//! sees lmux's own types: `Grid` (the visible cells), `Cell`, `Modes`. We own
//! a `Terminal`, feed it bytes from the daemon, and paint its grid ourselves
//! (see `gfx::frame` and `ui::terminal`). Terminals are `!Send`: they live on
//! the UI thread.

use std::cell::{Ref, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use libghostty_vt::render::{CellIterator, CursorVisualStyle, Dirty, RenderState, RowIterator};
use libghostty_vt::screen::{CellContentTag, CellWide, Screen as ActiveScreen};
use libghostty_vt::selection::{FormatOptions, SelectLineOptions, SelectWordOptions, Selection};
use libghostty_vt::style::{Palette, PaletteIndex, RgbColor, StyleColor, Underline as GUnderline};
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode, Point, PointCoordinate,
    PointSpace, PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes, TertiaryDeviceAttributes,
};
use libghostty_vt::{fmt::Format, Error as GhosttyError, Terminal};

/// Cell size in pixels the emulator is told about (it only uses it for size
/// reports and image protocols, which this app does not answer).
const CELL_PX: (u32, u32) = (8, 16);

/// A program that holds the screen (synchronized output, mode 2026) and
/// never lets go is shown live after this long.
const HOLD_TIMEOUT: Duration = Duration::from_secs(1);

/// Grid geometry.
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

// --- cells -------------------------------------------------------------------

/// A cell colour as the program set it. Palette indices stay indices so the
/// painter can apply the user's theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Index(u8),
    Rgb([u8; 3]),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(u8);

impl Flags {
    pub const BOLD: Flags = Flags(1);
    pub const ITALIC: Flags = Flags(2);
    pub const FAINT: Flags = Flags(4);
    pub const INVERSE: Flags = Flags(8);
    pub const INVISIBLE: Flags = Flags(16);
    pub const STRIKE: Flags = Flags(32);
    pub const OVERLINE: Flags = Flags(64);

    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Flags) {
        self.0 |= rhs.0;
    }
}

/// How a cell takes part in a double-width character.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Wide {
    #[default]
    Narrow,
    /// The leading half: the glyph is drawn two cells wide from here.
    Head,
    /// The trailing half: no glyph.
    Tail,
    /// Padding at the end of a wrapped line where a wide character did not fit.
    SpacerHead,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// First code point of the grapheme cluster (`' '` for an empty cell).
    pub ch: char,
    /// The rest of the cluster: combining marks, variation selectors, ZWJ
    /// sequences, skin tones.
    pub extra: Vec<char>,
    pub fg: Color,
    pub bg: Color,
    pub underline: Underline,
    pub underline_color: Color,
    pub flags: Flags,
    pub wide: Wide,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            extra: Vec::new(),
            fg: Color::Default,
            bg: Color::Default,
            underline: Underline::None,
            underline_color: Color::Default,
            flags: Flags::default(),
            wide: Wide::Narrow,
        }
    }
}

impl Cell {
    /// Draws no glyph: the trailing half of a wide character or line-end padding.
    pub fn is_spacer(&self) -> bool {
        matches!(self.wide, Wide::Tail | Wide::SpacerHead)
    }

    /// The cluster's combining characters that draw over the base glyph.
    /// Variation selectors, joiners, skin tones and regional indicators only
    /// change how a cluster is shaped; there is no shaper here, so they draw
    /// nothing of their own and a ZWJ sequence shows its first emoji.
    pub fn overlay_marks(&self) -> impl Iterator<Item = char> + '_ {
        let joined = self.extra.contains(&'\u{200D}');
        self.extra.iter().copied().filter(move |&z| {
            !joined
                && !matches!(z, '\u{FE00}'..='\u{FE0F}' | '\u{200D}' | '\u{1F3FB}'..='\u{1F3FF}' | '\u{1F1E6}'..='\u{1F1FF}')
        })
    }

    /// The cell's text, the whole grapheme cluster.
    pub fn text(&self) -> String {
        std::iter::once(self.ch).chain(self.extra.iter().copied()).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Beam,
    Underline,
    Hollow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Viewport row and column; `None` when the cursor is scrolled out of view.
    pub pos: Option<(usize, usize)>,
    pub visible: bool,
    pub shape: CursorShape,
}

/// The visible cells of a terminal, plus what is needed to colour them.
#[derive(Clone, Debug)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    /// Row-major, `rows * cols`.
    pub cells: Vec<Cell>,
    pub cursor: Cursor,
    /// Selected columns (inclusive) per viewport row.
    selection: Vec<Option<(u16, u16)>>,
    /// Palette entries the program changed (OSC 4).
    palette: Box<[Option<[u8; 3]>; 256]>,
    /// Default foreground / background / cursor colours the program changed
    /// (OSC 10, 11, 12).
    pub fg_override: Option<[u8; 3]>,
    pub bg_override: Option<[u8; 3]>,
    pub cursor_override: Option<[u8; 3]>,
}

impl Grid {
    fn new(cols: usize, rows: usize) -> Self {
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            cursor: Cursor { pos: None, visible: true, shape: CursorShape::Block },
            selection: vec![None; rows],
            palette: Box::new([None; 256]),
            fg_override: None,
            bg_override: None,
            cursor_override: None,
        }
    }

    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        (row < self.rows && col < self.cols).then(|| &self.cells[row * self.cols + col])
    }

    pub fn row(&self, row: usize) -> &[Cell] {
        if row < self.rows {
            &self.cells[row * self.cols..(row + 1) * self.cols]
        } else {
            &[]
        }
    }

    pub fn selected(&self, row: usize, col: usize) -> bool {
        self.selection.get(row).copied().flatten().is_some_and(|(a, b)| (a as usize..=b as usize).contains(&col))
    }

    pub fn palette_override(&self, i: u8) -> Option<[u8; 3]> {
        self.palette[i as usize]
    }
}

/// Terminal modes the program set, as the input encoders need them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub bracketed_paste: bool,
    pub app_cursor: bool,
    pub focus_events: bool,
    /// Mouse reporting: press and release (modes 9, 1000).
    pub mouse_click: bool,
    /// Also motion while a button is held (1002).
    pub mouse_drag: bool,
    /// Also motion with no button (1003).
    pub mouse_motion: bool,
    pub sgr_mouse: bool,
    pub utf8_mouse: bool,
    pub alt_screen: bool,
    pub show_cursor: bool,
}

impl Modes {
    /// True when the program asked for mouse reports of any kind.
    pub fn mouse_reporting(&self) -> bool {
        self.mouse_click || self.mouse_drag || self.mouse_motion
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionType {
    Simple,
    Semantic,
    Lines,
}

// --- emulator plumbing ---------------------------------------------------------

/// What the terminal asks of the app, filled by its callbacks.
struct Shared {
    /// Bytes the terminal wants written back to the program (device
    /// attributes, cursor position and colour reports).
    pty: Vec<u8>,
    /// OSC 52 copies: a program (local, or on a remote host through tmux and
    /// the daemon) sets the clipboard, and the app puts the text on this
    /// machine's clipboard. OSC 52 *reads* stay denied, so no program can
    /// read it.
    clipboard: Vec<String>,
    title: Option<String>,
    bells: u32,
    /// Since when the program holds the screen (mode 2026).
    held: Option<Instant>,
    /// The terminal changed since `render.grid` was read.
    stale: bool,
    render: Render,
}

impl Shared {
    fn new(size: TermSize) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Shared {
            pty: Vec::new(),
            clipboard: Vec::new(),
            title: None,
            bells: 0,
            held: None,
            stale: true,
            render: Render::new(size),
        }))
    }
}

type Term = Terminal<'static, 'static>;

fn new_term(size: TermSize, shared: &Rc<RefCell<Shared>>) -> Term {
    let mut t = Terminal::new(size.cols as u16, size.rows as u16).expect("libghostty terminal");
    // A view's own history only needs to hold what scrolling back fetches
    // from tmux (which keeps 50,000 lines): a full 10,000-line history cost
    // 32-51 MB per view. Local panes are drawn by tmux on the alternate
    // screen and keep none.
    let _ = t.set_scrollback_max_lines(Some(crate::state::STREAMED_HISTORY as usize));
    // Grapheme clusters (emoji with skin tones, ZWJ sequences) take one glyph's width.
    let _ = t.set_default_mode(Mode::GRAPHEME_CLUSTER, true);
    let rgb = |c: egui::Color32| RgbColor { r: c.r(), g: c.g(), b: c.b() };
    let _ = t.set_default_fg_color(Some(rgb(crate::theme::TERM_FG)));
    let _ = t.set_default_bg_color(Some(rgb(crate::theme::TERM_BG)));
    let _ = t.set_default_color_palette(Some(default_palette()));

    let s = shared.clone();
    let _ = t.on_pty_write(move |_, data| s.borrow_mut().pty.extend_from_slice(data));
    let s = shared.clone();
    let _ = t.on_clipboard_write(move |_, req| {
        let text = req.contents().find(|c| c.mime.starts_with("text/plain")).map(|c| String::from_utf8_lossy(c.data).into_owned());
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            s.borrow_mut().clipboard.push(text);
        }
        req.reply(Ok(()), false);
    });
    let s = shared.clone();
    let _ = t.on_title_changed(move |t| s.borrow_mut().title = t.title().ok().map(str::to_owned));
    let s = shared.clone();
    let _ = t.on_bell(move |_| s.borrow_mut().bells += 1);
    // When a program starts holding the screen, capture the frame it wants
    // left up: the grid keeps showing it until the hold ends.
    let s = shared.clone();
    let _ = t.on_render_hold(move |t, held| {
        if let Ok(mut s) = s.try_borrow_mut() {
            if held {
                let _ = update_grid(t, &mut s.render);
                s.stale = false;
            }
            s.held = held.then(Instant::now);
        }
    });
    // Ghostty's own answers: VT220 level, ANSI colour.
    let _ = t.on_device_attributes(|_| {
        Some(DeviceAttributes {
            primary: PrimaryDeviceAttributes::new(ConformanceLevel::LEVEL_2, &[DeviceAttributeFeature::ANSI_COLOR]),
            secondary: SecondaryDeviceAttributes { device_type: DeviceType::VT220, firmware_version: 10, rom_cartridge: 0 },
            tertiary: TertiaryDeviceAttributes { unit_id: 0 },
        })
    });
    t
}

fn default_palette() -> Palette {
    let mut p = Palette::default();
    for i in 0..=255u8 {
        let c = indexed_rgb(i);
        p.set(PaletteIndex(i), RgbColor { r: c[0], g: c[1], b: c[2] });
    }
    p
}

/// The terminal's render state and the grid last read from it.
struct Render {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    grid: Grid,
}

impl Render {
    fn new(size: TermSize) -> Self {
        Render {
            state: RenderState::new().expect("render state"),
            rows: RowIterator::new().expect("row iterator"),
            cells: CellIterator::new().expect("cell iterator"),
            grid: Grid::new(size.cols, size.rows),
        }
    }
}

pub struct TermView {
    term: Term,
    shared: Rc<RefCell<Shared>>,
    size: TermSize,
    selection: Option<Sel>,
    /// Wheel remainder: points that did not add up to a whole line yet, so a
    /// trackpad's small deltas still scroll instead of rounding away to zero.
    scroll_accum: f32,
    /// Lines to scroll back once the snapshot with tmux's history arrives
    /// (asked for when a streamed terminal starts scrolling back).
    pub history_scroll: Option<i32>,
    /// Wanted by the UI as visible (drives subscribe/unsubscribe).
    pub wants_visible: bool,
    /// Currently subscribed on the daemon.
    pub subscribed: bool,
    /// When a streamed view was last hidden; it is dropped after a while
    /// (see `AppState::sync_streamed_terms`).
    pub hidden_since: Option<std::time::Instant>,
    /// True once a `term_snapshot` has been written.
    pub have_snapshot: bool,
    /// A grid of another size than the pane's was answered with a fresh
    /// subscribe; not repeated until the tree shows the grid's size.
    pub resync_sent: bool,
    /// The size this app last asked the daemon to give the pane. Another
    /// viewer's size is never overridden unless this app has a local cause.
    pub asserted: Option<(u16, u16)>,
    /// When this app last asked the daemon to resize the pane; tree sizes that
    /// lag behind a drag are not a mismatch (see `AppState::apply_tree`).
    pub resized_at: Option<std::time::Instant>,
    /// The surface reported exit.
    pub exited: bool,
    /// Pointer gesture in progress over this terminal (see `ui::terminal`).
    pub pointer: PointerState,
    /// Where the grid was last painted, so tests and hit-testing can map a
    /// cell to a screen position: the top-left of cell (0, 0) and one cell's size.
    pub grid_origin: egui::Pos2,
    pub cell_size: egui::Vec2,
}

/// A selection being made: both ends in screen coordinates (rows from the top
/// of the scrollback), so scrolling does not move them.
struct Sel {
    ty: SelectionType,
    anchor: (u16, u32),
    anchor_right: bool,
    end: (u16, u32),
    end_right: bool,
}

/// What the pointer is doing to a terminal between frames.
#[derive(Clone, Debug, Default)]
pub struct PointerState {
    /// The button held since a press inside the grid, if any.
    pub held: Option<egui::PointerButton>,
    /// That press went to the program as a mouse report, not to a selection.
    pub reporting: bool,
    /// The pointer left the press's cell while held.
    pub dragged: bool,
    /// Cell of the last press and of the last reported motion.
    pub press_cell: (usize, usize),
    pub last_cell: Option<(usize, usize)>,
    /// Time and count of consecutive presses, for double and triple click.
    pub last_press_time: f64,
    pub clicks: u8,
    /// The context menu was open last frame.
    pub menu_open: bool,
}

type Pos = (u16, u32);

fn key(p: Pos) -> (u32, u16) {
    (p.1, p.0)
}

impl TermView {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = TermSize::new(cols as usize, rows as usize);
        let shared = Shared::new(size);
        Self {
            term: new_term(size, &shared),
            shared,
            size,
            selection: None,
            scroll_accum: 0.0,
            history_scroll: None,
            wants_visible: false,
            subscribed: false,
            hidden_since: None,
            have_snapshot: false,
            resync_sent: false,
            asserted: None,
            resized_at: None,
            exited: false,
            pointer: PointerState::default(),
            grid_origin: egui::Pos2::ZERO,
            cell_size: egui::Vec2::new(1.0, 1.0),
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
        self.term.vt_write(bytes);
        self.shared.borrow_mut().stale = true;
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let size = TermSize::new(cols as usize, rows as usize);
        if size == self.size {
            return false;
        }
        // tmux, on growing, always takes lines back from its history; Ghostty
        // only when the cursor is on the bottom row. Park the cursor there for
        // the resize, then put it back where its content went.
        let (x, y) = (self.term.cursor_x().unwrap_or(0), self.term.cursor_y().unwrap_or(0) as usize);
        let pull = size.rows > self.size.rows
            && y + 1 < self.size.rows
            && !self.mode().alt_screen
            && self.term.is_vt_ground().unwrap_or(false)
            && !self.term.mode(Mode::ORIGIN).unwrap_or(false)
            && self.history_rows() > 0;
        let before = self.history_rows();
        if pull {
            self.term.vt_write(format!("\x1b[{};{}H", self.size.rows, x + 1).as_bytes());
        }
        let _ = self.term.resize(size.cols as u16, size.rows as u16, CELL_PX.0, CELL_PX.1);
        if pull {
            let pulled = before.saturating_sub(self.history_rows());
            let row = (y + pulled).min(size.rows - 1);
            self.term.vt_write(format!("\x1b[{};{}H", row + 1, x.min(size.cols as u16 - 1) + 1).as_bytes());
        }
        self.size = size;
        self.shared.borrow_mut().stale = true;
        true
    }

    /// Replace the grid with a fresh terminal of the snapshot's size, then play
    /// the ANSI text into it (the daemon guarantees this reproduces the screen).
    pub fn apply_snapshot(&mut self, cols: u16, rows: u16, data: &str) {
        let size = TermSize::new(cols as usize, rows as usize);
        {
            let mut s = self.shared.borrow_mut();
            s.render = Render::new(size);
            s.held = None;
            s.stale = true;
        }
        self.term = new_term(size, &self.shared);
        self.size = size;
        self.selection = None;
        // The grid is new and empty: an erase-screen here would push a blank
        // line into history, which a later shrink and grow pulls back, moving
        // the screen (and the cursor) down a row.
        let data = data.strip_prefix("\x1b[2J").unwrap_or(data);
        self.feed(data.as_bytes());
        self.shared.borrow_mut().pty.clear();
        self.have_snapshot = true;
        if let Some(lines) = self.history_scroll.take() {
            self.scroll(lines);
        }
    }

    /// Text programs copied with OSC 52 since the last call, oldest first.
    pub fn take_clipboard(&mut self) -> Vec<String> {
        std::mem::take(&mut self.shared.borrow_mut().clipboard)
    }

    /// Bytes the terminal wants sent back to the program since the last call:
    /// answers to device attribute, cursor position and colour queries.
    pub fn take_pty_writes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.shared.borrow_mut().pty)
    }

    /// The title a program set (OSC 0/2), if any.
    pub fn title(&self) -> Option<String> {
        self.shared.borrow().title.clone()
    }

    /// Bells rung since the last call.
    pub fn take_bells(&mut self) -> u32 {
        std::mem::take(&mut self.shared.borrow_mut().bells)
    }

    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_viewport(ScrollViewport::Delta(-(lines as isize)));
        self.shared.borrow_mut().stale = true;
    }

    /// Turn a raw wheel delta in points into whole lines, keeping the remainder
    /// for the next frame. Positive means the content moved down, i.e. the user
    /// is scrolling back into history, which `scroll`'s positive direction is too.
    pub fn accumulate_scroll(&mut self, points: f32, line_height: f32) -> i32 {
        if line_height <= 0.0 {
            return 0;
        }
        self.scroll_accum += points / line_height;
        let whole = self.scroll_accum.trunc();
        self.scroll_accum -= whole;
        whole as i32
    }

    /// How many lines the view is scrolled back from the live screen.
    pub fn display_offset(&self) -> usize {
        self.term.scrollbar().map(|s| s.total.saturating_sub(s.len).saturating_sub(s.offset) as usize).unwrap_or(0)
    }

    /// Lines of history above the live screen.
    pub fn history_rows(&self) -> usize {
        self.term.scrollback_rows().unwrap_or(0)
    }

    pub fn mode(&self) -> Modes {
        let on = |m: Mode| self.term.mode(m).unwrap_or(false);
        Modes {
            bracketed_paste: on(Mode::BRACKETED_PASTE),
            app_cursor: on(Mode::DECCKM),
            focus_events: on(Mode::FOCUS_EVENT),
            mouse_click: on(Mode::NORMAL_MOUSE) || on(Mode::X10_MOUSE),
            mouse_drag: on(Mode::BUTTON_MOUSE),
            mouse_motion: on(Mode::ANY_MOUSE),
            sgr_mouse: on(Mode::SGR_MOUSE),
            utf8_mouse: on(Mode::UTF8_MOUSE),
            alt_screen: self.term.active_screen().is_ok_and(|s| s == ActiveScreen::Alternate),
            show_cursor: on(Mode::CURSOR_VISIBLE),
        }
    }

    /// The visible cells. Read from the emulator when it changed since the
    /// last call, except while a program holds the screen for a redraw.
    pub fn grid(&self) -> Ref<'_, Grid> {
        let refresh = {
            let s = self.shared.borrow();
            s.stale && !s.held.is_some_and(|since| since.elapsed() < HOLD_TIMEOUT)
        };
        if refresh {
            let mut s = self.shared.borrow_mut();
            let _ = update_grid(&self.term, &mut s.render);
            s.stale = false;
        }
        Ref::map(self.shared.borrow(), |s| &s.render.grid)
    }

    pub fn cursor(&self) -> Cursor {
        self.grid().cursor
    }

    // --- selection -----------------------------------------------------------

    fn screen_pos(&self, row: usize, col: usize) -> Pos {
        let top = self.term.scrollbar().map(|s| s.offset).unwrap_or(0) as u32;
        let col = col.min(self.size.cols - 1) as u16;
        (col, top + row.min(self.size.rows - 1) as u32)
    }

    /// Start a selection at viewport cell `(row, col)`; `right_half` says which
    /// half of the cell the pointer is on.
    pub fn start_selection(&mut self, ty: SelectionType, row: usize, col: usize, right_half: bool) {
        let p = self.screen_pos(row, col);
        self.selection = Some(Sel { ty, anchor: p, anchor_right: right_half, end: p, end_right: right_half });
        self.install_selection();
    }

    /// Move the selection's moving end to viewport cell `(row, col)`.
    pub fn update_selection(&mut self, row: usize, col: usize, right_half: bool) {
        let p = self.screen_pos(row, col);
        if let Some(sel) = self.selection.as_mut() {
            sel.end = p;
            sel.end_right = right_half;
            self.install_selection();
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.install_selection();
    }

    /// The first and last cell (inclusive) of the word or line at `p`.
    fn span_at(&self, ty: SelectionType, p: Pos) -> Option<(Pos, Pos)> {
        let at = self.term.grid_ref(Point::Screen(PointCoordinate { x: p.0, y: p.1 })).ok()?;
        let sel = match ty {
            SelectionType::Semantic => self.term.select_word(SelectWordOptions::new(at)),
            _ => self.term.select_line(SelectLineOptions::new(at)),
        }
        .ok()??;
        let pos = |r: &libghostty_vt::screen::GridRef| {
            self.term.point_from_grid_ref(r, PointSpace::Screen).ok().flatten().map(|c| (c.x, c.y))
        };
        Some((pos(&sel.start())?, pos(&sel.end())?))
    }

    /// The selection's first and last cell (inclusive), in screen order.
    fn selection_range(&self, sel: &Sel) -> Option<(Pos, Pos)> {
        let cols = self.size.cols as u16;
        match sel.ty {
            SelectionType::Simple => {
                let (a, b) = if key(sel.anchor) <= key(sel.end) {
                    ((sel.anchor, sel.anchor_right), (sel.end, sel.end_right))
                } else {
                    ((sel.end, sel.end_right), (sel.anchor, sel.anchor_right))
                };
                // The half the pointer is on decides whether its cell is in.
                let start = if a.1 {
                    if a.0 .0 + 1 >= cols { (0, a.0 .1 + 1) } else { (a.0 .0 + 1, a.0 .1) }
                } else {
                    a.0
                };
                let end = if b.1 {
                    b.0
                } else if b.0 .0 == 0 {
                    (cols - 1, b.0 .1.checked_sub(1)?)
                } else {
                    (b.0 .0 - 1, b.0 .1)
                };
                (key(start) <= key(end)).then_some((start, end))
            }
            ty => {
                let (a, b) = (self.span_at(ty, sel.anchor)?, self.span_at(ty, sel.end)?);
                let start = if key(a.0) <= key(b.0) { a.0 } else { b.0 };
                let end = if key(a.1) >= key(b.1) { a.1 } else { b.1 };
                Some((start, end))
            }
        }
    }

    /// Give the emulator the selection (it draws and copies from it).
    fn install_selection(&self) {
        let range = self.selection.as_ref().and_then(|s| self.selection_range(s));
        let at = |p: Pos| self.term.grid_ref(Point::Screen(PointCoordinate { x: p.0, y: p.1 })).ok();
        match range.and_then(|(s, e)| Some((at(s)?, at(e)?))) {
            Some((s, e)) => {
                let _ = self.term.set_selection(Some(&Selection::new(s, e, false)));
            }
            None => {
                let _ = self.term.set_selection(None);
            }
        }
        self.shared.borrow_mut().stale = true;
    }

    /// Drop the local history and return to the live screen. Only this
    /// client's copy: the program and the daemon are not told.
    pub fn clear_scrollback(&mut self) {
        self.term.scroll_viewport(ScrollViewport::Bottom);
        self.term.vt_write(b"\x1b[3J");
        self.clear_selection();
    }

    /// The selected text, `None` when nothing (or only an empty range) is selected.
    pub fn selection_text(&self) -> Option<String> {
        let opts = FormatOptions::new().with_emit_format(Format::Plain).with_unwrap(true).with_trim(true);
        let bytes = self.term.format_selection_alloc(None, opts).ok()??;
        let s = String::from_utf8_lossy(&bytes).into_owned();
        (!s.is_empty()).then_some(s)
    }

    /// The characters of viewport row `row`, one per column (a wide
    /// character's spacer reads as a space).
    pub fn row_chars(&self, row: usize) -> Vec<char> {
        self.grid().row(row).iter().map(|c| if c.is_spacer() || c.flags.contains(Flags::INVISIBLE) { ' ' } else { c.ch }).collect()
    }

    /// The OSC 8 hyperlink target at viewport cell `(row, col)`.
    fn hyperlink_at(&self, row: usize, col: usize) -> Option<String> {
        let at = self.term.grid_ref(Point::Viewport(PointCoordinate { x: u16::try_from(col).ok()?, y: row as u32 })).ok()?;
        if !at.cell().ok()?.has_hyperlink().ok()? {
            return None;
        }
        let mut buf = vec![0u8; 512];
        let n = match at.hyperlink_uri(&mut buf) {
            Ok(n) => n,
            Err(GhosttyError::OutOfSpace { required }) => {
                buf = vec![0u8; required];
                at.hyperlink_uri(&mut buf).ok()?
            }
            Err(_) => return None,
        };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    /// The link under viewport cell `(row, col)`: an OSC 8 hyperlink the
    /// program set, else an http(s) URL in the row's text.
    pub fn link_at(&self, row: usize, col: usize) -> Option<crate::termio::UrlSpan> {
        let col = col.min(self.size.cols.saturating_sub(1));
        if let Some(uri) = self.hyperlink_at(row, col) {
            if crate::termio::openable_link(&uri) {
                let same = |c: usize| self.hyperlink_at(row, c).as_deref() == Some(uri.as_str());
                let mut start = col;
                while start > 0 && same(start - 1) {
                    start -= 1;
                }
                let mut end = col + 1;
                while end < self.size.cols && same(end) {
                    end += 1;
                }
                return Some(crate::termio::UrlSpan { start, end, url: uri });
            }
        }
        crate::termio::url_at(&self.row_chars(row), col)
    }
}

/// Read the visible cells out of the emulator's render state, redoing only
/// the rows it marks dirty.
fn update_grid(term: &Term, r: &mut Render) -> Result<(), GhosttyError> {
    let Render { state, rows, cells, grid } = r;
    let snap = state.update(term)?;
    let (cols, nrows) = (snap.cols()? as usize, snap.rows()? as usize);
    let mut full = snap.dirty()? == Dirty::Full;
    if grid.cols != cols || grid.rows != nrows {
        *grid = Grid::new(cols, nrows);
        full = true;
    }

    let colors = snap.colors()?;
    let default = term.default_color_palette()?;
    for i in 0..256 {
        let c = colors.palette[i];
        grid.palette[i] = (c != default.0[i]).then_some([c.r, c.g, c.b]);
    }
    let changed = |now: Option<RgbColor>, default: Option<RgbColor>| (now != default).then_some(now).flatten().map(|c| [c.r, c.g, c.b]);
    grid.fg_override = changed(term.fg_color()?, term.default_fg_color()?);
    grid.bg_override = changed(term.bg_color()?, term.default_bg_color()?);
    grid.cursor_override = changed(snap.cursor_color()?, term.default_cursor_color()?);

    let cur = snap.cursor()?;
    grid.cursor = Cursor {
        pos: cur.viewport.map(|v| (v.y as usize, v.x as usize)),
        visible: cur.visible,
        shape: match cur.visual_style {
            CursorVisualStyle::Bar => CursorShape::Beam,
            CursorVisualStyle::Underline => CursorShape::Underline,
            CursorVisualStyle::BlockHollow => CursorShape::Hollow,
            _ => CursorShape::Block,
        },
    };

    let mut it = rows.update(&snap)?;
    while let Some(row) = it.next() {
        let y = row.viewport_y()?;
        if y < 0 || y as usize >= nrows {
            continue;
        }
        let y = y as usize;
        grid.selection[y] = row.selection()?.map(|s| (s.start_x, s.end_x));
        if !full && !row.dirty()? {
            continue;
        }
        let mut ci = cells.update(row)?;
        let mut x = 0;
        while let Some(c) = ci.next() {
            if x >= cols {
                break;
            }
            read_cell(c, &mut grid.cells[y * cols + x])?;
            x += 1;
        }
        row.set_dirty(false)?;
    }
    drop(it);
    snap.clean()?;
    Ok(())
}

fn read_cell(c: &libghostty_vt::render::CellIteration, out: &mut Cell) -> Result<(), GhosttyError> {
    let raw = c.raw_cell()?;
    let styled = raw.has_styling()?;
    let tag = raw.content_tag()?;
    if !styled && tag == CellContentTag::Codepoint && raw.codepoint()? == 0 && raw.wide()? == CellWide::Narrow {
        if *out != Cell::default() {
            *out = Cell::default();
        }
        return Ok(());
    }
    let mut cell = Cell {
        wide: match raw.wide()? {
            CellWide::Wide => Wide::Head,
            CellWide::SpacerTail => Wide::Tail,
            CellWide::SpacerHead => Wide::SpacerHead,
            _ => Wide::Narrow,
        },
        ..Cell::default()
    };
    if styled {
        let st = c.style()?;
        let color = |c: StyleColor| match c {
            StyleColor::None => Color::Default,
            StyleColor::Palette(i) => Color::Index(i.0),
            StyleColor::Rgb(c) => Color::Rgb([c.r, c.g, c.b]),
        };
        cell.fg = color(st.fg_color);
        cell.bg = color(st.bg_color);
        cell.underline_color = color(st.underline_color);
        cell.underline = match st.underline {
            GUnderline::None => Underline::None,
            GUnderline::Double => Underline::Double,
            GUnderline::Curly => Underline::Curly,
            GUnderline::Dotted => Underline::Dotted,
            GUnderline::Dashed => Underline::Dashed,
            _ => Underline::Single,
        };
        for (on, flag) in [
            (st.bold, Flags::BOLD),
            (st.italic, Flags::ITALIC),
            (st.faint, Flags::FAINT),
            (st.inverse, Flags::INVERSE),
            (st.invisible, Flags::INVISIBLE),
            (st.strikethrough, Flags::STRIKE),
            (st.overline, Flags::OVERLINE),
        ] {
            if on {
                cell.flags |= flag;
            }
        }
    }
    match tag {
        CellContentTag::Codepoint => cell.ch = char::from_u32(raw.codepoint()?).filter(|&c| c != '\0').unwrap_or(' '),
        CellContentTag::CodepointGrapheme => {
            let g = c.graphemes()?;
            if let Some((&first, rest)) = g.split_first() {
                cell.ch = first;
                cell.extra = rest.to_vec();
            }
        }
        CellContentTag::BgColorPalette => cell.bg = Color::Index(raw.bg_color_palette()?.0),
        CellContentTag::BgColorRgb => {
            let c = raw.bg_color_rgb()?;
            cell.bg = Color::Rgb([c.r, c.g, c.b]);
        }
    }
    *out = cell;
    Ok(())
}

// --- colours ---------------------------------------------------------------

/// The pixel-fire palette, as `gfx::config`'s default (warm ink, cream, flame).
pub const PALETTE: [[u8; 3]; 16] = [
    [0x24, 0x22, 0x1D], // black
    [0xE0, 0x50, 0x2C], // red
    [0xB3, 0xBC, 0x88], // green
    [0xE5, 0xAD, 0x71], // yellow
    [0x7F, 0xA6, 0xBD], // blue
    [0xC8, 0x83, 0xA6], // magenta
    [0x8C, 0xBC, 0xAA], // cyan
    [0xD8, 0xD4, 0xC9], // white
    [0x74, 0x72, 0x68], // bright black
    [0xF0, 0x7A, 0x3C], // bright red
    [0xCD, 0xD6, 0xA0], // bright green
    [0xFF, 0xC2, 0x7A], // bright yellow
    [0x9C, 0xC2, 0xD8], // bright blue
    [0xE0, 0x9E, 0xC0], // bright magenta
    [0xA8, 0xD8, 0xC6], // bright cyan
    [0xFF, 0xF4, 0xE4], // bright white
];

fn cube_idx(v: u8) -> u8 {
    if v == 0 {
        0
    } else {
        55 + v * 40
    }
}

fn indexed_rgb(i: u8) -> [u8; 3] {
    match i {
        0..=15 => PALETTE[i as usize],
        16..=231 => {
            let i = i - 16;
            [cube_idx(i / 36), cube_idx((i % 36) / 6), cube_idx(i % 6)]
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            [v, v, v]
        }
    }
}

fn halve(c: [u8; 3]) -> [u8; 3] {
    [c[0] / 2, c[1] / 2, c[2] / 2]
}

/// Resolve a cell colour to RGB; `default` stands for `Color::Default`.
pub fn resolve(color: Color, grid: &Grid, default: [u8; 3]) -> [u8; 3] {
    match color {
        Color::Default => default,
        Color::Index(i) => grid.palette_override(i).unwrap_or_else(|| indexed_rgb(i)),
        Color::Rgb(c) => c,
    }
}

/// Brighten a base ANSI colour, used for the BOLD flag.
fn brighten(c: Color) -> Color {
    match c {
        Color::Index(i) if i < 8 => Color::Index(i + 8),
        c => c,
    }
}

/// Foreground/background for a cell after applying INVERSE/BOLD/FAINT and the
/// default colour handling. Returns `(fg, bg)`.
pub fn cell_colors(cell: &Cell, grid: &Grid, default_fg: [u8; 3], default_bg: [u8; 3]) -> ([u8; 3], [u8; 3]) {
    let default_fg = grid.fg_override.unwrap_or(default_fg);
    let default_bg = grid.bg_override.unwrap_or(default_bg);
    let fg = if cell.flags.contains(Flags::BOLD) { brighten(cell.fg) } else { cell.fg };
    let mut fg = resolve(fg, grid, default_fg);
    let bg = resolve(cell.bg, grid, default_bg);
    if cell.flags.contains(Flags::FAINT) {
        fg = halve(fg);
    }
    if cell.flags.contains(Flags::INVERSE) {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &TermView, row: usize) -> String {
        t.row_chars(row).into_iter().collect::<String>().trim_end().to_owned()
    }

    fn cell(t: &TermView, row: usize, col: usize) -> Cell {
        t.grid().cell(row, col).unwrap().clone()
    }

    #[test]
    fn osc52_copy_reaches_the_clipboard_queue() {
        let mut t = TermView::new(20, 4);
        // As tmux relays it: `c` selection, BEL terminator.
        t.feed(b"\x1b]52;c;UkVNT1RFX0NPUFlfT0s=\x07");
        assert_eq!(t.take_clipboard(), vec!["REMOTE_COPY_OK".to_string()]);
        // The `ST` terminator and an empty selection field are valid too.
        t.feed(b"\x1b]52;;aGk=\x1b\\");
        assert_eq!(t.take_clipboard(), vec!["hi".to_string()]);
    }

    #[test]
    fn feeds_and_reads_grid() {
        let mut t = TermView::new(20, 4);
        t.feed(b"hello\r\nworld");
        assert_eq!(text(&t, 0), "hello");
        assert_eq!(text(&t, 1), "world");
    }

    #[test]
    fn snapshot_replaces_screen() {
        let mut t = TermView::new(20, 4);
        t.feed(b"old contents");
        t.apply_snapshot(20, 4, "\x1b[2J\x1b[Hfresh");
        assert_eq!(text(&t, 0), "fresh");
        assert!(t.have_snapshot);
    }

    /// A main-screen snapshot as the daemon writes it: a 6-row pane with the
    /// input box's border, `❯` line and footer at the bottom, cursor on `❯`.
    const INPUT_BOX: &str =
        "\x1b[2J\x1b[Hout 1\r\nout 2\r\n────────\r\n❯ hint\r\n────────\r\n? help\x1b[0J\x1b[4;3H";
    /// A keystroke as Claude Code's inline renderer draws it: cursor-relative
    /// moves only, so the cursor's row decides where the letter lands.
    const KEYSTROKE: &[u8] = b"\x1b[1B\r\x1b[1A\x1b[2Cx\r\x1b[1B\x1b[1A";

    #[test]
    fn a_keystroke_lands_on_the_prompt_row() {
        let mut t = TermView::new(8, 6);
        t.apply_snapshot(8, 6, INPUT_BOX);
        t.feed(KEYSTROKE);
        assert_eq!(text(&t, 3), "❯ xint");
        assert_eq!(text(&t, 2), "────────");
    }

    /// The snapshot's erase-screen must leave no blank line in history: after
    /// a shrink and grow the screen would come back a row lower than tmux's.
    #[test]
    fn snapshot_then_shrink_and_grow_keeps_the_rows() {
        let mut t = TermView::new(8, 6);
        t.apply_snapshot(8, 6, INPUT_BOX);
        t.resize(8, 5);
        t.resize(8, 6);
        assert_eq!(t.cursor().pos.map(|p| p.0), Some(3));
        assert_eq!(text(&t, 3), "❯ hint");
        t.feed(KEYSTROKE);
        assert_eq!(text(&t, 3), "❯ xint");
        assert_eq!(text(&t, 2), "────────");
    }

    #[test]
    fn resize_changes_dims() {
        let mut t = TermView::new(80, 24);
        assert!(t.resize(100, 30));
        assert_eq!(t.cols(), 100);
        assert_eq!(t.rows(), 30);
        assert!(!t.resize(100, 30));
        assert_eq!((t.grid().cols, t.grid().rows), (100, 30));
    }

    #[test]
    fn resize_reflows_wrapped_lines() {
        let mut t = TermView::new(10, 4);
        t.feed(b"0123456789abcdef");
        assert_eq!((text(&t, 0), text(&t, 1)), ("0123456789".into(), "abcdef".into()));
        t.resize(20, 4);
        assert_eq!(text(&t, 0), "0123456789abcdef");
        assert_eq!(text(&t, 1), "");
        t.resize(8, 4);
        assert_eq!((text(&t, 0), text(&t, 1)), ("01234567".into(), "89abcdef".into()));
    }

    #[test]
    fn indexed_palette_math() {
        assert_eq!(indexed_rgb(0), PALETTE[0]);
        assert_eq!(indexed_rgb(196), [255, 0, 0]); // bright red in cube
        assert_eq!(indexed_rgb(232), [8, 8, 8]);
        assert_eq!(indexed_rgb(255), [238, 238, 238]);
    }

    #[test]
    fn inverse_swaps() {
        let t = TermView::new(4, 2);
        let g = t.grid();
        let c = Cell { flags: Flags::INVERSE, ..Cell::default() };
        let (dfg, dbg) = ([200; 3], [10; 3]);
        assert_eq!(cell_colors(&c, &g, dfg, dbg), (dbg, dfg));
    }

    #[test]
    fn selections_by_char_word_and_line() {
        let mut t = TermView::new(20, 4);
        t.feed(b"hello world\r\nnext");
        t.start_selection(SelectionType::Simple, 0, 0, false);
        t.update_selection(0, 4, true);
        assert_eq!(t.selection_text().as_deref(), Some("hello"));
        t.start_selection(SelectionType::Semantic, 0, 8, false);
        assert_eq!(t.selection_text().as_deref(), Some("world"));
        t.start_selection(SelectionType::Lines, 0, 2, false);
        assert_eq!(t.selection_text().as_deref().map(str::trim_end), Some("hello world"));
        // A press without a drag selects nothing.
        t.start_selection(SelectionType::Simple, 0, 3, false);
        assert_eq!(t.selection_text(), None);
        t.clear_selection();
        assert_eq!(t.selection_text(), None);
    }

    #[test]
    fn selection_follows_the_scrolled_view() {
        let mut t = TermView::new(10, 2);
        for i in 0..6 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        t.scroll(2);
        // Viewport row 0 is now two lines further back than the screen's top.
        t.start_selection(SelectionType::Lines, 0, 0, false);
        assert_eq!(t.selection_text().as_deref().map(str::trim_end), Some("line3"));
        assert_eq!(text(&t, 0), "line3");
    }

    #[test]
    fn selection_marks_the_painted_cells() {
        let mut t = TermView::new(20, 2);
        t.feed(b"hello world");
        t.start_selection(SelectionType::Simple, 0, 1, false);
        t.update_selection(0, 3, true);
        let g = t.grid();
        let sel: Vec<usize> = (0..20).filter(|&c| g.selected(0, c)).collect();
        assert_eq!(sel, vec![1, 2, 3]);
    }

    #[test]
    fn links_from_text_and_osc8() {
        let mut t = TermView::new(40, 3);
        t.feed(b"go to https://a.dev/x now\r\n");
        t.feed(b"\x1b]8;;https://b.dev/\x1b\\click me\x1b]8;;\x1b\\ plain");
        let l = t.link_at(0, 8).expect("text url");
        assert_eq!((l.start, l.end, l.url.as_str()), (6, 21, "https://a.dev/x"));
        assert!(t.link_at(0, 2).is_none());
        let l = t.link_at(1, 3).expect("osc 8 link");
        assert_eq!((l.start, l.end, l.url.as_str()), (0, 8, "https://b.dev/"));
        assert!(t.link_at(1, 10).is_none());
    }

    #[test]
    fn clearing_scrollback_keeps_the_screen() {
        let mut t = TermView::new(10, 2);
        for i in 0..10 {
            t.feed(format!("line {i}\r\n").as_bytes());
        }
        t.scroll(3);
        t.clear_scrollback();
        assert_eq!(t.history_rows(), 0);
        assert_eq!(t.display_offset(), 0);
        assert_eq!(text(&t, 0), "line 9");
    }

    #[test]
    fn scrollback_accumulates() {
        let mut t = TermView::new(10, 2);
        for i in 0..10 {
            t.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(t.history_rows() > 0);
        t.scroll(2);
        assert_eq!(t.display_offset(), 2);
    }

    #[test]
    fn italic_and_strikethrough_and_overline() {
        let mut t = TermView::new(20, 2);
        t.feed(b"\x1b[3mi\x1b[0m\x1b[9ms\x1b[0m\x1b[53mo\x1b[0m\x1b[2mf\x1b[0mn");
        assert!(cell(&t, 0, 0).flags.contains(Flags::ITALIC));
        assert!(cell(&t, 0, 1).flags.contains(Flags::STRIKE));
        assert!(cell(&t, 0, 2).flags.contains(Flags::OVERLINE));
        assert!(cell(&t, 0, 3).flags.contains(Flags::FAINT));
        assert_eq!(cell(&t, 0, 4).flags, Flags::default());
    }

    #[test]
    fn curly_underline_with_colour() {
        let mut t = TermView::new(20, 2);
        t.feed(b"\x1b[4:3;58;2;255;0;0mw\x1b[0m\x1b[4:2md\x1b[4:4mt\x1b[4:5ma\x1b[4mu");
        let w = cell(&t, 0, 0);
        assert_eq!(w.underline, Underline::Curly);
        assert_eq!(w.underline_color, Color::Rgb([255, 0, 0]));
        assert_eq!(cell(&t, 0, 1).underline, Underline::Double);
        assert_eq!(cell(&t, 0, 2).underline, Underline::Dotted);
        assert_eq!(cell(&t, 0, 3).underline, Underline::Dashed);
        assert_eq!(cell(&t, 0, 4).underline, Underline::Single);
    }

    #[test]
    fn palette_and_truecolor_cells() {
        let mut t = TermView::new(20, 2);
        t.feed(b"\x1b[31;44mx\x1b[38;2;1;2;3my\x1b[38;5;200mz");
        let x = cell(&t, 0, 0);
        assert_eq!((x.fg, x.bg), (Color::Index(1), Color::Index(4)));
        assert_eq!(cell(&t, 0, 1).fg, Color::Rgb([1, 2, 3]));
        assert_eq!(cell(&t, 0, 2).fg, Color::Index(200));
    }

    #[test]
    fn osc8_hyperlink_at_a_cell() {
        let mut t = TermView::new(30, 2);
        t.feed(b"\x1b]8;;https://example.com/p\x1b\\link\x1b]8;;\x1b\\ text");
        for c in 0..4 {
            let l = t.link_at(0, c).expect("link");
            assert_eq!((l.start, l.end, l.url.as_str()), (0, 4, "https://example.com/p"));
        }
        assert!(t.link_at(0, 5).is_none());
    }

    #[test]
    fn emoji_clusters_are_two_cells_wide() {
        let mut t = TermView::new(20, 2);
        // Thumbs up with a skin tone, then a family ZWJ sequence, then 'x'.
        t.feed("\u{1F44D}\u{1F3FD}\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}x".as_bytes());
        let a = cell(&t, 0, 0);
        assert_eq!((a.ch, a.wide), ('\u{1F44D}', Wide::Head));
        assert_eq!(a.extra, vec!['\u{1F3FD}']);
        assert_eq!(cell(&t, 0, 1).wide, Wide::Tail);
        let b = cell(&t, 0, 2);
        assert_eq!((b.ch, b.wide), ('\u{1F468}', Wide::Head));
        assert_eq!(b.extra.len(), 4);
        assert_eq!(cell(&t, 0, 4).ch, 'x');
        // The cursor sits right after the last cluster and the 'x'.
        assert_eq!(t.cursor().pos, Some((0, 5)));
        t.feed(b"\r\n");
        t.feed("\u{1F468}\u{200D}\u{1F469}".as_bytes());
        assert_eq!(t.cursor().pos, Some((1, 2)));
    }

    #[test]
    fn mode_flags() {
        let mut t = TermView::new(20, 4);
        assert_eq!(t.mode(), Modes { show_cursor: true, ..Modes::default() });
        t.feed(b"\x1b[?2004h\x1b[?1h\x1b[?1004h\x1b[?1000h\x1b[?1006h\x1b[?1049h");
        let m = t.mode();
        assert!(m.bracketed_paste && m.app_cursor && m.focus_events && m.alt_screen);
        assert!(m.mouse_click && m.sgr_mouse && m.mouse_reporting());
        assert!(!m.mouse_drag && !m.mouse_motion && !m.utf8_mouse);
        t.feed(b"\x1b[?2004l\x1b[?1l\x1b[?1004l\x1b[?1000l\x1b[?1002h\x1b[?1003h\x1b[?1049l\x1b[?25l");
        let m = t.mode();
        assert!(!m.bracketed_paste && !m.app_cursor && !m.focus_events && !m.alt_screen && !m.show_cursor);
        assert!(m.mouse_drag && m.mouse_motion);
    }

    #[test]
    fn device_attributes_query_is_answered() {
        let mut t = TermView::new(20, 4);
        t.feed(b"\x1b[c");
        let reply = String::from_utf8(t.take_pty_writes()).unwrap();
        assert!(reply.starts_with("\x1b[?") && reply.ends_with('c'), "{reply:?}");
        assert!(t.take_pty_writes().is_empty());
        // A cursor position report too.
        t.feed(b"ab\x1b[6n");
        assert_eq!(String::from_utf8(t.take_pty_writes()).unwrap(), "\x1b[1;3R");
    }

    #[test]
    fn colour_queries_are_answered() {
        let mut t = TermView::new(20, 4);
        t.feed(b"\x1b]11;?\x07\x1b[5n");
        let reply = String::from_utf8(t.take_pty_writes()).unwrap();
        assert!(reply.starts_with("\x1b]11;rgb:"), "{reply:?}");
        assert!(reply.ends_with("\x1b[0n"), "{reply:?}");
    }

    #[test]
    fn title_and_bell() {
        let mut t = TermView::new(20, 4);
        t.feed(b"\x1b]2;my title\x07\x07");
        assert_eq!(t.title().as_deref(), Some("my title"));
        assert_eq!(t.take_bells(), 1);
    }

    #[test]
    fn osc_colour_overrides_reach_the_grid() {
        let mut t = TermView::new(20, 2);
        t.feed(b"\x1b]4;1;rgb:10/20/30\x07\x1b]11;rgb:01/02/03\x07x");
        let g = t.grid();
        assert_eq!(g.palette_override(1), Some([0x10, 0x20, 0x30]));
        assert_eq!(g.bg_override, Some([1, 2, 3]));
    }

    #[test]
    fn synchronized_output_holds_the_frame() {
        let mut t = TermView::new(20, 2);
        t.feed(b"before");
        assert_eq!(text(&t, 0), "before");
        t.feed(b"\x1b[?2026h\x1b[H\x1b[2Jduring");
        assert_eq!(text(&t, 0), "before", "the held frame stays up");
        t.feed(b"\x1b[?2026l");
        assert_eq!(text(&t, 0), "during");
    }
}
