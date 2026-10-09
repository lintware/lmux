//! Sidebar: top icon strip, folder rows, workspace cards.

use egui::{
    Align2, Color32, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2,
};
use lmux_proto::{AgentState, Folder, Ref, Workspace};

use super::Ctx;
use crate::state::{CreateKind, CreateTarget, DragItem, SidebarRow, SidebarRowKind};
use crate::theme;
use crate::util::{ellipsize, tilde, wrap_lines};

use super::icons::{self, Arrow};

const CARD_RADIUS: u8 = 10;
const PAD_X: f32 = 10.0;
const PAD_Y: f32 = 7.0;
const LINE: f32 = 15.0;
/// How far each nesting level of a folder shifts its row right.
const INDENT: f32 = 13.0;
const ROW_H: f32 = 26.0;

pub fn show(ui: &mut Ui, rect: Rect, c: &mut Ctx) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, theme::SIDEBAR_BG);

    let strip = Rect::from_min_size(rect.min, Vec2::new(rect.width(), theme::TOP_STRIP_H));
    top_icons(ui, strip, c);

    let list = Rect::from_min_max(egui::pos2(rect.min.x, strip.max.y), rect.max);
    // Rows are hit-tested against the area they were drawn in, so a drop while
    // the pointer is over the panes cannot land in the sidebar.
    c.state.sidebar_list_rect = Some(list);
    body(ui, list, c);
}

fn top_icons(ui: &mut Ui, rect: Rect, c: &mut Ctx) {
    let p = ui.painter_at(rect);
    let y = rect.center().y;
    let start = rect.min.x + 12.0 + theme::titlebar_inset();
    let step = 23.0;

    let unread = c.state.unread_total();

    let items: [(&str, f32); 5] = [
        ("sidebar", start),
        ("bell", start + step),
        ("plus", start + step * 2.0),
        ("back", start + step * 3.0),
        ("forward", start + step * 4.0),
    ];

    for (name, x) in items {
        let hit = Rect::from_center_size(egui::pos2(x, y), Vec2::splat(24.0));
        let resp = ui.interact(hit, Id::new(("sb", name)), Sense::click());
        let color = if resp.hovered() { theme::TEXT } else { theme::DIM };
        let icon = Rect::from_center_size(hit.center(), Vec2::splat(theme::ICON));
        match name {
            "sidebar" => icons::sidebar_toggle(&p, icon, color),
            "bell" => icons::bell(&p, icon, color),
            "plus" => icons::plus(&p, icon, color),
            "back" => {
                let col = if c.state.can_go_back() { color } else { theme::DIM_2 };
                icons::arrow(&p, icon, col, Arrow::Left);
            }
            "forward" => {
                let col = if c.state.can_go_forward() { color } else { theme::DIM_2 };
                icons::arrow(&p, icon, col, Arrow::Right);
            }
            _ => {}
        }
        if resp.clicked() {
            match name {
                "sidebar" => c.state.sidebar_hidden = true,
                "bell" => c.state.notif_open = !c.state.notif_open,
                "plus" => {
                    let folder = c.state.default_new_folder();
                    c.state.new_session(folder);
                }
                "back" => c.state.navigate(-1),
                "forward" => c.state.navigate(1),
                _ => {}
            }
        }
        if name == "plus" {
            // Right click on `+` asks where the new session or folder goes.
            new_item_menu(&resp, c);
        }
        if name == "bell" && unread > 0 {
            let badge = egui::pos2(x + 8.0, y - 8.0);
            p.circle_filled(badge, 6.0, theme::BADGE);
            p.text(
                badge,
                Align2::CENTER_CENTER,
                unread.to_string(),
                theme::ui(9.0),
                Color32::WHITE,
            );
        }
    }
}

