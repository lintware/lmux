//! Tab strips.
//!
//! Two distinct things live here:
//! - [`workspace_header`] — the workspace's own header row (icon + title). It is
//!   deliberately **not** a chip, so it can never read as an unclosable tab.
//! - [`pane_strip`] — one strip **per pane**, carrying that pane's surface chips
//!   (`icon · title · ×`) and that pane's right-aligned action group.
//!
//! Every chip draws its close button inside itself, right after the title, and
//! always visible — there is no chip that cannot be closed.

use egui::{
    Align2, Color32, CornerRadius, Id, Rect, Sense, Stroke, TextEdit, Ui, UiBuilder, Vec2,
};
use lmux_proto::methods as m;
use lmux_proto::{Pane, Surface, SurfaceKind};

use super::Ctx;
use crate::state::DragItem;
use crate::theme;
use crate::util::{ellipsize, measure};

use super::icons;

const PAD_L: f32 = 8.0;
const PAD_R: f32 = 7.0;
const ICON_W: f32 = 13.0;
const GAP: f32 = 5.0;
const CLOSE_W: f32 = 15.0;
const MIN_TITLE: f32 = 14.0;
const MAX_TITLE: f32 = 170.0;
const MIN_CHIP: f32 = PAD_L + ICON_W + GAP + MIN_TITLE + GAP + CLOSE_W + PAD_R;
const ACTION_STEP: f32 = 22.0;
const ACTIONS: [&str; 5] = ["plus", "browser", "split_right", "split_down", "ellipsis"];

/// The workspace's header row: a folder glyph and the workspace title.
pub fn workspace_header(ui: &mut Ui, rect: Rect, c: &mut Ctx) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, theme::STRIP_BG);

    let cy = rect.center().y;
    // With the sidebar hidden this row runs to the window's left edge, where
    // the macOS traffic lights sit and where the sidebar toggle is drawn (after
    // us, so it is not painted over). Keep the title clear of both.
    let lead = if c.state.sidebar_hidden {
        theme::titlebar_inset() + 30.0
    } else {
        0.0
    };
    icons::folder(
        &p,
        Rect::from_center_size(egui::pos2(rect.min.x + lead + 18.0, cy), Vec2::splat(13.0)),
        theme::ACCENT,
    );

    let title = c
        .state
        .selected_workspace()
        .map(|w| w.title.clone())
        .unwrap_or_else(|| "lmux".to_owned());
    let font = theme::ui(theme::FS_SMALL);
    let max_w = (rect.width() - 40.0 - lead).max(20.0);
    p.text(
        egui::pos2(rect.min.x + lead + 32.0, cy),
        Align2::LEFT_CENTER,
        ellipsize(ui.ctx(), &title, &font, max_w, false),
        font,
        theme::DIM,
    );
}

