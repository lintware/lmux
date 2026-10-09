//! Bottom status bar: key hints on the left, connection/state on the right.

use egui::{Align2, Color32, FontId, Rect, Stroke, Ui};

use super::Ctx;
use crate::theme;
use crate::util::measure;

struct Seg {
    text: String,
    color: Color32,
    font: FontId,
}

pub fn show(ui: &mut Ui, rect: Rect, c: &mut Ctx) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, theme::STATUS_BG);
    p.line_segment(
        [
            egui::pos2(rect.min.x, rect.min.y + 0.5),
            egui::pos2(rect.max.x, rect.min.y + 0.5),
        ],
        Stroke::new(1.0, theme::HAIRLINE),
    );

    let cy = rect.center().y;
    let hints = [
        ("Enter", "send"),
        ("Shift+Enter/Opt+Enter", "newline"),
        ("Shift+Tab", "mode"),
        ("Ctrl+.", "shortcuts"),
    ];
    let kfont = theme::ui_semibold(theme::FS_SMALL);
    let afont = theme::ui(theme::FS_SMALL);

    // Left: key hints, dropped from the right when the window is narrow.
    let mut x = rect.min.x + 12.0;
    let hard_limit = rect.max.x - 8.0;
    for (i, (key, action)) in hints.iter().enumerate() {
        let seg_w = measure(ui.ctx(), key, &kfont) + measure(ui.ctx(), action, &afont) + 24.0;
        if x + seg_w > hard_limit {
            break;
        }
        if i > 0 {
            p.text(egui::pos2(x, cy), Align2::LEFT_CENTER, "|", afont.clone(), theme::HAIRLINE);
            x += 10.0;
        }
        p.text(egui::pos2(x, cy), Align2::LEFT_CENTER, *key, kfont.clone(), theme::TEXT_SOFT);
        x += measure(ui.ctx(), key, &kfont) + 4.0;
        p.text(egui::pos2(x, cy), Align2::LEFT_CENTER, ":", afont.clone(), theme::DIM_2);
        x += 6.0;
        p.text(egui::pos2(x, cy), Align2::LEFT_CENTER, *action, afont.clone(), theme::DIM);
        x += measure(ui.ctx(), action, &afont) + 10.0;
    }
    let left_end = x;

    // Right: built right-to-left, only what fits before the hints.
    let (state_text, state_color) = match c.net.state {
        crate::net::ConnState::Connected => ("connected", theme::GREEN),
        crate::net::ConnState::Connecting => ("connecting", theme::AMBER),
        crate::net::ConnState::Disconnected => ("disconnected", theme::RED),
    };
    let ws_count = c.state.tree.workspaces.len();
    let pane_count: usize = c.state.tree.workspaces.iter().map(|w| w.panes.len()).sum();
    let focused = c
        .state
        .selected_workspace()
        .and_then(|w| w.focused_pane.clone())
        .and_then(|pane_id| c.state.pane(&pane_id).and_then(|p| c.state.active_surface(p)))
        .unwrap_or_default();

    let mut segs: Vec<Seg> = Vec::new();
    if !focused.is_empty() {
        segs.push(Seg { text: focused, color: theme::DIM_2, font: theme::mono(theme::FS_TINY) });
    }
    segs.push(Seg {
        text: format!("{ws_count} ws · {pane_count} panes"),
        color: theme::DIM,
        font: theme::ui(theme::FS_SMALL),
    });
    let host = format!("{} · {}", c.net.host_name(), state_text);
    segs.push(Seg { text: host, color: theme::TEXT_SOFT, font: theme::ui(theme::FS_SMALL) });

    let mut rx = rect.max.x - 12.0;
    for (i, seg) in segs.iter().enumerate() {
        let w = measure(ui.ctx(), &seg.text, &seg.font);
        if rx - w < left_end + 12.0 {
            break;
        }
        rx -= w;
        let color = if seg.text.ends_with(state_text) { state_color } else { seg.color };
        if i == segs.len() - 1 {
            // Host segment: draw the host dim and the state coloured.
            let host_only = format!("{} · ", c.net.host_name());
            let hw = measure(ui.ctx(), &host_only, &seg.font);
            p.text(egui::pos2(rx, cy), Align2::LEFT_CENTER, host_only, seg.font.clone(), theme::TEXT_SOFT);
            p.text(egui::pos2(rx + hw, cy), Align2::LEFT_CENTER, state_text, seg.font.clone(), state_color);
        } else {
            p.text(egui::pos2(rx, cy), Align2::LEFT_CENTER, &seg.text, seg.font.clone(), color);
        }
        rx -= 14.0;
    }
}
