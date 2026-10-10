//! The command palette (Cmd+Shift+P) and Go to Workspace (Cmd+P), as in cmux:
//! a field at the top of the window, a fuzzy-filtered list under it, arrows to
//! move, Enter to run, Escape to close. Workspaces come from every host.

use egui::{Align2, Color32, CornerRadius, Id, Key, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

use super::Ctx;
use crate::state::{Palette, PaletteMode};
use crate::theme;
use crate::util::ellipsize;

const WIDTH: f32 = 520.0;
const ROW_H: f32 = 30.0;
const MAX_ROWS: usize = 12;

/// Every command the palette offers, with its shortcut for the hint column.
pub const COMMANDS: &[(&str, &str)] = &[
    ("New Workspace", "⌘N"),
    ("Rename Workspace", "⇧⌘R"),
    ("Close Workspace", "⇧⌘W"),
    ("Next Workspace", "⌃⌘]"),
    ("Previous Workspace", "⌃⌘["),
    ("New Terminal Tab", "⌘T"),
    ("New Browser Tab", "⇧⌘B"),
    ("Rename Tab", "⌘R"),
    ("Close Tab", "⌘W"),
    ("Next Tab", "⇧⌘]"),
    ("Previous Tab", "⇧⌘["),
    ("Split Right", "⌘D"),
    ("Split Down", "⇧⌘D"),
    ("Toggle Pane Zoom", "⇧⌘↩"),
    ("Toggle Sidebar", "⌘B"),
    ("Show Notifications", "⌘I"),
    ("Jump to Latest Unread", "⇧⌘U"),
    ("New Folder", ""),
    ("Add Remote Host…", ""),
    ("New Cloud Box…", ""),
    ("Import from cmux…", ""),
    ("Zoom In", "⌘="),
    ("Zoom Out", "⌘-"),
    ("Reset Zoom", "⌘0"),
];

/// One row: what it shows, a dim detail on the right, and what it does.
#[derive(Clone, Debug, PartialEq)]
pub enum Pick {
    Command(&'static str),
    /// `(host index, workspace ref)`.
    Workspace(usize, String),
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub detail: String,
    pub pick: Pick,
}

/// Subsequence match, case-insensitive. Higher is better; `None` means no
/// match. Consecutive letters and word starts score more, so "nw" ranks
/// "New Workspace" above "Next Workspace"'s scattered match.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut ti = 0;
    let mut prev: Option<usize> = None;
    for qc in query.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (ti..t.len()).find(|&i| t[i] == qc)?;
        score += 1;
        if prev.is_some_and(|p| p + 1 == found) {
            score += 5;
        }
        if found == 0 || !t[found - 1].is_alphanumeric() {
            score += 3;
        }
        prev = Some(found);
        ti = found + 1;
    }
    // Shorter texts win ties.
    Some(score * 100 - t.len() as i32)
}