/// One pane's tab strip: its surface chips and its own action group.
pub fn pane_strip(
    ui: &mut Ui,
    rect: Rect,
    pane: &Pane,
    active: Option<&str>,
    focused: bool,
    c: &mut Ctx,
) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, theme::STRIP_BG);
    p.line_segment(
        [
            egui::pos2(rect.min.x, rect.max.y - 0.5),
            egui::pos2(rect.max.x, rect.max.y - 0.5),
        ],
        Stroke::new(1.0, theme::HAIRLINE),
    );

    // The strip's empty area is the pane's drag handle. It is registered first,
    // so the chips and the action group drawn after it take precedence.
    let header = ui.interact(rect, Id::new(("panestrip", &pane.id)), Sense::click_and_drag());
    if header.double_clicked() {
        // Double clicking the strip's empty run is the `+`: a new tab right
        // where the pointer already is.
        c.state.new_terminal_chip(&pane.id);
    }
    if header.clicked() && !focused {
        c.state.focus_pane(&pane.id);
    }
    if header.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or_else(|| rect.center());
        let size = c
            .state
            .pane_rects
            .get(&pane.id)
            .map(|r| r.size())
            .unwrap_or_else(|| Vec2::new(rect.width(), rect.height()));
        c.state.begin_tab_drag(DragItem::Pane(pane.id.clone()), &pane.id, origin - rect.min, size, origin);
        ui.ctx().request_repaint();
    }

    // The action group gives way to the chips before they do.
    let mut actions = ACTIONS.len();
    while actions > 1 && rect.width() - (actions as f32 * ACTION_STEP) - 10.0 < MIN_CHIP {
        actions -= 1;
    }
    let group_left = rect.max.x - actions as f32 * ACTION_STEP - 4.0;

    let font = theme::ui(theme::FS_SMALL);
    let natural: Vec<f32> = pane
        .surfaces
        .iter()
        .map(|s| chip_natural_width(ui.ctx(), s, &font))
        .collect();
    let active_idx = pane
        .surfaces
        .iter()
        .position(|s| active == Some(s.id.as_str()))
        .unwrap_or(0);
    let budget = (group_left - rect.min.x - 6.0).max(MIN_CHIP);
    let widths = crate::layout::fit_chips(&natural, MIN_CHIP, budget, active_idx);

    let mut x = rect.min.x + 6.0;
    for (i, surface) in pane.surfaces.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(0.0);
        if w <= 0.0 {
            continue;
        }
        let chip = Rect::from_min_size(egui::pos2(x, rect.min.y), Vec2::new(w, rect.height()));
        chip_ui(ui, chip, surface, active == Some(surface.id.as_str()), x > rect.min.x + 7.0, pane, focused, c);
        x += w;
    }

    draw_actions(ui, rect, group_left, actions, pane, c);
}

fn chip_natural_width(ctx: &egui::Context, surface: &Surface, font: &egui::FontId) -> f32 {
    let title_w = measure(ctx, &surface.title, font).clamp(MIN_TITLE, MAX_TITLE);
    PAD_L + ICON_W + GAP + title_w + GAP + CLOSE_W + PAD_R
}

