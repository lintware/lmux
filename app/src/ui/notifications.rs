//! Notifications panel, opened from the bell in the sidebar top strip.

use egui::{Align2, Color32, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use lmux_proto::methods as m;

use super::Ctx;
use crate::theme;
use crate::util::{ellipsize, wrap_lines};

pub const WIDTH: f32 = 320.0;

/// The panel for a single host.
pub fn show(ui: &mut Ui, rect: Rect, c: &mut Ctx) {
    show_hosts(ui, rect, &mut [c]);
}

/// The panel for every host: one list, newest first, each item naming its host
/// when there is more than one. Clicking an item selects its workspace on its
/// own host, which puts that host on screen.
pub fn show_hosts(ui: &mut Ui, rect: Rect, hosts: &mut [&mut Ctx]) {
    let multi = hosts.len() > 1;
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, theme::SIDEBAR_BG);
    p.line_segment(
        [
            egui::pos2(rect.min.x + 0.5, rect.min.y),
            egui::pos2(rect.min.x + 0.5, rect.max.y),
        ],
        Stroke::new(1.0, theme::HAIRLINE),
    );

    // Header.
    let header = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 32.0));
    p.text(
        egui::pos2(header.min.x + 12.0, header.center().y),
        Align2::LEFT_CENTER,
        "Notifications",
        theme::ui_semibold(theme::FS_BODY),
        theme::TEXT,
    );
    let clear = Rect::from_min_size(
        egui::pos2(header.max.x - 62.0, header.min.y + 7.0),
        Vec2::new(50.0, 18.0),
    );
    let clear_resp = ui.interact(clear, Id::new("notif-clear"), Sense::click());
    p.text(
        clear.center(),
        Align2::CENTER_CENTER,
        "Clear",
        theme::ui(theme::FS_SMALL),
        if clear_resp.hovered() { theme::TEXT } else { theme::DIM },
    );
    if clear_resp.clicked() {
        for c in hosts.iter_mut() {
            c.state.rpc_empty(m::NOTIFY_CLEAR);
            c.state.notifications.clear();
        }
    }
    p.line_segment(
        [
            egui::pos2(rect.min.x, header.max.y - 0.5),
            egui::pos2(rect.max.x, header.max.y - 0.5),
        ],
        Stroke::new(1.0, theme::HAIRLINE),
    );

    let list = Rect::from_min_max(egui::pos2(rect.min.x, header.max.y), rect.max);
    let builder = egui::UiBuilder::new()
        .max_rect(list)
        .layout(egui::Layout::top_down(egui::Align::Min));
    ui.scope_builder(builder, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt("notifications")
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                // (host index, host name, notification), newest first.
                let mut items: Vec<(usize, String, lmux_proto::Notification)> = hosts
                    .iter()
                    .enumerate()
                    .flat_map(|(i, c)| {
                        let name = c.net.host_name();
                        c.state.notifications.iter().map(move |n| (i, name.clone(), n.clone()))
                    })
                    .collect();
                items.sort_by(|a, b| b.2.created_at.cmp(&a.2.created_at));
                if items.is_empty() {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("nothing yet")
                            .font(theme::ui(theme::FS_SMALL))
                            .color(theme::DIM_2),
                    );
                    return;
                }
                for (host, host_name, n) in items {
                    let c = &mut hosts[host];
                    let subtitle = match (multi, n.subtitle.as_deref()) {
                        (true, Some(sub)) => Some(format!("{host_name} · {sub}")),
                        (true, None) => Some(host_name.clone()),
                        (false, sub) => sub.map(str::to_owned),
                    };
                    let h = item_height(&n, subtitle.is_some());
                    let w = ui.available_width();
                    let (row, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
                    let inner = row.shrink2(Vec2::new(10.0, 4.0));
                    let painter = ui.painter_at(inner);
                    if resp.hovered() {
                        painter.rect_filled(inner, CornerRadius::same(6), Color32::from_white_alpha(6));
                    }
                    let mut y = inner.min.y + 2.0;
                    painter.text(
                        egui::pos2(inner.min.x, y),
                        Align2::LEFT_TOP,
                        ellipsize(ui.ctx(), &n.title, &theme::ui_semibold(theme::FS_BODY), inner.width(), false),
                        theme::ui_semibold(theme::FS_BODY),
                        theme::TEXT,
                    );
                    y += 16.0;
                    if let Some(sub) = subtitle.as_deref() {
                        painter.text(
                            egui::pos2(inner.min.x, y),
                            Align2::LEFT_TOP,
                            ellipsize(ui.ctx(), sub, &theme::ui(theme::FS_SMALL), inner.width(), false),
                            theme::ui(theme::FS_SMALL),
                            theme::DIM,
                        );
                        y += 14.0;
                    }
                    if let Some(body) = n.body.as_deref() {
                        for line in wrap_lines(ui.ctx(), body, &theme::ui(theme::FS_SMALL), inner.width(), 2) {
                            painter.text(
                                egui::pos2(inner.min.x, y),
                                Align2::LEFT_TOP,
                                line,
                                theme::ui(theme::FS_SMALL),
                                theme::TEXT_SOFT,
                            );
                            y += 14.0;
                        }
                    }
                    if !n.read {
                        painter.circle_filled(egui::pos2(inner.max.x - 4.0, inner.min.y + 8.0), 3.0, theme::ACCENT);
                    }
                    painter.line_segment(
                        [
                            egui::pos2(row.min.x, row.max.y - 0.5),
                            egui::pos2(row.max.x, row.max.y - 0.5),
                        ],
                        Stroke::new(1.0, theme::HAIRLINE_SOFT),
                    );
                    let _ = StrokeKind::Inside;
                    if resp.clicked() {
                        c.state.rpc(m::NOTIFY_MARK_READ, serde_json::json!({ "id": n.id }));
                        for item in c.state.notifications.iter_mut() {
                            if item.id == n.id {
                                item.read = true;
                            }
                        }
                        if let Some(ws) = n.workspace.clone() {
                            c.state.select_workspace(&ws);
                        }
                    }
                }
                ui.add_space(8.0);
            });
    });
}

fn item_height(n: &lmux_proto::Notification, subtitle: bool) -> f32 {
    let mut h = 8.0 + 16.0;
    if subtitle {
        h += 14.0;
    }
    if n.body.is_some() {
        h += 28.0;
    }
    h + 6.0
}
