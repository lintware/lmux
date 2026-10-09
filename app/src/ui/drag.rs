//! Drag-and-drop for surface chips, pane headers and sidebar rows.
//!
//! The gesture runs in two halves. The drag *starts* inside a chip, a pane strip
//! or a sidebar row (`tabstrip.rs`, `sidebar.rs`), which stores a [`TabDrag`] in
//! [`AppState`]. Each frame [`show`] then updates the pointer, resolves the
//! [`DropTarget`] against the geometry that drawing just recorded (`pane_rects` /
//! `chip_rects` for the panes, `sidebar_rows` for the sidebar), paints the
//! feedback, and on release issues the RPC the target implies. Because the tree
//! is never patched locally, the result is whatever tree the daemon sends back.

use egui::{Align2, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, Ui};

use super::Ctx;
use crate::state::{
    folder_holds, folder_position, insertion_slot, workspace_position, AppState, DragItem, DropTarget,
    SidebarRowKind, TabDrag,
};
use crate::theme;

/// Fraction of a pane's width/height that counts as its edge, for a split drop.
const EDGE_BAND: f32 = 0.25;

/// How much of a folder row's height takes a folder *beside* it rather than
/// *inside* it. The rest is an adopt zone, so dropping on a folder's row always
/// means "in here" unless the pointer is at its very top.
const FOLDER_SIBLING_BAND: f32 = 0.3;

/// Height of the ghost a dragged sidebar row carries.
const SIDEBAR_GHOST_H: f32 = 26.0;

/// Drive a drag: update the pointer, resolve the target, paint, and apply on
/// release. Called once per frame after every pane has been drawn.
pub fn show(ui: &mut Ui, c: &mut Ctx) {
    let Some(drag) = c.state.tab_drag.clone() else {
        c.state.tab_drop = None;
        return;
    };

    let (pointer, down) = ui.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));

    if !down {
        // Released anywhere — over a target, the source, empty space, or outside
        // the window. Resolve against where the pointer actually is; when there
        // is no valid target the drop is a no-op.
        if let Some(p) = pointer {
            if let Some(target) = resolve(&c.state, &drag, p) {
                c.state.apply_drop(target);
            }
        }
        c.state.cancel_tab_drag();
        ui.ctx().request_repaint();
        return;
    }

    if let Some(p) = pointer {
        if let Some(d) = c.state.tab_drag.as_mut() {
            d.pointer = p;
        }
    }
    let drag = c.state.tab_drag.clone().unwrap_or(drag);

    c.state.tab_drop = resolve(&c.state, &drag, drag.pointer);
    paint(ui, &c.state, &drag);
    // The ghost and the drop indicator must keep up with the pointer.
    ui.ctx().request_repaint();
}

/// What the dragged item would do if released at `pointer`, or `None` for a
/// no-op (nothing under the pointer, the source itself, or its current slot).
fn resolve(state: &AppState, drag: &TabDrag, pointer: Pos2) -> Option<DropTarget> {
    match &drag.item {
        DragItem::Surface(_) | DragItem::Pane(_) => resolve_pane_target(state, drag, pointer),
        DragItem::Workspace(_) | DragItem::Folder(_) => resolve_sidebar_target(state, drag, pointer),
    }
}

