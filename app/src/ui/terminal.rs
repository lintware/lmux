//! Paint an `alacritty_terminal` grid into an egui rect.
//!
//! Costs are kept down for the 60 fps repaint path (review #13): the cell buffer
//! and font metrics are reused between frames, adjacent cells that share a
//! background collapse into one rectangle, and underline width is character
//! count × cell width instead of a fresh text layout.
//!
//! Glyphs the mono face does not carry (Claude Code's `⏺ ✻`, emoji, CJK) are
//! drawn one cell at a time at their exact column so they cannot push the rest
//! of the row sideways (review #4).

use std::cell::RefCell;
use std::collections::HashMap;

use egui::{Align2, Color32, CornerRadius, FontId, Id, Pos2, Rect, Sense, Stroke, Vec2};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};

use super::Ctx;
use crate::term::{cell_colors, is_wide_spacer, viewport_point, SelectionType};
use crate::termio::{self, MouseAction, MouseButton};
use crate::theme;

const PAD: f32 = 6.0;

/// Cell size and font for a mono size, cached per size (review #13: metrics do
/// not need recomputing for every pane on every frame).
#[derive(Clone)]
struct Metrics {
    w: f32,
    h: f32,
    font: FontId,
}

#[derive(Clone)]
struct CellInfo {
    ch: char,
    fg: Color32,
    bg: Color32,
    underline: bool,
    /// Leading half of a double-width character; the next cell is its spacer.
    wide: bool,
    /// Trailing half of a wide character: occupies a cell but draws no glyph.
    spacer: bool,
    /// Combining marks that belong to `ch`.
    zw: Vec<char>,
    /// Inside the mouse selection.
    selected: bool,
}

impl CellInfo {
    fn blank() -> Self {
        Self {
            ch: ' ',
            fg: Color32::TRANSPARENT,
            bg: Color32::TRANSPARENT,
            underline: false,
            wide: false,
            spacer: false,
            zw: Vec::new(),
            selected: false,
        }
    }
}

/// Translucent wash over selected cells; the text stays drawn on top.
const SELECTION: Color32 = Color32::from_rgba_premultiplied(0x05, 0x42, 0x80, 0x80);

/// Where the grid sits on screen, for mapping the pointer to a cell.
struct Grid {
    origin: Pos2,
    w: f32,
    h: f32,
    cols: usize,
    rows: usize,
}

impl Grid {
    /// The cell under `pos`, clamped into the grid, and whether `pos` is on
    /// the cell's right half (which decides if a selection includes it).
    fn cell_at(&self, pos: Pos2) -> (usize, usize, bool) {
        let x = (pos.x - self.origin.x) / self.w;
        let y = (pos.y - self.origin.y) / self.h;
        let max_col = self.cols.saturating_sub(1);
        let col = (x.floor().max(0.0) as usize).min(max_col);
        let right_half = if x < 0.0 {
            false
        } else if x >= self.cols as f32 {
            true
        } else {
            x - col as f32 >= 0.5
        };
        let row = (y.floor().max(0.0) as usize).min(self.rows.saturating_sub(1));
        (row, col, right_half)
    }
}

/// Presses this close together (seconds) on the same cell count as a double
/// or triple click.
const MULTI_CLICK: f64 = 0.4;