fn body(ui: &mut Ui, rect: Rect, c: &mut Ctx) {
    let builder = UiBuilder::new()
        .max_rect(rect)
        .layout(egui::Layout::top_down(egui::Align::Min));
    ui.scope_builder(builder, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt("sidebar")
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let folders = c.state.tree.folders.clone();
                let workspaces = c.state.tree.workspaces.clone();
                let selected = c.state.selected.clone();

                if workspaces.is_empty() && folders.is_empty() {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("no workspaces")
                            .font(theme::ui(theme::FS_SMALL))
                            .color(theme::DIM_2),
                    );
                    return;
                }

                folder_level(ui, &folders, &workspaces, None, 0, selected.as_deref(), c);

                // Workspaces without a folder.
                for ws in workspaces.iter().filter(|w| w.folder.is_none()) {
                    ws_card(ui, ws, selected.as_deref() == Some(ws.id.as_str()), c);
                }
                ui.add_space(8.0);

                // The leftover space under the cards: a double click starts a
                // new session beside the selected one, and a right click asks
                // which folder a session or folder belongs in. Clamped to the
                // visible body so a card that scrolls past the fold cannot be
                // hit.
                let rest = ui.available_rect_before_wrap().intersect(rect);
                if rest.width() > 4.0 && rest.height() > 4.0 {
                    let resp = ui.interact(rest, Id::new("sidebar-empty"), Sense::click());
                    if resp.double_clicked() {
                        let folder = c.state.default_new_folder();
                        c.state.new_session(folder);
                    }
                    new_item_menu(&resp, c);
                }
            });
    });
}

/// One level of the folder tree, then the workspaces each folder holds.
/// Collapsed folders are not walked, so their sessions stay hidden too.
#[allow(clippy::too_many_arguments)]
fn folder_level(
    ui: &mut Ui,
    folders: &[Folder],
    workspaces: &[Workspace],
    parent: Option<&str>,
    depth: usize,
    selected: Option<&str>,
    c: &mut Ctx,
) {
    for folder in folders.iter().filter(|f| f.parent.as_deref() == parent) {
        let collapsed = c.state.collapsed.contains(&folder.id) || folder.collapsed;
        let has_children = folders.iter().any(|f| f.parent.as_deref() == Some(folder.id.as_str()));
        if folder_row(ui, folder, depth, has_children, collapsed, c) {
            continue;
        }
        folder_level(ui, folders, workspaces, Some(&folder.id), depth + 1, selected, c);
        for ws in workspaces
            .iter()
            .filter(|w| w.folder.as_deref() == Some(folder.id.as_str()))
        {
            ws_card(ui, ws, selected == Some(ws.id.as_str()), c);
        }
    }
}

/// A folder row. Returns true when the folder is collapsed, so its contents
/// (nested folders and its own sessions) are skipped.
fn folder_row(
    ui: &mut Ui,
    folder: &Folder,
    depth: usize,
    has_children: bool,
    collapsed: bool,
    c: &mut Ctx,
) -> bool {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::click_and_drag());
    c.state.folder_rects.insert(folder.id.clone(), rect);
    c.state.sidebar_rows.push(SidebarRow {
        id: folder.id.clone(),
        kind: SidebarRowKind::Folder,
        rect,
        in_folder: folder.parent.clone(),
    });
    let painter = ui.painter_at(rect);
    let inner = rect.shrink2(Vec2::new(theme::SIDEBAR_PAD, 0.0));
    let lead = inner.min.x + depth as f32 * INDENT;
    let cy = rect.center().y;
    let color = if resp.hovered() { theme::TEXT } else { theme::TEXT_SOFT };

    if has_children {
        icons::chevron(
            &painter,
            egui::pos2(lead + 5.0, cy),
            3.4,
            theme::DIM,
            if collapsed { Arrow::Right } else { Arrow::Down },
        );
    }
    icons::folder(
        &painter,
        Rect::from_center_size(egui::pos2(lead + 21.0, cy), Vec2::splat(13.0)),
        theme::DIM,
    );
    let text_x = lead + 34.0;
    let avail = (inner.max.x - text_x - 4.0).max(10.0);

    let editing = c.state.folder_rename.as_ref().is_some_and(|r| r.folder == folder.id);
    if editing {
        folder_rename_editor(ui, rect, text_x, avail, folder, c);
    } else {
        let label = ellipsize(ui.ctx(), &folder.name, &theme::ui_semibold(theme::FS_BODY), avail, false);
        painter.text(
            egui::pos2(text_x, cy),
            Align2::LEFT_CENTER,
            label,
            theme::ui_semibold(theme::FS_BODY),
            color,
        );
    }

    if resp.double_clicked() {
        // Double click edits the name, like a chip's. egui reports `clicked`
        // for the pair's first click, which flipped the row, so take that flip
        // back: an edit must not hide the folder's own cards.
        let window = ui.ctx().options(|o| o.input_options.max_double_click_delay);
        let now = ui.input(|i| i.time);
        c.state.take_back_folder_toggle(&folder.id, window, now);
        c.state.begin_folder_rename(&folder.id, &folder.name);
        return collapsed;
    }
    if resp.clicked() {
        c.state.toggle_folder(&folder.id, ui.input(|i| i.time));
    }

    // The row's own `+`: a session in this folder, without leaving the row.
    // Registered after the row, so it takes the click inside its own rect, and
    // hidden while a row is being dragged so it cannot read as a drop target.
    let plus = Rect::from_center_size(egui::pos2(inner.max.x - 12.0, cy), Vec2::splat(18.0));
    c.state.folder_plus_rects.insert(folder.id.clone(), plus);
    let plus_resp = ui.interact(plus, Id::new(("folderplus", &folder.id)), Sense::click());
    if c.state.tab_drag.is_none() && (resp.hovered() || plus_resp.hovered()) {
        let color = if plus_resp.hovered() { theme::TEXT } else { theme::DIM_2 };
        icons::plus(&painter, Rect::from_center_size(plus.center(), Vec2::splat(12.0)), color);
    }
    if plus_resp.clicked() {
        c.state.new_session(Some(folder.id.clone()));
        return collapsed;
    }

    if resp.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or_else(|| rect.center());
        c.state.begin_tab_drag(
            DragItem::Folder(folder.id.clone()),
            "",
            origin - rect.min,
            rect.size(),
            origin,
        );
        ui.ctx().request_repaint();
        return collapsed;
    }

    folder_menu(&resp, folder, c);
    collapsed
}

