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
use crate::term::{cell_colors, is_wide_spacer, viewport_point};
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
        }
    }
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
    let resp = ui.interact(rect, Id::new(("term", surface)), Sense::click());
    if resp.clicked() {
        if let Some(pane) = c.state.pane_of_surface(surface) {
            c.state.focus_pane(&pane);
        }
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
                let pty_attached = c.state.local_ptys.contains_key(surface);
                let alt = c
                    .state
                    .terms
                    .get(surface)
                    .is_some_and(|tv| tv.mode().contains(TermMode::ALT_SCREEN));
                if pty_attached && alt {
                    // tmux drives this PTY through the alternate screen, which
                    // keeps no history, so the local grid has nothing to scroll:
                    // arrow keys are what the pane's application (and tmux's own
                    // copy-mode) understands.
                    let seq: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
                    let mut bytes = Vec::with_capacity(3 * lines.unsigned_abs() as usize);
                    for _ in 0..lines.unsigned_abs() {
                        bytes.extend_from_slice(seq);
                    }
                    c.state.write_local(surface, &bytes);
                } else if let Some(tv) = c.state.terms.get_mut(surface) {
                    tv.scroll(lines);
                }
            }
        }
    }

    let default_fg = Rgb { r: theme::TERM_FG.r(), g: theme::TERM_FG.g(), b: theme::TERM_FG.b() };
    let default_bg = Rgb { r: theme::TERM_BG.r(), g: theme::TERM_BG.g(), b: theme::TERM_BG.b() };
    let bg_default = color32(default_bg);
    let origin = Pos2::new(rect.min.x + PAD, rect.min.y + PAD);

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

        for cell in content.display_iter {
            let row = cell.point.line.0;
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