#[allow(clippy::too_many_arguments)]
fn chip_ui(
    ui: &mut Ui,
    chip: Rect,
    surface: &Surface,
    is_active: bool,
    has_left_neighbour: bool,
    pane: &Pane,
    focused: bool,
    c: &mut Ctx,
) {
    let p = ui.painter_at(chip);
    c.state.chip_rects.insert(surface.id.clone(), chip);
    // Click selects, drag starts a move/split/swap gesture: `click_and_drag`
    // keeps the existing click, middle-click, double-click and menu behaviour.
    let resp = ui.interact(chip, Id::new(("chip", &surface.id)), Sense::click_and_drag());

    if is_active {
        p.rect_filled(chip, CornerRadius::ZERO, theme::chip_active_bg());
    } else if resp.hovered() {
        p.rect_filled(chip, CornerRadius::ZERO, theme::chip_hover_bg());
    }
    if is_active {
        p.line_segment(
            [
                egui::pos2(chip.min.x, chip.min.y + 1.0),
                egui::pos2(chip.max.x, chip.min.y + 1.0),
            ],
            Stroke::new(2.0, theme::ACCENT),
        );
    }
    if has_left_neighbour {
        p.line_segment(
            [
                egui::pos2(chip.min.x + 0.5, chip.min.y + 4.0),
                egui::pos2(chip.min.x + 0.5, chip.max.y - 4.0),
            ],
            Stroke::new(1.0, theme::HAIRLINE),
        );
    }

    let color = if is_active { theme::TEXT_SOFT } else { theme::DIM };
    let icon_center = egui::pos2(chip.min.x + PAD_L + ICON_W * 0.5, chip.center().y);
    let icon = Rect::from_center_size(icon_center, Vec2::splat(theme::ICON));
    match &surface.kind {
        SurfaceKind::Terminal { .. } => icons::terminal(&p, icon, color),
        SurfaceKind::Browser { .. } => icons::globe(&p, icon, color),
    }

    let title_x = chip.min.x + PAD_L + ICON_W + GAP;
    let title_w = (chip.width() - (PAD_L + ICON_W + GAP + GAP + CLOSE_W + PAD_R)).max(0.0);

    let editing = c.state.renaming.as_ref().is_some_and(|r| r.surface == surface.id);
    if editing {
        rename_editor(ui, chip, title_x, surface, c);
    } else {
        p.text(
            egui::pos2(title_x, chip.center().y),
            Align2::LEFT_CENTER,
            ellipsize(ui.ctx(), &surface.title, &theme::ui(theme::FS_SMALL), title_w, false),
            theme::ui(theme::FS_SMALL),
            color,
        );
    }

    // Close button: inside the chip, right after the title, always visible.
    let close_center = egui::pos2(chip.max.x - PAD_R - CLOSE_W * 0.5, chip.center().y);
    let close_hit = Rect::from_center_size(close_center, Vec2::splat(16.0));
    let close_resp = ui.interact(close_hit, Id::new(("chipx", &surface.id)), Sense::click());
    let close_color = if close_resp.hovered() {
        theme::RED
    } else if is_active {
        theme::DIM
    } else {
        theme::DIM_2
    };
    icons::close(&p, Rect::from_center_size(close_center, Vec2::splat(9.0)), close_color);
    if close_resp.clicked() {
        c.state.close_surface(&pane.id, &surface.id);
        return;
    }

    // Dragging the chip starts a move/split/swap gesture. The click, rename and
    // context-menu paths below are unaffected: egui only reports a drag once the
    // pointer has moved past its threshold with the button held.
    if resp.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or_else(|| chip.center());
        c.state.begin_tab_drag(
            DragItem::Surface(surface.id.clone()),
            &pane.id,
            origin - chip.min,
            chip.size(),
            origin,
        );
        ui.ctx().request_repaint();
        return;
    }

    if resp.clicked_by(egui::PointerButton::Middle) {
        c.state.close_surface(&pane.id, &surface.id);
        return;
    }
    if resp.double_clicked() {
        c.state.begin_rename(&surface.id, &surface.title);
        return;
    }
    if resp.clicked() {
        c.state.select_tab(&pane.id, &surface.id);
        if !focused {
            c.state.focus_pane(&pane.id);
        }
    }

    if let Some(action) = tab_menu(&resp, surface, pane, c) {
        run_tab_action(action, surface, pane, c);
    }
}

/// What the tab's right-click menu asked for.
enum TabAction {
    Rename,
    Close,
    CloseLeft,
    CloseRight,
    CloseOthers,
    MoveToWorkspace(String),
    MoveToNewWorkspace,
    MoveToPane(String),
    NewTab { browser: bool },
    FullWidth,
    CopyIds,
}