/// The folder row's own menu: everything acts on this folder.
fn folder_menu(resp: &egui::Response, folder: &Folder, c: &mut Ctx) {
    let mut new_session = false;
    let mut new_child = false;
    let mut rename = false;
    let mut close = false;
    resp.context_menu(|ui| {
        ui.set_min_width(160.0);
        if ui.button("New Session Here").clicked() {
            new_session = true;
            ui.close();
        }
        if ui.button("New Folder Inside").clicked() {
            new_child = true;
            ui.close();
        }
        ui.separator();
        if ui.button("Rename Folder").clicked() {
            rename = true;
            ui.close();
        }
        if ui.button("Close Folder").clicked() {
            close = true;
            ui.close();
        }
    });
    if new_session {
        c.state.new_session(Some(folder.id.clone()));
    } else if new_child {
        c.state.new_folder(Some(folder.id.clone()));
    } else if rename {
        c.state.begin_folder_rename(&folder.id, &folder.name);
    } else if close {
        c.state.rpc(lmux_proto::methods::FOLDER_CLOSE, serde_json::json!({ "folder": folder.id }));
    }
}

/// The menu for the empty sidebar and the `+`: a submenu per host, so a session
/// can be made on this machine or on any remote connection, in any of that
/// host's folders.
fn new_item_menu(resp: &egui::Response, c: &mut Ctx) {
    let targets = c.state.create_targets(c.net.hosts(), c.net.host_index());

    let mut session: Option<(usize, Option<String>)> = None;
    let mut folder: Option<(usize, Option<String>)> = None;
    resp.context_menu(|ui| {
        ui.set_min_width(200.0);
        let mut close = false;
        for target in &targets {
            if host_submenu(ui, &format!("New Session on {}", target.host), "", target, &mut session) {
                close = true;
            }
        }
        ui.separator();
        for target in &targets {
            if host_submenu(ui, &format!("New Folder on {}", target.host), "Inside ", target, &mut folder)
            {
                close = true;
            }
        }
        if close {
            ui.close();
        }
    });

    let pick = match (session, folder) {
        (Some(pick), _) => (CreateKind::Session, pick),
        (_, Some(pick)) => (CreateKind::Folder, pick),
        (None, None) => return,
    };
    let Some(target) = targets.get(pick.1 .0) else {
        return;
    };
    c.state.create_in(target, pick.0, pick.1 .1.clone());
}

/// `New Session on <host> ▸ Top Level / <folder trail>…`. Returns true when one
/// of its entries was clicked, so the caller can close the outer menu too.
fn host_submenu(
    ui: &mut Ui,
    label: &str,
    item_prefix: &str,
    target: &CreateTarget,
    picked: &mut Option<(usize, Option<String>)>,
) -> bool {
    let index = target.index;
    let folders = target.folders.clone();
    let prefix = item_prefix.to_owned();
    let mut choice: Option<Option<String>> = None;
    let resp = ui.menu_button(label, |ui| {
        ui.set_min_width(180.0);
        if ui.button("Top Level").clicked() {
            choice = Some(None);
            ui.close();
        }
        for (id, trail) in &folders {
            if ui.button(format!("{prefix}{trail}")).clicked() {
                choice = Some(Some(id.clone()));
                ui.close();
            }
        }
        choice.is_some()
    });
    if let Some(chosen) = choice {
        *picked = Some((index, chosen));
        return true;
    }
    resp.inner == Some(true)
}