/// Mouse presses, drags and releases over the grid. When the program asked
/// for mouse reports they go to it as escape sequences (Shift keeps them
/// local, as in other terminals); otherwise they drive a local selection.
fn pointer_input(ui: &egui::Ui, resp: &egui::Response, surface: &str, grid: &Grid, c: &mut Ctx) {
    let (events, now, mods_now) = ui.input(|i| (i.events.clone(), i.time, i.modifiers));
    let over = resp.contains_pointer();
    let mode = c.state.term_mode(surface);
    let mut out: Vec<u8> = Vec::new();
    let mut report = |action, button, (row, col): (usize, usize), mods| {
        if let Some(b) = termio::mouse_report(mode, action, button, col, row, mods) {
            out.extend_from_slice(&b);
        }
    };
    for ev in events {
        match ev {
            egui::Event::PointerButton { pos, button, pressed: true, modifiers }
                if over && grid_contains(grid, pos) =>
            {
                if let Some(pane) = c.state.pane_of_surface(surface) {
                    c.state.focus_pane(&pane);
                }
                let (row, col, right) = grid.cell_at(pos);
                let Some(tv) = c.state.terms.get_mut(surface) else { continue };
                // Cmd+click opens the link under the pointer, whatever the
                // program asked for.
                if modifiers.command && button == egui::PointerButton::Primary {
                    if let Some(link) = tv.link_at(row, col) {
                        termio::open_url(&link.url);
                        continue;
                    }
                }
                if termio::mouse_reporting(mode) && !modifiers.shift {
                    let Some(mb) = MouseButton::from_egui(button) else { continue };
                    report(MouseAction::Press, mb, (row, col), modifiers);
                    let p = &mut tv.pointer;
                    p.held = Some(button);
                    p.reporting = true;
                    p.last_cell = Some((row, col));
                    continue;
                }
                if button != egui::PointerButton::Primary {
                    continue;
                }
                let p = &mut tv.pointer;
                p.clicks = if now - p.last_press_time < MULTI_CLICK && p.press_cell == (row, col) {
                    p.clicks % 3 + 1
                } else {
                    1
                };
                p.last_press_time = now;
                p.press_cell = (row, col);
                p.held = Some(button);
                p.reporting = false;
                p.dragged = false;
                let ty = match p.clicks {
                    2 => SelectionType::Semantic,
                    3 => SelectionType::Lines,
                    _ => SelectionType::Simple,
                };
                tv.start_selection(ty, row, col, right);
            }
            egui::Event::PointerMoved(pos) => {
                let Some(tv) = c.state.terms.get_mut(surface) else { continue };
                let (row, col, right) = grid.cell_at(pos);
                let p = &mut tv.pointer;
                match p.held {
                    Some(button) if p.reporting => {
                        // Drag reports go out once per cell, not per pixel.
                        if p.last_cell != Some((row, col)) {
                            p.last_cell = Some((row, col));
                            if let Some(mb) = MouseButton::from_egui(button) {
                                report(MouseAction::Motion, mb, (row, col), mods_now);
                            }
                        }
                    }
                    Some(_) => {
                        if (row, col) != p.press_cell {
                            p.dragged = true;
                        }
                        tv.update_selection(row, col, right);
                    }
                    None => {
                        let hover = over && grid_contains(grid, pos);
                        if hover
                            && !mods_now.shift
                            && mode.contains(TermMode::MOUSE_MOTION)
                            && p.last_cell != Some((row, col))
                        {
                            p.last_cell = Some((row, col));
                            report(MouseAction::Motion, MouseButton::None, (row, col), mods_now);
                        }
                    }
                }
            }
            egui::Event::PointerButton { pos, button, pressed: false, modifiers } => {
                let Some(tv) = c.state.terms.get_mut(surface) else { continue };
                let p = &mut tv.pointer;
                if p.held != Some(button) {
                    continue;
                }
                p.held = None;
                if p.reporting {
                    let (row, col, _) = grid.cell_at(pos);
                    if let Some(mb) = MouseButton::from_egui(button) {
                        report(MouseAction::Release, mb, (row, col), modifiers);
                    }
                } else if p.clicks == 1 && !p.dragged {
                    // A plain click (no drag, not a word or line pick) clears.
                    tv.clear_selection();
                }
            }
            _ => {}
        }
    }
    c.state.send_term_input(surface, &out);
}