fn tab_menu(resp: &egui::Response, surface: &Surface, pane: &Pane, c: &mut Ctx) -> Option<TabAction> {
    let idx = pane.surfaces.iter().position(|s| s.id == surface.id).unwrap_or(0);
    let count = pane.surfaces.len();
    let left = c.state.pane_beside(&pane.id, -1);
    let right = c.state.pane_beside(&pane.id, 1);
    let current = c.state.selected.clone();
    let others: Vec<(String, String)> = c
        .state
        .sidebar_order()
        .into_iter()
        .filter(|w| Some(w) != current.as_ref())
        .filter_map(|w| c.state.workspace(&w).map(|ws| (w, ws.title.clone())))
        .collect();
    let split = c.state.selected_workspace().is_some_and(|w| w.panes.len() > 1);
    let full = c.state.zoomed_pane.as_deref() == Some(pane.id.as_str());

    let mut action = None;
    resp.context_menu(|ui| {
        ui.set_min_width(220.0);
        item(ui, &mut action, "Rename Tab…", "⌘R", true, TabAction::Rename);
        ui.separator();
        item(ui, &mut action, "Close Tab", "⌘W", true, TabAction::Close);
        item(ui, &mut action, "Close Tabs to Left", "", idx > 0, TabAction::CloseLeft);
        item(ui, &mut action, "Close Tabs to Right", "", idx + 1 < count, TabAction::CloseRight);
        item(ui, &mut action, "Close Other Tabs", "", count > 1, TabAction::CloseOthers);
        ui.menu_button("Move Tab", |ui| {
            ui.set_min_width(180.0);
            for (id, title) in &others {
                if ui.button(title).clicked() {
                    action = Some(TabAction::MoveToWorkspace(id.clone()));
                    ui.close();
                }
            }
            if !others.is_empty() {
                ui.separator();
            }
            if ui.button("New Workspace").clicked() {
                action = Some(TabAction::MoveToNewWorkspace);
                ui.close();
            }
        });
        for (label, target) in [("Move to Left Pane", &left), ("Move to Right Pane", &right)] {
            if ui.add_enabled(target.is_some(), egui::Button::new(label)).clicked() {
                action = target.clone().map(TabAction::MoveToPane);
                ui.close();
            }
        }
        ui.separator();
        item(ui, &mut action, "New Terminal Tab to Right", "⌘T", true, TabAction::NewTab { browser: false });
        item(ui, &mut action, "New Browser Tab to Right", "", true, TabAction::NewTab { browser: true });
        ui.separator();
        let full_label = if full { "Exit Full Width" } else { "Full Width Tab" };
        item(ui, &mut action, full_label, "⇧⌘↩", split || full, TabAction::FullWidth);
        ui.separator();
        item(ui, &mut action, "Copy IDs", "", true, TabAction::CopyIds);
    });
    action
}

/// One menu row with its shortcut shown on the right.
fn item(ui: &mut Ui, action: &mut Option<TabAction>, label: &str, key: &str, enabled: bool, a: TabAction) {
    let mut b = egui::Button::new(label);
    if !key.is_empty() {
        b = b.shortcut_text(key);
    }
    if ui.add_enabled(enabled, b).clicked() {
        *action = Some(a);
        ui.close();
    }
}

fn run_tab_action(action: TabAction, surface: &Surface, pane: &Pane, c: &mut Ctx) {
    let ids: Vec<&str> = pane.surfaces.iter().map(|s| s.id.as_str()).collect();
    let idx = ids.iter().position(|s| *s == surface.id).unwrap_or(0);
    let close = |c: &mut Ctx, which: &[&str]| {
        for s in which {
            c.state.close_surface(&pane.id, s);
        }
    };
    match action {
        TabAction::Rename => c.state.begin_rename(&surface.id, &surface.title),
        TabAction::Close => c.state.close_surface(&pane.id, &surface.id),
        TabAction::CloseLeft => close(c, &ids[..idx]),
        TabAction::CloseRight => close(c, &ids[idx + 1..]),
        TabAction::CloseOthers => {
            c.state.select_tab(&pane.id, &surface.id);
            let rest: Vec<&str> = ids.iter().copied().filter(|s| *s != surface.id).collect();
            close(c, &rest);
        }
        TabAction::MoveToWorkspace(ws) => {
            c.state.rpc(m::SURFACE_MOVE, serde_json::json!({ "surface": surface.id, "workspace": ws }));
        }
        TabAction::MoveToNewWorkspace => {
            c.state.rpc(m::SURFACE_MOVE_TO_NEW_WORKSPACE, serde_json::json!({ "surface": surface.id }));
        }
        TabAction::MoveToPane(to) => {
            c.state.rpc(m::SURFACE_MOVE, serde_json::json!({ "surface": surface.id, "pane": to }));
            c.state.focus_pane(&to);
        }
        TabAction::NewTab { browser } => c.state.new_chip_after(&pane.id, &surface.id, browser),
        TabAction::FullWidth => {
            c.state.focus_pane(&pane.id);
            c.state.select_tab(&pane.id, &surface.id);
            c.state.toggle_zoom();
        }
        TabAction::CopyIds => {
            // The refs an agent passes to `lmux … --workspace/--pane/--surface`.
            // A tab on screen is in the selected workspace.
            let ws = c.state.selected.clone().map(|w| format!("{w} ")).unwrap_or_default();
            c.state.clipboard_out = Some(format!("{ws}{} {}", pane.id, surface.id));
        }
    }
}