/// Where a sidebar row lands: inside a folder row, between two rows of the same
/// container, or at the end of the list. Everything is measured against the rows
/// that were drawn this frame, so the painted line and the RPC agree.
fn resolve_sidebar_target(state: &AppState, drag: &TabDrag, pointer: Pos2) -> Option<DropTarget> {
    let list = state.sidebar_list_rect?;
    if !list.contains(pointer) {
        return None;
    }
    let dragged = match &drag.item {
        DragItem::Workspace(id) | DragItem::Folder(id) => id.as_str(),
        _ => return None,
    };

    // The row under the pointer, or the nearest gap when the pointer is past the
    // ends of the list: clamping keeps a drop near an edge meaningful instead of
    // resolving to nothing.
    let row = match state.sidebar_rows.iter().find(|r| r.rect.contains(pointer)) {
        Some(row) => row,
        None => {
            let rows = &state.sidebar_rows;
            let last = rows.last()?;
            if pointer.y > last.rect.max.y {
                // Below every row: the end of the list, which is the end of the
                // top level. Not "after the last row's own container": the last
                // row may sit inside a folder, and a drop in open space must not
                // silently join that folder.
                return Some(DropTarget::Sidebar {
                    into: None,
                    slot: None,
                    adopt: None,
                    line_y: Some(last.rect.max.y),
                });
            }
            rows.first()?
        }
    };
    let below = pointer.y >= row.rect.center().y;

    match (drag.item.item_is_folder(), row.kind) {
        // A folder over a folder row: the top band puts it beside that folder
        // (which is how a folder comes back out of another folder), the rest
        // nests it inside.
        (true, SidebarRowKind::Folder) => {
            let band = (pointer.y - row.rect.min.y) / row.rect.height().max(1.0);
            if band < FOLDER_SIBLING_BAND {
                let parent = row.in_folder.clone();
                if folder_holds(&state.tree, dragged, parent.as_deref()) {
                    return None;
                }
                let target = folder_position(&state.tree, &row.id, parent.as_deref())?;
                let own = folder_position(&state.tree, dragged, parent.as_deref());
                let slot = insertion_slot(target, own)?;
                Some(DropTarget::Sidebar {
                    into: parent,
                    slot: Some(slot),
                    adopt: None,
                    line_y: Some(row.rect.min.y),
                })
            } else {
                adopt_folder(&state.tree, dragged, &row.id)
            }
        }
        // A session over a folder row goes into that folder: the whole row is an
        // adopt zone, even when the folder is collapsed and has no visible gap.
        (false, SidebarRowKind::Folder) => adopt_folder(&state.tree, dragged, &row.id),
        // A session over another session: before it or after it, in its folder.
        (false, SidebarRowKind::Workspace) => {
            let folder = row.in_folder.clone();
            let target = workspace_position(&state.tree, &row.id, folder.as_deref())?
                + usize::from(below);
            let own = workspace_position(&state.tree, dragged, folder.as_deref());
            let slot = insertion_slot(target, own)?;
            Some(DropTarget::Sidebar {
                into: folder,
                slot: Some(slot),
                adopt: None,
                line_y: Some(if below { row.rect.max.y } else { row.rect.min.y }),
            })
        }
        // A folder over a session row joins the session's folder, after it. The
        // session gives the line; the folder takes the position the daemon can
        // express, which is the end of that folder's child folders.
        (true, SidebarRowKind::Workspace) => {
            let parent = row.in_folder.clone();
            if folder_holds(&state.tree, dragged, parent.as_deref()) {
                return None;
            }
            Some(DropTarget::Sidebar {
                into: parent.clone(),
                slot: None,
                adopt: parent,
                line_y: None,
            })
        }
    }
}

/// A drop that puts a row inside a folder: the folder row is filled, and the row
/// goes to the front of that folder's own rows. A folder cannot be dropped into
/// its own subtree, so that case resolves to nothing.
fn adopt_folder(tree: &lmux_proto::Tree, dragged: &str, folder: &str) -> Option<DropTarget> {
    if folder_holds(tree, dragged, Some(folder)) {
        return None;
    }
    Some(DropTarget::Sidebar {
        into: Some(folder.to_owned()),
        slot: Some(0),
        adopt: Some(folder.to_owned()),
        line_y: None,
    })
}

/// The pane a chip or pane header would land in, by the geometry the pane pass
/// recorded.
fn resolve_pane_target(state: &AppState, drag: &TabDrag, pointer: Pos2) -> Option<DropTarget> {
    let (pane, rect) = hovered_pane(state, pointer)?;
    let pane_id = pane.id.clone();

    match &drag.item {
        DragItem::Surface(surface) => {
            let strip_bottom = rect.min.y + theme::PANE_STRIP_H;
            if pointer.y < strip_bottom {
                // Over a strip: reorder within the pane, or insert into another
                // pane. `index` is the slot in the target's list (the dragged
                // chip removed, so same-pane reorder is not off by one).
                let index = insert_index(state, pane, pointer.x, surface);
                if drag.source_pane == pane_id
                    && Some(index) == filtered_index(pane, surface)
                {
                    return None;
                }
                return Some(DropTarget::Move { pane: pane_id, index: Some(index) });
            }
            let content =
                Rect::from_min_max(Pos2::new(rect.min.x, strip_bottom), rect.max);
            if let Some(side) = edge_side(content, pointer) {
                // The daemon splits the pane the tab lands in, so a pane holding
                // only the dragged tab has nothing left to divide.
                let splittable = state
                    .pane(&pane_id)
                    .is_some_and(|pn| pn.surfaces.len() >= 2 || drag.source_pane != pane_id);
                if splittable {
                    return Some(DropTarget::Split { pane: pane_id, side });
                }
            }
            if drag.source_pane == pane_id {
                // Dropping on the source pane's own body (away from an edge) is
                // not a reorder, so leave it alone.
                return None;
            }
            Some(DropTarget::Move { pane: pane_id, index: None })
        }
        DragItem::Pane(pane) => {
            if &pane_id == pane {
                return None;
            }
            Some(DropTarget::Swap { other: pane_id })
        }
        // Sidebar rows resolve against the sidebar, never against a pane.
        DragItem::Workspace(_) | DragItem::Folder(_) => None,
    }
}