/// The inline editor a freshly created (or renamed) folder gets.
fn folder_rename_editor(
    ui: &mut Ui,
    row: Rect,
    text_x: f32,
    width: f32,
    folder: &Folder,
    c: &mut Ctx,
) {
    let Some(ren) = c.state.folder_rename.clone() else { return };
    let mut text = ren.text;
    let mut commit = false;
    let mut cancel = false;

    let edit_rect = Rect::from_min_max(
        egui::pos2(text_x - 2.0, row.min.y + 3.0),
        egui::pos2(text_x - 2.0 + width, row.max.y - 3.0),
    );
    let edit_id = Id::new(("folder-rename", &folder.id));
    ui.scope_builder(
        UiBuilder::new().max_rect(edit_rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            ui.style_mut().visuals.extreme_bg_color = Color32::from_rgb(0x2A, 0x2A, 0x2A);
            ui.style_mut().visuals.selection.bg_fill = theme::ACCENT;
            let out = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .id(edit_id)
                    .font(theme::ui_semibold(theme::FS_BODY))
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
        let name = text.trim().to_owned();
        if !name.is_empty() {
            c.state.rpc(
                lmux_proto::methods::FOLDER_RENAME,
                serde_json::json!({ "folder": folder.id, "name": name }),
            );
        }
        c.state.folder_rename = None;
    } else if cancel {
        c.state.folder_rename = None;
    } else if let Some(r) = c.state.folder_rename.as_mut() {
        r.text = text;
        r.focus_requested = true;
    }
}

fn ws_card(ui: &mut Ui, ws: &Workspace, selected: bool, c: &mut Ctx) {
    let w = ui.available_width();
    let h = card_height(ui.ctx(), ws, w - 2.0 * theme::SIDEBAR_PAD, selected);
    let (row, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click_and_drag());
    let rect = row.shrink2(Vec2::new(theme::SIDEBAR_PAD, 3.0));
    c.state.card_rects.insert(ws.id.clone(), rect);
    c.state.sidebar_rows.push(SidebarRow {
        id: ws.id.clone(),
        kind: SidebarRowKind::Workspace,
        rect,
        in_folder: ws.folder.clone(),
    });

    let p = ui.painter_at(rect);
    let radius = CornerRadius::same(CARD_RADIUS);
    if selected {
        p.rect_filled(rect, radius, theme::CARD_SELECTED);
        // Thin, brighter bar at the left edge (as in the reference).
        let bar = Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height()));
        p.rect_filled(bar, radius, Color32::from_rgb(0x66, 0xB6, 0xFF));
    } else if resp.hovered() {
        p.rect_filled(rect, radius, Color32::from_white_alpha(8));
    }

    let title_color = if selected { Color32::WHITE } else { theme::TEXT };
    let body_color = if selected {
        Color32::from_rgb(0xDC, 0xE9, 0xFB)
    } else {
        theme::DIM
    };
    let faint = if selected {
        Color32::from_rgb(0xC3, 0xD7, 0xF2)
    } else {
        theme::DIM_2
    };

    let x = rect.min.x + PAD_X;
    let max_w = rect.width() - 2.0 * PAD_X;
    let mut y = rect.min.y + PAD_Y;

    // Title.
    p.text(
        egui::pos2(x, y),
        Align2::LEFT_TOP,
        ellipsize(ui.ctx(), &ws.title, &theme::ui_semibold(theme::FS_TITLE), max_w - 18.0, false),
        theme::ui_semibold(theme::FS_TITLE),
        title_color,
    );
    y += 17.0;

    // Summary (up to three lines).
    if let Some(summary) = ws.summary.as_deref().filter(|s| !s.is_empty()) {
        for line in wrap_lines(ui.ctx(), summary, &theme::ui(theme::FS_SMALL), max_w, 3) {
            p.text(
                egui::pos2(x, y),
                Align2::LEFT_TOP,
                ellipsize(ui.ctx(), &line, &theme::ui(theme::FS_SMALL), max_w, false),
                theme::ui(theme::FS_SMALL),
                body_color,
            );
            y += LINE;
        }
        y += 2.0;
    }

    // Agent state line.
    if let Some(color) = agent_color(ws.agent) {
        icons::dot(&p, egui::pos2(x + 3.0, y + 6.0), 3.0, color);
        p.text(
            egui::pos2(x + 12.0, y),
            Align2::LEFT_TOP,
            agent_label(ws.agent),
            theme::ui(theme::FS_SMALL),
            if selected { title_color } else { theme::TEXT_SOFT },
        );
        if let Some(branch) = ws.git_branch.as_deref() {
            let bx = x + 12.0 + 68.0;
            icons::branch(&p, Rect::from_center_size(egui::pos2(bx + 5.0, y + 7.0), Vec2::splat(12.0)), faint);
            p.text(
                egui::pos2(bx + 14.0, y),
                Align2::LEFT_TOP,
                ellipsize(ui.ctx(), branch, &theme::ui(theme::FS_SMALL), (rect.max.x - 10.0) - (bx + 14.0), false),
                theme::ui(theme::FS_SMALL),
                faint,
            );
        }
        y += LINE;
    }

    // cwd, middle-truncated.
    if !ws.cwd.is_empty() {
        let cwd = tilde(&ws.cwd);
        p.text(
            egui::pos2(x, y),
            Align2::LEFT_TOP,
            ellipsize(ui.ctx(), &cwd, &theme::mono(theme::FS_TINY), max_w, true),
            theme::mono(theme::FS_TINY),
            faint,
        );
        y += LINE - 1.0;
    }

    // Status pills.
    if !ws.status_pills.is_empty() {
        y += 2.0;
        let mut px = x;
        for pill in &ws.status_pills {
            let color = pill
                .color
                .as_deref()
                .map(|h| super::parse_hex(h, theme::DIM))
                .unwrap_or(theme::DIM);
            let label = match (&pill.icon, pill.value.is_empty()) {
                (Some(icon), false) => format!("{icon} {}", pill.value),
                (Some(icon), true) => icon.clone(),
                (None, _) => pill.value.clone(),
            };
            let tw = crate::util::measure(ui.ctx(), &label, &theme::ui(theme::FS_TINY));
            let chip = Rect::from_min_size(egui::pos2(px, y), Vec2::new(tw + 12.0, 15.0));
            if chip.max.x > rect.max.x - PAD_X {
                break;
            }
            let border = if selected { Color32::from_white_alpha(90) } else { theme::HAIRLINE };
            p.rect_stroke(chip, CornerRadius::same(7), Stroke::new(1.0, border), StrokeKind::Inside);
            p.text(
                chip.center(),
                Align2::CENTER_CENTER,
                label,
                theme::ui(theme::FS_TINY),
                if selected { Color32::WHITE } else { color },
            );
            px = chip.max.x + 4.0;
        }
    }

    // Progress bar across the bottom.
    if let Some(progress) = &ws.progress {
        let bar = Rect::from_min_size(
            egui::pos2(rect.min.x + PAD_X, rect.max.y - 6.0),
            Vec2::new(rect.width() - 2.0 * PAD_X, 3.0),
        );
        let track = if selected { Color32::from_white_alpha(60) } else { theme::HAIRLINE };
        p.rect_filled(bar, CornerRadius::same(2), track);
        let filled = Rect::from_min_size(bar.min, Vec2::new(bar.width() * progress.value.clamp(0.0, 1.0), bar.height()));
        p.rect_filled(filled, CornerRadius::same(2), if selected { Color32::WHITE } else { theme::ACCENT });
    }

    // Unread badge, top right.
    if ws.unread_notifications > 0 {
        let badge = egui::pos2(rect.max.x - 12.0, rect.min.y + 11.0);
        p.circle_filled(badge, 8.0, theme::BADGE);
        p.text(
            badge,
            Align2::CENTER_CENTER,
            ws.unread_notifications.to_string(),
            theme::ui(theme::FS_TINY),
            Color32::WHITE,
        );
    }

    // Close button, beside the badge: hidden until the card is pointed at, like
    // the row controls in the reference. It is registered even while hidden,
    // because egui hands the pointer to the topmost widget and the card would
    // otherwise keep the hover that is supposed to reveal it.
    let close_center = egui::pos2(
        rect.max.x - if ws.unread_notifications > 0 { 30.0 } else { 12.0 },
        rect.min.y + 11.0,
    );
    let close_resp = ui.interact(
        Rect::from_center_size(close_center, Vec2::splat(18.0)),
        Id::new(("cardx", &ws.id)),
        Sense::click(),
    );
    if c.state.tab_drag.is_none() && (resp.hovered() || close_resp.hovered()) {
        let color = if close_resp.hovered() { theme::RED } else { theme::DIM_2 };
        icons::close(&p, Rect::from_center_size(close_center, Vec2::splat(9.0)), color);
    }
    if close_resp.clicked() {
        c.state.rpc(
            lmux_proto::methods::WORKSPACE_CLOSE,
            serde_json::json!({ "workspace": ws.id }),
        );
        return;
    }

    if resp.double_clicked() {
        // Double clicking a card selects it and starts a new session next to it,
        // in the same folder — the sidebar's double click always means "new
        // session here".
        c.state.select_workspace(&ws.id);
        c.state.new_session(ws.folder.clone());
        return;
    }
    if resp.clicked() {
        c.state.select_workspace(&ws.id);
    }
    if resp.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or_else(|| rect.center());
        c.state.begin_tab_drag(
            DragItem::Workspace(ws.id.clone()),
            "",
            origin - rect.min,
            rect.size(),
            origin,
        );
        ui.ctx().request_repaint();
        return;
    }
    card_menu(&resp, ws, c);
}