/// The rows for a mode and query, best first.
pub fn items(mode: PaletteMode, query: &str, hosts: &[&mut Ctx]) -> Vec<Item> {
    let mut all: Vec<Item> = match mode {
        PaletteMode::Commands => COMMANDS
            .iter()
            .map(|(name, keys)| Item { label: (*name).to_owned(), detail: (*keys).to_owned(), pick: Pick::Command(name) })
            .collect(),
        PaletteMode::Workspaces => {
            let multi = hosts.len() > 1;
            hosts
                .iter()
                .enumerate()
                .flat_map(|(i, h)| {
                    let host = h.net.host_name();
                    h.state
                        .sidebar_order()
                        .into_iter()
                        .filter_map(|id| h.state.workspace(&id).map(|w| (id, w.title.clone(), w.folder.clone())))
                        .map(|(id, title, folder)| {
                            let trail = folder.map(|f| h.state.folder_path(&f)).unwrap_or_default();
                            let detail = match (multi, trail.is_empty()) {
                                (true, true) => host.clone(),
                                (true, false) => format!("{host} · {trail}"),
                                (false, _) => trail,
                            };
                            Item { label: title, detail, pick: Pick::Workspace(i, id) }
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        }
    };
    if query.trim().is_empty() {
        return all;
    }
    let mut scored: Vec<(i32, Item)> = all
        .drain(..)
        .filter_map(|it| {
            let hay = format!("{} {}", it.label, it.detail);
            fuzzy_score(query, &it.label)
                .map(|s| s + 50)
                .or_else(|| fuzzy_score(query, &hay))
                .map(|s| (s, it))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, it)| it).collect()
}

/// Draw the palette if it is open on the host on screen (`hosts[active]`).
/// Returns what was picked, for the caller to run against the right host.
pub fn show(ui: &mut Ui, area: Rect, hosts: &mut [&mut Ctx], active: usize) -> Option<Pick> {
    let mut pal: Palette = hosts.get(active)?.state.palette.clone()?;
    let list = items(pal.mode, &pal.query, hosts);
    if pal.index >= list.len() {
        pal.index = list.len().saturating_sub(1);
    }

    // Keys first, so the field below never sees the arrows or Enter.
    let (up, down, enter, escape) = ui.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, Key::Enter),
            i.consume_key(egui::Modifiers::NONE, Key::Escape),
        )
    });
    if escape {
        hosts[active].state.palette = None;
        return None;
    }
    if up {
        pal.index = pal.index.saturating_sub(1);
    }
    if down && pal.index + 1 < list.len() {
        pal.index += 1;
    }
    let mut picked: Option<Pick> = if enter { list.get(pal.index).map(|it| it.pick.clone()) } else { None };

    // Dim the window behind; a click outside closes.
    let ctx = ui.ctx().clone();
    let layer = egui::LayerId::new(egui::Order::Middle, Id::new("palette-dim"));
    let painter = ctx.layer_painter(layer);
    painter.rect_filled(area, 0.0, Color32::from_black_alpha(90));

    let rows = list.len().min(MAX_ROWS);
    let width = WIDTH.min(area.width() - 32.0);
    let height = 44.0 + rows as f32 * ROW_H + if rows > 0 { 8.0 } else { 0.0 };
    let panel = Rect::from_min_size(
        egui::pos2(area.center().x - width / 2.0, area.min.y + 56.0),
        Vec2::new(width, height),
    );
    let mut closed_by_click = false;
    egui::Area::new(Id::new("palette"))
        .order(egui::Order::Foreground)
        .fixed_pos(panel.min)
        .show(&ctx, |ui| {
            let p = ui.painter().clone();
            p.rect_filled(panel, CornerRadius::same(10), theme::SIDEBAR_BG);
            p.rect_stroke(panel, CornerRadius::same(10), Stroke::new(1.0, theme::HAIRLINE), StrokeKind::Inside);

            let field = Rect::from_min_size(panel.min + Vec2::new(12.0, 10.0), Vec2::new(width - 24.0, 24.0));
            let hint = match pal.mode {
                PaletteMode::Commands => "Type a command",
                PaletteMode::Workspaces => "Go to workspace",
            };
            let before = pal.query.clone();
            ui.scope_builder(egui::UiBuilder::new().max_rect(field), |ui| {
                ui.style_mut().visuals.extreme_bg_color = theme::SIDEBAR_BG;
                let out = ui.add(
                    egui::TextEdit::singleline(&mut pal.query)
                        .id(Id::new("palette-field"))
                        .hint_text(hint)
                        .font(theme::ui(theme::FS_TITLE))
                        .frame(egui::Frame::NONE)
                        .desired_width(field.width()),
                );
                out.request_focus();
            });
            if pal.query != before {
                pal.index = 0;
            }
            p.line_segment(
                [egui::pos2(panel.min.x, panel.min.y + 42.0), egui::pos2(panel.max.x, panel.min.y + 42.0)],
                Stroke::new(1.0, theme::HAIRLINE),
            );

            // Keep the selection on screen.
            let first = pal.index.saturating_sub(MAX_ROWS - 1);
            for (row, it) in list.iter().enumerate().skip(first).take(MAX_ROWS) {
                let y = panel.min.y + 46.0 + (row - first) as f32 * ROW_H;
                let r = Rect::from_min_size(egui::pos2(panel.min.x + 6.0, y), Vec2::new(width - 12.0, ROW_H));
                let resp = ui.interact(r, Id::new(("palette-row", row)), Sense::click());
                if resp.hovered() && ui.input(|i| i.pointer.delta() != Vec2::ZERO) {
                    pal.index = row;
                }
                if row == pal.index {
                    p.rect_filled(r, CornerRadius::same(6), theme::CARD_SELECTED);
                }
                let fg = if row == pal.index { Color32::WHITE } else { theme::TEXT };
                let detail_w = crate::util::measure(ui.ctx(), &it.detail, &theme::ui(theme::FS_SMALL));
                p.text(
                    egui::pos2(r.min.x + 10.0, r.center().y),
                    Align2::LEFT_CENTER,
                    ellipsize(ui.ctx(), &it.label, &theme::ui(theme::FS_BODY), r.width() - detail_w - 32.0, false),
                    theme::ui(theme::FS_BODY),
                    fg,
                );
                p.text(
                    egui::pos2(r.max.x - 10.0, r.center().y),
                    Align2::RIGHT_CENTER,
                    &it.detail,
                    theme::ui(theme::FS_SMALL),
                    if row == pal.index { Color32::from_rgb(0xFF, 0xE4, 0xC8) } else { theme::DIM },
                );
                if resp.clicked() {
                    picked = Some(it.pick.clone());
                }
            }
            if ui.input(|i| i.pointer.primary_clicked())
                && ui.input(|i| i.pointer.interact_pos()).is_some_and(|pos| !panel.contains(pos))
            {
                closed_by_click = true;
            }
        });

    let st = &mut hosts[active].state;
    st.palette = if picked.is_some() || closed_by_click { None } else { Some(pal) };
    picked
}