/// The pane whose drawn rect contains `pointer`.
fn hovered_pane<'a>(
    state: &'a AppState,
    pointer: Pos2,
) -> Option<(&'a lmux_proto::Pane, Rect)> {
    let ws = state.selected_workspace()?;
    ws.panes.iter().find_map(|p| {
        state
            .pane_rects
            .get(&p.id)
            .filter(|r| r.contains(pointer))
            .map(|r| (p, *r))
    })
}

/// The slot `x` implies in `pane`'s list, with `dragged` removed.
fn insert_index(state: &AppState, pane: &lmux_proto::Pane, x: f32, dragged: &str) -> usize {
    let mut index = 0;
    for s in &pane.surfaces {
        if s.id == dragged {
            continue;
        }
        if let Some(r) = state.chip_rects.get(&s.id) {
            if x < r.center().x {
                return index;
            }
        }
        index += 1;
    }
    index
}

/// `dragged`'s own slot once it is removed from its pane's list.
fn filtered_index(pane: &lmux_proto::Pane, dragged: &str) -> Option<usize> {
    pane.surfaces.iter().position(|s| s.id == dragged)
}

/// Which edge of `rect` the pointer is inside, if any. The nearest edge wins.
fn edge_side(rect: Rect, pointer: Pos2) -> Option<&'static str> {
    let bx = rect.width() * EDGE_BAND;
    let by = rect.height() * EDGE_BAND;
    let mut best: Option<(&'static str, f32)> = None;
    let mut consider = |side, dist: f32, band: f32| {
        if dist >= 0.0 && dist <= band && best.is_none_or(|(_, d)| dist < d) {
            best = Some((side, dist));
        }
    };
    consider("left", pointer.x - rect.min.x, bx);
    consider("right", rect.max.x - pointer.x, bx);
    consider("up", pointer.y - rect.min.y, by);
    consider("down", rect.max.y - pointer.y, by);
    best.map(|(side, _)| side)
}

// --- painting ---------------------------------------------------------------