/// A card's menu: everything acts on this session. `Move to Folder` lists every
/// folder by its trail, plus the top level, because dragging a card out of a
/// deep folder to the sidebar's empty tail is a long haul.
fn card_menu(resp: &egui::Response, ws: &Workspace, c: &mut Ctx) {
    // "Top Level" only appears when the session is somewhere else, so no entry
    // ever says what the session already is.
    let targets: Vec<(Option<Ref>, String)> = ws
        .folder
        .is_some()
        .then(|| (None, "Top Level".to_owned()))
        .into_iter()
        .chain(
            c.state
                .tree
                .folders
                .iter()
                .filter(|f| ws.folder.as_deref() != Some(f.id.as_str()))
                .map(|f| (Some(f.id.clone()), c.state.folder_path(&f.id))),
        )
        .collect();

    let mut new_session = false;
    let mut close = false;
    let mut move_to: Option<Option<Ref>> = None;
    resp.context_menu(|ui| {
        ui.set_min_width(170.0);
        if ui.button("New Session Here").clicked() {
            new_session = true;
            ui.close();
        }
        ui.menu_button("Move to Folder", |ui| {
            ui.set_min_width(180.0);
            for (target, trail) in &targets {
                // The label names the target, so two entries never read alike.
                let label = match target {
                    Some(_) => trail.clone(),
                    None => "Top Level".to_owned(),
                };
                if ui.button(label).clicked() {
                    move_to = Some(target.clone());
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.button("Close Session").clicked() {
            close = true;
            ui.close();
        }
    });

    if new_session {
        c.state.new_session(ws.folder.clone());
    } else if let Some(folder) = move_to {
        c.state.rpc(
            lmux_proto::methods::WORKSPACE_MOVE,
            serde_json::json!({ "workspace": ws.id, "folder": folder }),
        );
    } else if close {
        c.state.rpc(lmux_proto::methods::WORKSPACE_CLOSE, serde_json::json!({ "workspace": ws.id }));
    }
}

fn card_height(ctx: &egui::Context, ws: &Workspace, inner_w: f32, selected: bool) -> f32 {
    let _ = (ctx, selected);
    let mut h = PAD_Y + 17.0; // title
    if ws.summary.as_deref().is_some_and(|s| !s.is_empty()) {
        let lines = wrap_lines(ctx, ws.summary.as_deref().unwrap_or(""), &theme::ui(theme::FS_SMALL), inner_w, 3).len();
        h += lines as f32 * LINE + 2.0;
    }
    if agent_color(ws.agent).is_some() {
        h += LINE;
    }
    if !ws.cwd.is_empty() {
        h += LINE - 1.0;
    }
    if !ws.status_pills.is_empty() {
        h += 19.0;
    }
    if ws.progress.is_some() {
        h += 8.0;
    }
    h += PAD_Y + 3.0;
    h.max(30.0)
}

fn agent_color(state: AgentState) -> Option<Color32> {
    match state {
        AgentState::None => None,
        AgentState::Running => Some(theme::GREEN),
        AgentState::Idle => Some(theme::ACCENT),
        AgentState::NeedsInput => Some(theme::AMBER),
    }
}

fn agent_label(state: AgentState) -> &'static str {
    match state {
        AgentState::None => "",
        AgentState::Running => "Running",
        AgentState::Idle => "Idle",
        AgentState::NeedsInput => "Needs input",
    }
}
