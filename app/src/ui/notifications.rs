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
        egui::pos2(header.max.x - 56.0, header.min.y + 7.0),
        Vec2::new(46.0, 18.0),
    );
    let read_all = Rect::from_min_size(
        egui::pos2(clear.min.x - 98.0, header.min.y + 7.0),
        Vec2::new(94.0, 18.0),
    );
    let read_resp = ui.interact(read_all, Id::new("notif-read-all"), Sense::click());
    p.text(
        read_all.center(),
        Align2::CENTER_CENTER,
        "Mark All Read",
        theme::ui(theme::FS_SMALL),
        if read_resp.hovered() { theme::TEXT } else { theme::DIM },
    );
    if read_resp.clicked() {
        for c in hosts.iter_mut() {
            c.state.rpc(m::NOTIFY_MARK_READ, serde_json::json!({}));
            for n in c.state.notifications.iter_mut() {
                n.read = true;
            }
        }
    }
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
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                for (host, host_name, n) in items {
                    let c = &mut hosts[host];
                    // Where it came from: host (when several), workspace, then
                    // the notification's own subtitle.
                    let ws_title = n
                        .workspace
                        .as_deref()
                        .and_then(|w| c.state.workspace(w))
                        .map(|w| w.title.clone());
                    let parts: Vec<String> = [multi.then(|| host_name.clone()), ws_title, n.subtitle.clone()]
                        .into_iter()
                        .flatten()
                        .filter(|s| !s.is_empty())
                        .collect();
                    let subtitle = (!parts.is_empty()).then(|| parts.join(" · "));
                    let h = item_height(&n, subtitle.is_some());
                    let w = ui.available_width();
                    let (row, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
                    let inner = row.shrink2(Vec2::new(10.0, 4.0));
                    let painter = ui.painter_at(inner);
                    if resp.hovered() {
                        painter.rect_filled(inner, CornerRadius::same(6), Color32::from_white_alpha(6));
                    }
                    let mut y = inner.min.y + 2.0;
                    let when = ago(now, n.created_at);
                    let when_w = crate::util::measure(ui.ctx(), &when, &theme::ui(theme::FS_TINY));
                    painter.text(
                        egui::pos2(inner.max.x - 12.0, y + 1.0),
                        Align2::RIGHT_TOP,
                        &when,
                        theme::ui(theme::FS_TINY),
                        theme::DIM_2,
                    );
                    painter.text(
                        egui::pos2(inner.min.x, y),
                        Align2::LEFT_TOP,
                        ellipsize(
                            ui.ctx(),
                            &n.title,
                            &theme::ui_semibold(theme::FS_BODY),
                            inner.width() - when_w - 20.0,
                            false,
                        ),
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
                    // Dismiss, on hover: bottom right, clear of the time.
                    let x_rect = Rect::from_center_size(
                        egui::pos2(inner.max.x - 6.0, inner.max.y - 10.0),
                        Vec2::splat(16.0),
                    );
                    let x_resp = ui.interact(x_rect, Id::new(("notif-x", host, &n.id)), Sense::click());
                    if resp.hovered() || x_resp.hovered() {
                        let col = if x_resp.hovered() { theme::TEXT } else { theme::DIM_2 };
                        super::icons::close(&painter, Rect::from_center_size(x_rect.center(), Vec2::splat(8.0)), col);
                    }
                    if x_resp.clicked() {
                        c.state.rpc(m::NOTIFY_CLEAR, serde_json::json!({ "id": n.id }));
                        c.state.notifications.retain(|i| i.id != n.id);
                        continue;
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

/// "now", "5m", "3h", "2d" for the last week, then the date ("Oct 9", UTC):
/// how long ago `at` (Unix ms) was.
pub fn ago(now_ms: u64, at_ms: u64) -> String {
    let secs = now_ms.saturating_sub(at_ms) / 1000;
    match secs {
        0..=59 => "now".to_owned(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=604_799 => format!("{}d", secs / 86_400),
        _ => {
            let (_, month, day) = civil_from_days((at_ms / 86_400_000) as i64);
            const MONTHS: [&str; 12] =
                ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            format!("{} {day}", MONTHS[(month - 1) as usize])
        }
    }
}

/// Days since 1970-01-01 to (year, month, day), Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::ago;

    #[test]
    fn ago_reads_like_cmux() {
        let now = 1_760_000_000_000;
        assert_eq!(ago(now, now - 5_000), "now");
        assert_eq!(ago(now, now - 5 * 60_000), "5m");
        assert_eq!(ago(now, now - 3 * 3_600_000), "3h");
        assert_eq!(ago(now, now - 2 * 86_400_000), "2d");
        // 30 days before 2026-10-09 00:00 UTC.
        let oct9 = 1_791_504_000_000;
        assert_eq!(ago(oct9, oct9 - 30 * 86_400_000), "Sep 9");
        assert_eq!(ago(now + 10 * 86_400_000_000, 1_000), "Jan 1");
    }
}