/// Wheel input over a terminal, in whole lines (positive = back in history).
fn wheel_input(ui: &egui::Ui, resp: &egui::Response, surface: &str, grid: &Grid, lines: i32, c: &mut Ctx) {
    let mode = c.state.term_mode(surface);
    let shift = ui.input(|i| i.modifiers.shift);
    let count = lines.unsigned_abs() as usize;
    if termio::mouse_reporting(mode) && !shift {
        // The program scrolls itself (tmux with `mouse on` enters copy mode).
        let Some(pos) = resp.hover_pos() else { return };
        let (row, col, _) = grid.cell_at(pos);
        let button = if lines > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
        let mods = ui.input(|i| i.modifiers);
        let mut bytes = Vec::new();
        for _ in 0..count {
            if let Some(b) = termio::mouse_report(mode, MouseAction::Press, button, col, row, mods) {
                bytes.extend_from_slice(&b);
            }
        }
        c.state.send_term_input(surface, &bytes);
        return;
    }
    let pty_attached = c.state.local_ptys.contains_key(surface);
    if pty_attached && mode.contains(TermMode::ALT_SCREEN) {
        // tmux drives this PTY through the alternate screen, which keeps no
        // history, so the local grid has nothing to scroll: arrow keys are
        // what the pane's application (and tmux's own copy-mode) understands.
        let key = if lines > 0 { egui::Key::ArrowUp } else { egui::Key::ArrowDown };
        let app_cursor = mode.contains(TermMode::APP_CURSOR);
        let seq = crate::keys::terminal_key_mode(key, egui::Modifiers::default(), app_cursor).unwrap_or_default();
        c.state.write_local(surface, &seq.repeat(count));
    } else if let Some(tv) = c.state.terms.get_mut(surface) {
        tv.scroll(lines);
    }
}

fn grid_contains(grid: &Grid, pos: Pos2) -> bool {
    let size = Vec2::new(grid.cols as f32 * grid.w, grid.rows as f32 * grid.h);
    // The padding around the grid counts as its nearest cell.
    Rect::from_min_size(grid.origin, size).expand(PAD).contains(pos)
}

