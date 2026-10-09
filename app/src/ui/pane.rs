//! Split layout: pane rectangles, focus ring, inline tabs and draggable dividers.

use egui::{Align2, CornerRadius, Id, Rect, Sense, Stroke, Ui, Vec2};
use lmux_proto::methods as m;
use lmux_proto::{SplitDirection, SurfaceKind, Workspace};

use super::Ctx;
use crate::layout::{self, Solved};
use crate::state::Drag;
use crate::theme;

use super::{browser, tabstrip, terminal};

pub fn show(ui: &mut Ui, rect: Rect, c: &mut Ctx) {
    let Some(ws) = c.state.selected_workspace().cloned() else {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, theme::MAIN_BG);
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "no workspace selected",
            theme::ui(13.0),
            theme::DIM_2,
        );
        c.state.cancel_tab_drag();
        return;
    };

    let mut tree = ws.layout.clone();
    if let Some(d) = c.state.dragging.clone() {
        layout::set_ratio(&mut tree, &d.path, d.ratio);
    }
    let mut solved = Solved::default();
    layout::solve(&tree, rect, theme::DIVIDER_W, &mut solved);

    // A zoomed pane (Cmd+Shift+Enter) takes the whole area on its own.
    let zoomed = c
        .state
        .zoomed_pane
        .clone()
        .filter(|z| solved.panes.iter().any(|s| s.pane == *z));
    if let Some(z) = zoomed {
        draw_pane(ui, rect, &z, &ws, false, c);
        return;
    }
    let multi = solved.panes.len() > 1;

    for slot in &solved.panes {
        draw_pane(ui, slot.rect, &slot.pane, &ws, multi, c);
    }
    for div in &solved.dividers {
        draw_divider(ui, div.rect, div.region, &div.path, div.direction, &ws, c);
    }
}

fn draw_pane(ui: &mut Ui, rect: Rect, pane_id: &str, ws: &Workspace, multi: bool, c: &mut Ctx) {
    let focused = ws.focused_pane.as_deref() == Some(pane_id);
    c.state.pane_rects.insert(pane_id.to_owned(), rect);
    let Some(pane) = c.state.pane(pane_id).cloned() else { return };

    let resp = ui.interact(rect, Id::new(("pane", pane_id)), Sense::click());
    if resp.clicked() && !focused {
        c.state.rpc(m::PANE_FOCUS, serde_json::json!({ "pane": pane_id }));
    }

    // Every pane carries its own strip, focused or not.
    let strip = Rect::from_min_size(rect.min, Vec2::new(rect.width(), theme::PANE_STRIP_H));
    let active = c.state.active_surface(&pane);
    tabstrip::pane_strip(ui, strip, &pane, active.as_deref(), focused, c);
    let content = Rect::from_min_max(egui::pos2(rect.min.x, strip.max.y), rect.max);

    if let Some(sid) = active.as_deref() {
        match c.state.surface(sid).map(|s| s.kind.clone()) {
            Some(SurfaceKind::Terminal { .. }) => terminal::paint(ui, content, sid, focused, c),
            Some(SurfaceKind::Browser { .. }) => browser::show(ui, content, sid, focused, c),
            None => {
                ui.painter_at(content).rect_filled(content, 0.0, theme::TERM_BG);
            }
        }
    } else {
        ui.painter_at(content).rect_filled(content, 0.0, theme::TERM_BG);
    }

    // Unfocused splits are dimmed, as Ghostty's unfocused-split-opacity does
    // in cmux, so the focused one reads at a glance without a border.
    if multi && !focused {
        ui.painter_at(content).rect_filled(content, 0.0, theme::UNFOCUSED_DIM);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_divider(
    ui: &mut Ui,
    rect: Rect,
    region: Rect,
    path: &[u8],
    direction: SplitDirection,
    ws: &Workspace,
    c: &mut Ctx,
) {
    let resp = ui.interact(rect, Id::new(("divider", path.to_vec())), Sense::drag());
    let p = ui.painter_at(rect);
    if resp.hovered() || resp.dragged() {
        p.rect_filled(rect.shrink(1.0), CornerRadius::same(2), theme::ACCENT);
    } else {
        // A hairline so adjacent panes read as separate, without a boxy border.
        let c = rect.center();
        let (a, b) = match direction {
            SplitDirection::Horizontal => {
                (egui::pos2(c.x, rect.min.y), egui::pos2(c.x, rect.max.y))
            }
            SplitDirection::Vertical => {
                (egui::pos2(rect.min.x, c.y), egui::pos2(rect.max.x, c.y))
            }
        };
        p.line_segment([a, b], Stroke::new(1.0, theme::HAIRLINE));
    }

    if resp.drag_started() {
        let ratio = layout::get_ratio(&ws.layout, path).unwrap_or(0.5);
        let pane = layout::first_leaf_at(&ws.layout, path).unwrap_or_default();
        c.state.dragging = Some(Drag {
            path: path.to_vec(),
            direction,
            region,
            pane,
            ratio,
        });
    }
    if resp.dragged() {
        let pos = ui.input(|i| i.pointer.interact_pos()).map(|pt| match direction {
            SplitDirection::Horizontal => pt.x,
            SplitDirection::Vertical => pt.y,
        });
        if let (Some(pos), Some(d)) = (pos, c.state.dragging.as_mut()) {
            d.ratio = layout::ratio_from_pos(region, direction, pos, theme::DIVIDER_W);
        }
        ui.ctx().request_repaint();
    }
    if resp.drag_stopped() {
        if let Some(d) = c.state.dragging.take() {
            c.state.rpc(
                m::PANE_RESIZE,
                serde_json::json!({ "pane": d.pane, "ratio": d.ratio, "path": d.path }),
            );
        }
    }

    // Double-click a divider to reset it to 50/50.
    if resp.double_clicked() {
        if let Some(pane) = layout::first_leaf_at(&ws.layout, path) {
            c.state.rpc(m::PANE_RESIZE, serde_json::json!({ "pane": pane, "ratio": 0.5, "path": path }));
        }
    }
}