fn accent_alpha(a: u8) -> Color32 {
    let c = theme::ACCENT;
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

fn paint(ui: &mut Ui, state: &AppState, drag: &TabDrag) {
    let p = ui.painter().clone();

    // Dim what is being dragged.
    match &drag.item {
        DragItem::Surface(s) => {
            if let Some(r) = state.chip_rects.get(s) {
                p.rect_filled(*r, CornerRadius::ZERO, Color32::from_black_alpha(165));
            }
        }
        DragItem::Pane(id) => {
            if let Some(r) = state.pane_rects.get(id) {
                p.rect_filled(*r, CornerRadius::ZERO, Color32::from_black_alpha(140));
            }
        }
        DragItem::Workspace(id) => {
            if let Some(r) = state.card_rects.get(id) {
                p.rect_filled(*r, CornerRadius::same(6), Color32::from_black_alpha(150));
            }
        }
        DragItem::Folder(id) => {
            if let Some(r) = state.folder_rects.get(id) {
                p.rect_filled(*r, CornerRadius::same(4), Color32::from_black_alpha(150));
            }
        }
    }

    // The ghost that follows the pointer. A sidebar row drags as one row-sized
    // chip rather than as its whole card: a card-sized ghost would cover the
    // gap the line marks.
    let ghost = match &drag.item {
        DragItem::Workspace(_) | DragItem::Folder(_) => {
            let width = state
                .sidebar_list_rect
                .map(|r| r.width() - 20.0)
                .unwrap_or(drag.size.x);
            Rect::from_center_size(drag.pointer, egui::vec2(width, SIDEBAR_GHOST_H))
        }
        _ => Rect::from_min_size(drag.pointer - drag.grab, drag.size),
    };
    p.rect_filled(
        ghost,
        CornerRadius::same(4),
        Color32::from_rgba_unmultiplied(0x3A, 0x3A, 0x40, 240),
    );
    p.rect_stroke(
        ghost,
        CornerRadius::same(4),
        Stroke::new(1.0, theme::ACCENT),
        StrokeKind::Inside,
    );
    let label = match &drag.item {
        DragItem::Surface(s) => state.surface(s).map(|x| x.title.clone()),
        DragItem::Workspace(id) => state.workspace(id).map(|x| x.title.clone()),
        DragItem::Folder(id) => state.folder(id).map(|x| x.name.clone()),
        DragItem::Pane(id) => state.pane(id).and_then(|pn| {
            pn.surfaces
                .iter()
                .find(|s| Some(&s.id) == pn.active_surface.as_ref())
                .or_else(|| pn.surfaces.first())
                .map(|s| s.title.clone())
        }),
    };
    if let Some(label) = label {
        p.text(
            ghost.center(),
            Align2::CENTER_CENTER,
            crate::util::ellipsize(
                ui.ctx(),
                &label,
                &theme::ui(theme::FS_SMALL),
                ghost.width() - 10.0,
                false,
            ),
            theme::ui(theme::FS_SMALL),
            theme::TEXT,
        );
    }

    // The drop indicator is drawn last, so it stays visible over the ghost.
    if let Some(target) = &state.tab_drop {
        match target {
            DropTarget::Move { pane, index: Some(index) } => {
                if let Some(x) = caret_x(state, drag, pane, *index) {
                    if let Some(rect) = state.pane_rects.get(pane) {
                        p.line_segment(
                            [
                                Pos2::new(x, rect.min.y + 3.0),
                                Pos2::new(x, rect.min.y + theme::PANE_STRIP_H - 3.0),
                            ],
                            Stroke::new(2.0, theme::ACCENT),
                        );
                    }
                }
            }
            DropTarget::Move { pane, index: None } => highlight_pane(&p, state, pane),
            DropTarget::Swap { other } => highlight_pane(&p, state, other),
            DropTarget::Split { pane, side } => {
                // Drawn on the pane the daemon will split, so the shaded half is
                // where the tab actually lands.
                if let Some(rect) = state.pane_rects.get(pane) {
                    let rect = *rect;
                    highlight_pane_rect(&p, rect);
                    let content = Rect::from_min_max(
                        Pos2::new(rect.min.x, rect.min.y + theme::PANE_STRIP_H),
                        rect.max,
                    );
                    if let Some(half) = split_half(content, side) {
                        p.rect_filled(half, CornerRadius::ZERO, accent_alpha(60));
                        p.rect_stroke(
                            half,
                            CornerRadius::ZERO,
                            Stroke::new(1.5, theme::ACCENT),
                            StrokeKind::Inside,
                        );
                    }
                }
            }
            DropTarget::Sidebar { adopt, line_y, .. } => {
                // One of the two: a folder filled when the row goes inside it, or
                // a line at the gap the row will take. Never both, because a drop
                // resolves to a slot in a container or to a folder itself.
                if let Some(folder) = adopt {
                    if let Some(rect) = state.folder_rects.get(folder) {
                        let rect = *rect;
                        p.rect_filled(rect, CornerRadius::same(6), accent_alpha(40));
                        p.rect_stroke(
                            rect,
                            CornerRadius::same(6),
                            Stroke::new(1.5, theme::ACCENT),
                            StrokeKind::Inside,
                        );
                    }
                }
                if let (Some(y), Some(list)) = (line_y, state.sidebar_list_rect) {
                    p.line_segment(
                        [Pos2::new(list.min.x + 6.0, *y), Pos2::new(list.max.x - 6.0, *y)],
                        Stroke::new(2.0, theme::ACCENT),
                    );
                }
            }
        }
    }
}

/// The caret x position for inserting at `index` into `pane` (the dragged chip
/// removed from the sequence, when this is the source pane).
fn caret_x(state: &AppState, drag: &TabDrag, pane: &str, index: usize) -> Option<f32> {
    let p = state.pane(pane)?;
    let dragged = match &drag.item {
        DragItem::Surface(s) => Some(s.as_str()),
        DragItem::Pane(_) | DragItem::Workspace(_) | DragItem::Folder(_) => None,
    };
    let list: Vec<&lmux_proto::Surface> = p
        .surfaces
        .iter()
        .filter(|s| Some(s.id.as_str()) != dragged)
        .collect();
    if index < list.len() {
        state.chip_rects.get(&list[index].id).map(|r| r.min.x)
    } else {
        list.last()
            .and_then(|s| state.chip_rects.get(&s.id))
            .map(|r| r.max.x)
    }
}

fn highlight_pane(p: &egui::Painter, state: &AppState, pane: &str) {
    if let Some(rect) = state.pane_rects.get(pane) {
        highlight_pane_rect(p, *rect);
    }
}

fn highlight_pane_rect(p: &egui::Painter, rect: Rect) {
    p.rect_stroke(
        rect.shrink(1.0),
        CornerRadius::ZERO,
        Stroke::new(2.0, theme::ACCENT),
        StrokeKind::Inside,
    );
    p.rect_filled(rect, CornerRadius::ZERO, accent_alpha(28));
}

/// The half of `content` a split on `side` would take.
fn split_half(content: Rect, side: &str) -> Option<Rect> {
    let w = content.width() * 0.5;
    let h = content.height() * 0.5;
    Some(match side {
        "left" => Rect::from_min_max(content.min, Pos2::new(content.min.x + w, content.max.y)),
        "right" => Rect::from_min_max(Pos2::new(content.max.x - w, content.min.y), content.max),
        "up" => Rect::from_min_max(content.min, Pos2::new(content.max.x, content.min.y + h)),
        "down" => Rect::from_min_max(Pos2::new(content.min.x, content.max.y - h), content.max),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use egui::Vec2;
    use lmux_proto::{Layout, Pane, SplitDirection, Surface, SurfaceKind, Tree, Workspace};

    fn term(id: &str) -> Surface {
        Surface {
            id: id.into(),
            title: id.into(),
            kind: SurfaceKind::Terminal { tmux_pane: None, cols: 80, rows: 24 },
        }
    }

    /// Two panes side by side: pane:1 holds s1,s2; pane:2 holds s3.
    fn two_pane_state() -> AppState {
        let p1 = Pane {
            id: "pane:1".into(),
            surfaces: vec![term("s1"), term("s2")],
            active_surface: Some("s1".into()),
        };
        let p2 = Pane {
            id: "pane:2".into(),
            surfaces: vec![term("s3")],
            active_surface: Some("s3".into()),
        };
        let ws = Workspace {
            id: "workspace:1".into(),
            title: "w".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: lmux_proto::AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(Layout::Pane { pane: "pane:1".into() }),
                second: Box::new(Layout::Pane { pane: "pane:2".into() }),
            },
            panes: vec![p1, p2],
            focused_pane: Some("pane:1".into()),
            ports: vec![],
            pr: None,
        };
        let tree = Tree {
            host: "h".into(),
            folders: vec![],
            workspaces: vec![ws],
            selected_workspace: Some("workspace:1".into()),
        };
        let mut st = AppState::new();
        st.apply_tree(tree);
        st.selected = Some("workspace:1".into());
        // Panes: pane:1 on the left, pane:2 on the right.
        st.pane_rects.insert("pane:1".into(), Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(400.0, 400.0)));
        st.pane_rects.insert("pane:2".into(), Rect::from_min_max(Pos2::new(400.0, 0.0), Pos2::new(800.0, 400.0)));
        // Chips in pane:1: s1 [6,100], s2 [100,194].
        st.chip_rects.insert("s1".into(), Rect::from_min_max(Pos2::new(6.0, 0.0), Pos2::new(100.0, 22.0)));
        st.chip_rects.insert("s2".into(), Rect::from_min_max(Pos2::new(100.0, 0.0), Pos2::new(194.0, 22.0)));
        st.chip_rects.insert("s3".into(), Rect::from_min_max(Pos2::new(406.0, 0.0), Pos2::new(500.0, 22.0)));
        st
    }

    fn drag_of(item: DragItem, source: &str, pointer: Pos2) -> TabDrag {
        TabDrag { item, source_pane: source.into(), grab: Vec2::ZERO, size: Vec2::new(90.0, 22.0), pointer }
    }

    #[test]
    fn reorder_to_the_end() {
        let st = two_pane_state();
        let d = drag_of(DragItem::Surface("s1".into()), "pane:1", Pos2::new(180.0, 10.0));
        assert_eq!(
            resolve(&st, &d, d.pointer),
            Some(DropTarget::Move { pane: "pane:1".into(), index: Some(1) })
        );
    }

    #[test]
    fn reorder_to_the_front() {
        let st = two_pane_state();
        let d = drag_of(DragItem::Surface("s2".into()), "pane:1", Pos2::new(20.0, 10.0));
        assert_eq!(
            resolve(&st, &d, d.pointer),
            Some(DropTarget::Move { pane: "pane:1".into(), index: Some(0) })
        );
    }

    #[test]
    fn dropping_at_the_current_slot_is_a_no_op() {
        let st = two_pane_state();
        // s1 is first; dropping left of s2's centre resolves to index 0.
        let d = drag_of(DragItem::Surface("s1".into()), "pane:1", Pos2::new(60.0, 10.0));
        assert_eq!(resolve(&st, &d, d.pointer), None);
    }

    #[test]
    fn move_to_another_panes_body_appends() {
        let st = two_pane_state();
        let d = drag_of(DragItem::Surface("s1".into()), "pane:1", Pos2::new(600.0, 200.0));
        assert_eq!(
            resolve(&st, &d, d.pointer),
            Some(DropTarget::Move { pane: "pane:2".into(), index: None })
        );
    }

    #[test]
    fn move_to_another_panes_strip_inserts() {
        let st = two_pane_state();
        let d = drag_of(DragItem::Surface("s1".into()), "pane:1", Pos2::new(600.0, 10.0));
        assert_eq!(
            resolve(&st, &d, d.pointer),
            Some(DropTarget::Move { pane: "pane:2".into(), index: Some(1) })
        );
    }

    #[test]
    fn dropping_on_the_source_body_is_a_no_op() {
        let st = two_pane_state();
        let d = drag_of(DragItem::Surface("s1".into()), "pane:1", Pos2::new(250.0, 200.0));
        assert_eq!(resolve(&st, &d, d.pointer), None);
    }

    #[test]
    fn edge_bands_split_on_the_matching_side() {
        let st = two_pane_state();
        let d = drag_of(DragItem::Surface("s1".into()), "pane:1", Pos2::new(0.0, 0.0));
        for (p, side) in [
            (Pos2::new(30.0, 200.0), "left"),
            (Pos2::new(370.0, 200.0), "right"),
            (Pos2::new(200.0, 90.0), "up"),
            (Pos2::new(200.0, 380.0), "down"),
        ] {
            assert_eq!(
                resolve(&st, &d, p),
                Some(DropTarget::Split { pane: "pane:1".into(), side }),
                "pointer {p:?}"
            );
        }
    }

    #[test]
    fn edge_drop_on_another_pane_splits_that_pane() {
        let st = two_pane_state();
        // s1 lives in pane:1; the pointer is in pane:2's right edge band.
        let d = drag_of(DragItem::Surface("s1".into()), "pane:1", Pos2::new(780.0, 200.0));
        assert_eq!(
            resolve(&st, &d, d.pointer),
            Some(DropTarget::Split { pane: "pane:2".into(), side: "right" })
        );
    }

    #[test]
    fn edge_drop_on_a_lone_chips_own_pane_is_a_no_op() {
        let st = two_pane_state();
        // s3 is the only tab in pane:2, so the split would leave nothing behind.
        let d = drag_of(DragItem::Surface("s3".into()), "pane:2", Pos2::new(780.0, 200.0));
        assert_eq!(resolve(&st, &d, d.pointer), None);
    }

    #[test]
    fn no_pane_under_the_pointer_is_a_no_op() {
        let st = two_pane_state();
        let d = drag_of(DragItem::Surface("s1".into()), "pane:1", Pos2::new(-50.0, -50.0));
        assert_eq!(resolve(&st, &d, d.pointer), None);
    }

    #[test]
    fn dragging_a_pane_onto_another_swaps() {
        let st = two_pane_state();
        let d = drag_of(DragItem::Pane("pane:1".into()), "pane:1", Pos2::new(600.0, 200.0));
        assert_eq!(
            resolve(&st, &d, d.pointer),
            Some(DropTarget::Swap { other: "pane:2".into() })
        );
    }

    #[test]
    fn dragging_a_pane_onto_itself_is_a_no_op() {
        let st = two_pane_state();
        let d = drag_of(DragItem::Pane("pane:1".into()), "pane:1", Pos2::new(200.0, 200.0));
        assert_eq!(resolve(&st, &d, d.pointer), None);
    }

    // --- sidebar rows -------------------------------------------------------

    /// A sidebar drawn as: folder `f1` with session `w1` in it, then folder `f2`
    /// (holding folder `f3`), then the top-level sessions `w2` and `w3`.
    fn sidebar_state() -> AppState {
        use crate::state::{SidebarRow, SidebarRowKind};
        use lmux_proto::{Folder, Tree};
        let ws = |id: &str, folder: Option<&str>| Workspace {
            id: id.into(),
            title: id.into(),
            folder: folder.map(str::to_owned),
            cwd: "/tmp".into(),
            git_branch: None,
            agent: lmux_proto::AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: format!("pane-{id}") },
            panes: vec![],
            focused_pane: None,
            ports: vec![],
            pr: None,
        };
        let folder = |id: &str, parent: Option<&str>| Folder {
            id: id.into(),
            name: id.into(),
            parent: parent.map(str::to_owned),
            collapsed: false,
        };
        let tree = Tree {
            host: "h".into(),
            folders: vec![folder("f1", None), folder("f2", None), folder("f3", Some("f2"))],
            workspaces: vec![
                ws("w1", Some("f1")),
                ws("w2", None),
                ws("w3", None),
            ],
            selected_workspace: Some("w2".into()),
        };
        let row = |id: &str, kind: SidebarRowKind, top: f32, bottom: f32, in_folder: Option<&str>| {
            SidebarRow {
                id: id.into(),
                kind,
                rect: Rect::from_min_max(Pos2::new(0.0, top), Pos2::new(260.0, bottom)),
                in_folder: in_folder.map(str::to_owned),
            }
        };
        let mut st = AppState::new();
        st.apply_tree(tree);
        st.selected = Some("w2".into());
        st.sidebar_list_rect =
            Some(Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(260.0, 400.0)));
        st.sidebar_rows = vec![
            row("f1", SidebarRowKind::Folder, 0.0, 26.0, None),
            row("w1", SidebarRowKind::Workspace, 26.0, 80.0, Some("f1")),
            row("f2", SidebarRowKind::Folder, 80.0, 106.0, None),
            row("f3", SidebarRowKind::Folder, 106.0, 132.0, Some("f2")),
            row("w2", SidebarRowKind::Workspace, 132.0, 186.0, None),
            row("w3", SidebarRowKind::Workspace, 186.0, 240.0, None),
        ];
        st
    }

    fn sidebar_drag(item: DragItem, pointer: Pos2) -> TabDrag {
        TabDrag {
            item,
            source_pane: String::new(),
            grab: Vec2::ZERO,
            size: Vec2::new(200.0, 54.0),
            pointer,
        }
    }

    fn sidebar_target(st: &AppState, item: DragItem, pointer: Pos2) -> Option<DropTarget> {
        let d = sidebar_drag(item, pointer);
        resolve(st, &d, pointer)
    }

    #[test]
    fn a_session_dropped_on_a_folder_row_joins_that_folder() {
        let st = sidebar_state();
        // The whole folder row is an adopt zone, even below its label.
        assert_eq!(
            sidebar_target(&st, DragItem::Workspace("w2".into()), Pos2::new(100.0, 96.0)),
            Some(DropTarget::Sidebar {
                into: Some("f2".into()),
                slot: Some(0),
                adopt: Some("f2".into()),
                line_y: None,
            })
        );
    }

    #[test]
    fn a_session_dropped_above_a_session_lands_in_front_of_it() {
        let st = sidebar_state();
        // w1 lives in f1; the pointer is in w2's top half, at the top level.
        assert_eq!(
            sidebar_target(&st, DragItem::Workspace("w1".into()), Pos2::new(100.0, 140.0)),
            Some(DropTarget::Sidebar {
                into: None,
                slot: Some(0),
                adopt: None,
                line_y: Some(132.0),
            })
        );
    }

    #[test]
    fn a_session_dropped_below_a_session_lands_behind_it() {
        let st = sidebar_state();
        // Behind w3, which is the last top-level session: the lower half of its
        // row is the end of the list.
        assert_eq!(
            sidebar_target(&st, DragItem::Workspace("w1".into()), Pos2::new(100.0, 225.0)),
            Some(DropTarget::Sidebar {
                into: None,
                slot: Some(2),
                adopt: None,
                line_y: Some(240.0),
            })
        );
    }

    #[test]
    fn a_session_dropped_on_its_own_neighbour_is_a_no_op() {
        let st = sidebar_state();
        // w2 sits directly above w3, so w2 onto w3's top half changes nothing.
        assert_eq!(
            sidebar_target(&st, DragItem::Workspace("w2".into()), Pos2::new(100.0, 140.0)),
            None
        );
        // The same for w2 onto the gap just under itself, which is w3's top half.
        assert_eq!(
            sidebar_target(&st, DragItem::Workspace("w2".into()), Pos2::new(100.0, 184.0)),
            None,
            "the drop below w2 is the slot w2 already holds"
        );
    }

    #[test]
    fn a_session_dropped_past_the_last_row_goes_to_the_top_level() {
        let st = sidebar_state();
        assert_eq!(
            sidebar_target(&st, DragItem::Workspace("w1".into()), Pos2::new(100.0, 380.0)),
            Some(DropTarget::Sidebar {
                into: None,
                slot: None,
                adopt: None,
                line_y: Some(240.0),
            })
        );
    }

    #[test]
    fn a_session_dropped_in_open_space_below_a_folder_does_not_join_it() {
        use crate::state::{SidebarRow, SidebarRowKind};
        let mut st = sidebar_state();
        // Only one folder's session is on screen, so the last row is inside it.
        st.sidebar_rows = vec![
            SidebarRow {
                id: "f1".into(),
                kind: SidebarRowKind::Folder,
                rect: Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(260.0, 26.0)),
                in_folder: None,
            },
            SidebarRow {
                id: "w1".into(),
                kind: SidebarRowKind::Workspace,
                rect: Rect::from_min_max(Pos2::new(0.0, 26.0), Pos2::new(260.0, 80.0)),
                in_folder: Some("f1".into()),
            },
        ];
        assert_eq!(
            sidebar_target(&st, DragItem::Workspace("w2".into()), Pos2::new(100.0, 200.0)),
            Some(DropTarget::Sidebar {
                into: None,
                slot: None,
                adopt: None,
                line_y: Some(80.0),
            }),
            "open space is the top level, not the folder that happens to be last"
        );
    }

    #[test]
    fn a_folder_dropped_on_another_folders_top_band_becomes_its_sibling() {
        let st = sidebar_state();
        // The top of f1's row: f2 leaves the top level's back for its front.
        assert_eq!(
            sidebar_target(&st, DragItem::Folder("f2".into()), Pos2::new(100.0, 2.0)),
            Some(DropTarget::Sidebar {
                into: None,
                slot: Some(0),
                adopt: None,
                line_y: Some(0.0),
            })
        );
    }

    #[test]
    fn a_folder_dropped_below_a_folder_row_nests_inside_it() {
        let st = sidebar_state();
        assert_eq!(
            sidebar_target(&st, DragItem::Folder("f3".into()), Pos2::new(100.0, 100.0)),
            Some(DropTarget::Sidebar {
                into: Some("f2".into()),
                slot: Some(0),
                adopt: Some("f2".into()),
                line_y: None,
            })
        );
    }

    #[test]
    fn a_folder_dropped_into_its_own_subtree_is_refused() {
        let st = sidebar_state();
        // f3 lives inside f2, so f2 cannot adopt it: the daemon refuses that
        // move, and the indicator must not promise it.
        assert_eq!(
            sidebar_target(&st, DragItem::Folder("f2".into()), Pos2::new(100.0, 128.0)),
            None
        );
        // Nor can f2 take f3's slot inside itself.
        assert_eq!(
            sidebar_target(&st, DragItem::Folder("f2".into()), Pos2::new(100.0, 107.0)),
            None
        );
    }

    #[test]
    fn a_folder_dropped_on_a_session_row_joins_that_session_s_folder() {
        let st = sidebar_state();
        assert_eq!(
            sidebar_target(&st, DragItem::Folder("f1".into()), Pos2::new(100.0, 160.0)),
            Some(DropTarget::Sidebar {
                into: None,
                slot: None,
                adopt: None,
                line_y: None,
            })
        );
        // w1 is in f1, so a folder dropped on w1's row lands inside f1.
        assert_eq!(
            sidebar_target(&st, DragItem::Folder("f3".into()), Pos2::new(100.0, 60.0)),
            Some(DropTarget::Sidebar {
                into: Some("f1".into()),
                slot: None,
                adopt: Some("f1".into()),
                line_y: None,
            })
        );
    }

    #[test]
    fn a_drop_outside_the_sidebar_resolves_to_nothing() {
        let st = sidebar_state();
        // In the panes, and above the list in the top icon strip.
        assert_eq!(
            sidebar_target(&st, DragItem::Workspace("w1".into()), Pos2::new(500.0, 160.0)),
            None
        );
        assert_eq!(
            sidebar_target(&st, DragItem::Workspace("w1".into()), Pos2::new(100.0, -20.0)),
            None
        );
        // A chip over the sidebar has no pane under it either.
        assert_eq!(
            sidebar_target(&st, DragItem::Surface("s1".into()), Pos2::new(100.0, 160.0)),
            None
        );
    }
}