#[derive(Default)]
struct Scratch {
    /// Reused across panes and frames; indexed `row * cols + col`.
    cells: Vec<CellInfo>,
    metrics: HashMap<u32, Metrics>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

fn metrics_for(ctx: &egui::Context, size: f32) -> Metrics {
    let key = size.to_bits();
    if let Some(m) = SCRATCH.with(|s| s.borrow().metrics.get(&key).cloned()) {
        return m;
    }
    let font = theme::mono(size);
    let (w, h) = ctx.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
    let m = Metrics { w: w.max(1.0), h: h.max(1.0), font };
    SCRATCH.with(|s| s.borrow_mut().metrics.insert(key, m.clone()));
    m
}

fn color32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

/// Call `f(start_column, length, bg)` for each run of cells in `row` that share
/// a non-default background. Adjacent equal backgrounds merge into one run, so a
/// full-width bar is one rectangle rather than one per cell (review #13).
fn for_each_bg_run(row: &[CellInfo], bg_default: Color32, mut f: impl FnMut(usize, usize, Color32)) {
    let mut start = 0;
    while start < row.len() {
        let bg = row[start].bg;
        let mut end = start + 1;
        while end < row.len() && row[end].bg == bg {
            end += 1;
        }
        if bg != bg_default {
            f(start, end - start, bg);
        }
        start = end;
    }
}

pub fn paint(ui: &mut egui::Ui, rect: Rect, surface: &str, _focused: bool, c: &mut Ctx) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::TERM_BG);

    let metrics = metrics_for(ui.ctx(), theme::FS_TERM);
    let usable = Vec2::new((rect.width() - 2.0 * PAD).max(1.0), (rect.height() - 2.0 * PAD).max(1.0));
    let cols = (usable.x / metrics.w).floor().max(2.0) as u16;
    let rows = (usable.y / metrics.h).floor().max(1.0) as u16;
    c.state.desired_sizes.insert(surface.to_owned(), (cols, rows));
    c.state.visible.insert(surface.to_owned());
    c.state.ensure_term(surface);

    // Click focuses this pane; the wheel scrolls scrollback when hovering.
    let resp = ui.interact(rect, Id::new(("term", surface)), Sense::click_and_drag());
    if resp.clicked() {
        if let Some(pane) = c.state.pane_of_surface(surface) {
            c.state.focus_pane(&pane);
        }
    }
    let origin = Pos2::new(rect.min.x + PAD, rect.min.y + PAD);
    let grid = Grid { origin, w: metrics.w, h: metrics.h, cols: cols as usize, rows: rows as usize };
    if let Some(tv) = c.state.terms.get_mut(surface) {
        tv.grid_origin = origin;
        tv.cell_size = Vec2::new(metrics.w, metrics.h);
    }
    pointer_input(ui, &resp, surface, &grid, c);

    // Cmd held over a link: underline it and show a hand.
    let hover_link = match resp.hover_pos() {
        Some(pos) if ui.input(|i| i.modifiers.command) && grid_contains(&grid, pos) => {
            let (row, col, _) = grid.cell_at(pos);
            c.state.terms.get(surface).and_then(|tv| tv.link_at(row, col)).map(|l| (row, l))
        }
        _ => None,
    };
    if hover_link.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.hovered() {
        // Raw wheel events, converted to points. `smooth_scroll_delta` is
        // low-pass filtered over frames, so a single notch can arrive as a
        // fraction of a line: the accumulator in the term keeps the remainder
        // and whole lines are what get scrolled.
        let wheel = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|ev| match ev {
                    egui::Event::MouseWheel { unit, delta, .. } => Some(match unit {
                        egui::MouseWheelUnit::Point => delta.y,
                        egui::MouseWheelUnit::Line => delta.y * metrics.h,
                        egui::MouseWheelUnit::Page => delta.y * metrics.h * rows as f32,
                    }),
                    _ => None,
                })
                .sum::<f32>()
        });
        if wheel != 0.0 {
            let lines = c
                .state
                .terms
                .get_mut(surface)
                .map(|tv| tv.accumulate_scroll(wheel, metrics.h))
                .unwrap_or(0);
            if lines != 0 {
                wheel_input(ui, &resp, surface, &grid, lines, c);
            }
        }
    }

    let default_fg = Rgb { r: theme::TERM_FG.r(), g: theme::TERM_FG.g(), b: theme::TERM_FG.b() };
    let default_bg = Rgb { r: theme::TERM_BG.r(), g: theme::TERM_BG.g(), b: theme::TERM_BG.b() };
    let bg_default = color32(default_bg);

    let (cols_usize, rows_usize) = (cols as usize, rows as usize);
    let n = cols_usize * rows_usize;

    // Reuse the scratch buffer instead of allocating a fresh grid each frame.
    let mut cells = SCRATCH.with(|s| std::mem::take(&mut s.borrow_mut().cells));
    if cells.len() < n {
        cells.resize(n, CellInfo::blank());
    }

    let mut cursor: Option<(usize, usize, CursorShape)> = None;
    {
        let Some(tv) = c.state.terms.get(surface) else {
            SCRATCH.with(|s| s.borrow_mut().cells = cells);
            return;
        };
        let content = tv.term().renderable_content();
        let display_offset = content.display_offset;
        let selection = content.selection;

        for cell in content.display_iter {
            // Grid lines run negative into history; the viewport row adds back
            // how far the view is scrolled.
            let row = cell.point.line.0 + display_offset as i32;
            if row < 0 || row as usize >= rows_usize {
                continue;
            }
            let col = cell.point.column.0;
            if col >= cols_usize {
                continue;
            }
            let info = &cell.cell;
            let (fg, bg) = cell_colors(info.fg, info.bg, info.flags, default_fg, default_bg);
            let spacer = is_wide_spacer(info.flags);
            let hidden = info.flags.contains(Flags::HIDDEN);
            let ch = if spacer || hidden { ' ' } else { info.c };
            let dst = &mut cells[row as usize * cols_usize + col];
            dst.ch = ch;
            dst.fg = color32(fg);
            dst.bg = color32(bg);
            dst.underline = info.flags.intersects(Flags::ALL_UNDERLINES);
            dst.wide = info.flags.contains(Flags::WIDE_CHAR);
            dst.spacer = spacer;
            dst.selected = selection.is_some_and(|s| s.contains(cell.point));
            dst.zw.clear();
            if !spacer && !hidden {
                if let Some(zw) = info.zerowidth() {
                    dst.zw.extend_from_slice(zw);
                }
            }
        }
        if content.mode.contains(TermMode::SHOW_CURSOR) {
            if let Some((r, col)) = viewport_point(display_offset, content.cursor.point) {
                if r < rows_usize && col < cols_usize {
                    cursor = Some((r, col, content.cursor.shape));
                }
            }
        }
    }

    // Backgrounds: one rectangle per run of equal colour.
    for r in 0..rows_usize {
        let y = origin.y + r as f32 * metrics.h;
        let row = &cells[r * cols_usize..r * cols_usize + cols_usize];
        for_each_bg_run(row, bg_default, |start, len, bg| {
            let cell_rect = Rect::from_min_size(
                Pos2::new(origin.x + start as f32 * metrics.w, y),
                Vec2::new(len as f32 * metrics.w, metrics.h),
            );
            painter.rect_filled(cell_rect, 0.0, bg);
        });
        // Selection: one wash per run of selected cells, under the text.
        let mut col = 0;
        while col < cols_usize {
            if !row[col].selected {
                col += 1;
                continue;
            }
            let start = col;
            while col < cols_usize && row[col].selected {
                col += 1;
            }
            let sel_rect = Rect::from_min_size(
                Pos2::new(origin.x + start as f32 * metrics.w, y),
                Vec2::new((col - start) as f32 * metrics.w, metrics.h),
            );
            painter.rect_filled(sel_rect, 0.0, SELECTION);
        }
    }

    let draw_run = |run: &mut String, col: usize, y: f32, fg: Color32, underline: bool, len: usize| {
        let text = run.trim_end();
        if text.is_empty() {
            run.clear();
            return;
        }
        let x = origin.x + col as f32 * metrics.w;
        painter.text(Pos2::new(x, y), Align2::LEFT_TOP, text, metrics.font.clone(), fg);
        if underline {
            // Character count × cell width (review #13): no text layout needed.
            let w = len as f32 * metrics.w;
            painter.line_segment(
                [Pos2::new(x, y + metrics.h - 1.0), Pos2::new(x + w, y + metrics.h - 1.0)],
                Stroke::new(1.0, fg),
            );
        }
        run.clear();
    };

    // Text: batch runs of ASCII (the mono face advances exactly one cell), draw
    // every other glyph at its exact column so a fallback face's advance cannot
    // push the row sideways.
    let mut scratch_text = String::new();
    for r in 0..rows_usize {
        let y = origin.y + r as f32 * metrics.h;
        let row = &cells[r * cols_usize..r * cols_usize + cols_usize];
        let mut run = String::new();
        let mut run_col = 0usize;
        let mut run_fg = Color32::TRANSPARENT;
        let mut run_underline = false;
        let mut run_len = 0usize;

        for (col, info) in row.iter().enumerate() {
            if info.spacer {
                draw_run(&mut run, run_col, y, run_fg, run_underline, run_len);
                run_len = 0;
                continue;
            }
            if info.ch.is_ascii() {
                if !run.is_empty() && (info.fg != run_fg || info.underline != run_underline) {
                    draw_run(&mut run, run_col, y, run_fg, run_underline, run_len);
                    run_len = 0;
                }
                if run.is_empty() {
                    run_col = col;
                    run_fg = info.fg;
                    run_underline = info.underline;
                }
                run.push(info.ch);
                run_len += 1;
            } else {
                draw_run(&mut run, run_col, y, run_fg, run_underline, run_len);
                run_len = 0;
                let x = origin.x + col as f32 * metrics.w;
                if info.ch != ' ' {
                    scratch_text.clear();
                    scratch_text.push(info.ch);
                    scratch_text.extend(info.zw.iter());
                    painter.text(
                        Pos2::new(x, y),
                        Align2::LEFT_TOP,
                        &scratch_text,
                        metrics.font.clone(),
                        info.fg,
                    );
                }
                if info.underline {
                    let w = if info.wide { 2.0 } else { 1.0 } * metrics.w;
                    painter.line_segment(
                        [Pos2::new(x, y + metrics.h - 1.0), Pos2::new(x + w, y + metrics.h - 1.0)],
                        Stroke::new(1.0, info.fg),
                    );
                }
            }
        }
        draw_run(&mut run, run_col, y, run_fg, run_underline, run_len);
    }

    if let Some((r, link)) = &hover_link {
        let y = origin.y + (*r as f32 + 1.0) * metrics.h - 1.0;
        let x0 = origin.x + link.start as f32 * metrics.w;
        let x1 = origin.x + link.end.min(cols_usize) as f32 * metrics.w;
        painter.line_segment([Pos2::new(x0, y), Pos2::new(x1, y)], Stroke::new(1.0, theme::ACCENT_HOVER));
    }

    // Cursor.
    if let Some((r, col, shape)) = cursor {
        let cell_rect = Rect::from_min_size(
            Pos2::new(origin.x + col as f32 * metrics.w, origin.y + r as f32 * metrics.h),
            Vec2::new(metrics.w, metrics.h),
        );
        let cursor_color = theme::TERM_CURSOR;
        match shape {
            CursorShape::Beam => {
                let beam = Rect::from_min_size(cell_rect.min, Vec2::new(2.0, cell_rect.height()));
                painter.rect_filled(beam, 0.0, cursor_color);
            }
            CursorShape::Underline => {
                let line = Rect::from_min_size(
                    Pos2::new(cell_rect.min.x, cell_rect.max.y - 2.0),
                    Vec2::new(cell_rect.width(), 2.0),
                );
                painter.rect_filled(line, 0.0, cursor_color);
            }
            CursorShape::HollowBlock => {
                painter.rect_stroke(cell_rect, 0.0, Stroke::new(1.0, cursor_color), egui::StrokeKind::Inside);
            }
            _ => {
                // Block: fill, then redraw the glyph underneath in the bg colour.
                painter.rect_filled(cell_rect, CornerRadius::ZERO, cursor_color);
                let ch = cells
                    .get(r * cols_usize + col)
                    .map(|c| c.ch)
                    .unwrap_or(' ');
                if ch != ' ' {
                    painter.text(
                        Pos2::new(cell_rect.min.x, cell_rect.min.y),
                        Align2::LEFT_TOP,
                        ch.to_string(),
                        metrics.font.clone(),
                        theme::TERM_BG,
                    );
                }
            }
        }
    }

    SCRATCH.with(|s| s.borrow_mut().cells = cells);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(bg: Color32) -> CellInfo {
        CellInfo { bg, ..CellInfo::blank() }
    }

    #[test]
    fn backgrounds_merge_into_runs() {
        let default = Color32::BLACK;
        let red = Color32::RED;
        let blue = Color32::BLUE;
        // one red run of 3, a default gap, then a blue run of 2.
        let row = vec![
            cell(red),
            cell(red),
            cell(red),
            cell(default),
            cell(blue),
            cell(blue),
        ];
        let mut runs = Vec::new();
        for_each_bg_run(&row, default, |start, len, bg| runs.push((start, len, bg)));
        assert_eq!(runs, vec![(0, 3, red), (4, 2, blue)]);
    }

    #[test]
    fn full_width_bar_is_one_rectangle() {
        let red = Color32::RED;
        let row = vec![cell(red); 200];
        let mut count = 0;
        for_each_bg_run(&row, Color32::BLACK, |_, _, _| count += 1);
        assert_eq!(count, 1, "a 200-cell bar must be a single rectangle, not 200");
    }
}
