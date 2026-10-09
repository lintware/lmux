//! Drag-and-drop for surface chips and pane headers.
//!
//! The gesture runs in two halves. The drag *starts* inside a chip or a pane
//! strip (`tabstrip.rs`), which stores a [`TabDrag`] in [`AppState`]. Each frame
//! [`show`] then updates the pointer, resolves the [`DropTarget`] against the
//! geometry that pane drawing just recorded (`pane_rects` / `chip_rects`), paints
//! the feedback, and on release issues the RPC the target implies. Because the
//! tree is never patched locally, the result is whatever tree the daemon sends
//! back.

use egui::{Align2, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, Ui};

use super::Ctx;
use crate::state::{AppState, DragItem, DropTarget, TabDrag};
use crate::theme;

/// Fraction of a pane's width/height that counts as its edge, for a split drop.
const EDGE_BAND: f32 = 0.25;

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
/// no-op (no pane under the pointer, the source itself, or its current slot).
fn resolve(state: &AppState, drag: &TabDrag, pointer: Pos2) -> Option<DropTarget> {
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
    }

    // The ghost that follows the pointer.
    let ghost = Rect::from_min_size(drag.pointer - drag.grab, drag.size);
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
        }
    }
}

/// The caret x position for inserting at `index` into `pane` (the dragged chip
/// removed from the sequence, when this is the source pane).
fn caret_x(state: &AppState, drag: &TabDrag, pane: &str, index: usize) -> Option<f32> {
    let p = state.pane(pane)?;
    let dragged = match &drag.item {
        DragItem::Surface(s) => Some(s.as_str()),
        DragItem::Pane(_) => None,
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
}