fn rename_editor(ui: &mut Ui, chip: Rect, title_x: f32, surface: &Surface, c: &mut Ctx) {
    let Some(ren) = c.state.renaming.clone() else { return };
    let mut text = ren.text.clone();
    let mut commit = false;
    let mut cancel = false;

    let edit_rect = Rect::from_min_max(
        egui::pos2(title_x - 2.0, chip.min.y + 3.0),
        egui::pos2(chip.max.x - PAD_R - CLOSE_W, chip.max.y - 2.0),
    );
    let edit_id = Id::new(("rename", &surface.id));
    ui.scope_builder(
        UiBuilder::new().max_rect(edit_rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            ui.style_mut().visuals.extreme_bg_color = Color32::from_rgb(0x2A, 0x2A, 0x2A);
            ui.style_mut().visuals.selection.bg_fill = theme::ACCENT;
            let out = ui.add(
                TextEdit::singleline(&mut text)
                    .id(edit_id)
                    .font(theme::ui(theme::FS_SMALL))
                    .text_color(theme::TEXT)
                    .desired_width(edit_rect.width().max(24.0)),
            );
            if !ren.focus_requested {
                out.request_focus();
            }
            if out.lost_focus() {
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    commit = true;
                } else {
                    cancel = true;
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
        },
    );

    if commit {
        let title = text.trim().to_owned();
        if !title.is_empty() {
            c.state.rpc(
                m::SURFACE_RENAME,
                serde_json::json!({ "surface": surface.id, "title": title }),
            );
        }
        c.state.renaming = None;
    } else if cancel {
        c.state.renaming = None;
    } else if let Some(r) = c.state.renaming.as_mut() {
        r.text = text;
        r.focus_requested = true;
    }
}

fn draw_actions(ui: &mut Ui, rect: Rect, group_left: f32, count: usize, pane: &Pane, c: &mut Ctx) {
    let p = ui.painter_at(rect);
    for (i, name) in ACTIONS.iter().take(count).enumerate() {
        let center = egui::pos2(group_left + ACTION_STEP * (i as f32 + 0.5), rect.center().y);
        let hit = Rect::from_center_size(center, Vec2::splat(20.0));
        let resp = ui.interact(hit, Id::new(("paneaction", &pane.id, *name)), Sense::click());
        let color = if resp.hovered() { theme::TEXT } else { theme::DIM };
        let icon = Rect::from_center_size(center, Vec2::splat(theme::ICON));
        match *name {
            "plus" => icons::plus(&p, icon, color),
            "browser" => icons::globe(&p, icon, color),
            "split_right" => icons::split(&p, icon, color, false),
            "split_down" => icons::split(&p, icon, color, true),
            _ => icons::ellipsis(&p, icon, color),
        }

        if resp.clicked() {
            match *name {
                "plus" => c.state.new_terminal_chip(&pane.id),
                "browser" => c.state.new_browser_chip(&pane.id),
                "split_right" => split(c, &pane.id, "right"),
                "split_down" => split(c, &pane.id, "down"),
                _ => {}
            }
        }

        if *name == "ellipsis" {
            let mut right = false;
            let mut down = false;
            resp.context_menu(|ui| {
                ui.set_min_width(140.0);
                if ui.button("Split right").clicked() {
                    right = true;
                    ui.close();
                }
                if ui.button("Split down").clicked() {
                    down = true;
                    ui.close();
                }
            });
            if right {
                split(c, &pane.id, "right");
            } else if down {
                split(c, &pane.id, "down");
            }
        }
    }
}

fn split(c: &mut Ctx, pane: &str, side: &str) {
    c.state.rpc(
        m::SPLIT_NEW,
        serde_json::json!({ "side": side, "type": "terminal", "pane": pane }),
    );
}
