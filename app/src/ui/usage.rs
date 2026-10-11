//! Usage limits at the bottom of the sidebar. A slim bar shows OpenUsage's
//! busiest limit; a click expands every provider's meters above it.

use egui::{Align2, Color32, CornerRadius, Id, Rect, Sense, Stroke, Ui, Vec2};

use super::icons::{self, Arrow};
use crate::theme;
use crate::usage::{self, Provider, Resource, Usage};
use crate::util::ellipsize;

const BAR_H: f32 = 36.0;
const ROW_H: f32 = 32.0;
const HEAD_H: f32 = 32.0;
const MAX_PANEL_H: f32 = 520.0;

fn open_id() -> Id {
    Id::new("usage-open")
}

fn is_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(open_id())).unwrap_or(false)
}

/// Pixels the sidebar gives to this section out of `avail`.
pub fn height(ctx: &egui::Context, avail: f32) -> f32 {
    if !is_open(ctx) {
        return BAR_H;
    }
    let u = usage::current();
    let want = BAR_H + panel_content_h(&u) + 8.0;
    want.min(BAR_H + MAX_PANEL_H).min(avail * 0.7).max(BAR_H)
}

fn panel_content_h(u: &Usage) -> f32 {
    if !u.online || u.providers.is_empty() {
        return 56.0;
    }
    u.providers
        .iter()
        .map(|(_, p)| HEAD_H + rows(p).len() as f32 * ROW_H + 8.0)
        .sum()
}

/// Meters first (bounded limits), then balances.
fn rows(p: &Provider) -> Vec<(&String, &Resource)> {
    let mut v: Vec<_> = p.resources.iter().collect();
    v.sort_by_key(|(_, r)| r.kind != "consumption");
    v
}

/// Draws the section; returns true when the REMOTE button was clicked.
pub fn show(ui: &mut Ui, rect: Rect) -> bool {
    let ctx = ui.ctx().clone();
    let p = ui.painter_at(rect);
    let u = usage::current();
    let open = is_open(&ctx);
    let now = now_unix();

    p.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, theme::HAIRLINE));

    let full = Rect::from_min_size(egui::pos2(rect.min.x, rect.max.y - BAR_H), Vec2::new(rect.width(), BAR_H));
    let label_font = theme::pixel(theme::FS_SMALL);
    let remote_w = 2.0 * theme::SIDEBAR_PAD + 12.0 + crate::util::measure(&ctx, "REMOTE", &label_font);
    let split = (full.max.x - remote_w).round();
    let bar = Rect::from_min_max(full.min, egui::pos2(split, full.max.y));
    let remote = Rect::from_min_max(egui::pos2(split, full.min.y), full.max);
    let remote_clicked = remote_button(ui, &p, remote, &label_font);
    p.line_segment(
        [egui::pos2(split + 0.5, full.min.y + 9.0), egui::pos2(split + 0.5, full.max.y - 9.0)],
        Stroke::new(1.0, theme::HAIRLINE),
    );

    let resp = ui.interact(bar, Id::new("usage-bar"), Sense::click());
    if resp.hovered() {
        p.rect_filled(bar, 0.0, theme::chip_hover_bg());
    }
    if resp.clicked() {
        ctx.data_mut(|d| d.insert_temp(open_id(), !open));
        // The section's height was already chosen this frame.
        ctx.request_repaint();
    }

    let inner = bar.shrink2(Vec2::new(theme::SIDEBAR_PAD, 0.0));
    let cy = bar.center().y;
    let (dot, tip) = if !u.online {
        (theme::DIM_2, "OpenUsage is not running")
    } else if u.providers.iter().any(|(_, p)| p.stale) {
        (theme::AMBER, "Some data is stale")
    } else {
        (theme::GREEN, "Connected to OpenUsage")
    };
    icons::dot(&p, egui::pos2(inner.min.x + 4.0, cy), 3.0, dot);
    let color = if open || resp.hovered() { theme::TEXT_SOFT } else { theme::DIM };
    // Narrow sidebars drop the percent first, then the label.
    let label_end = inner.min.x + 16.0 + crate::util::measure(&ctx, "USAGE", &label_font);
    let chevron_x = inner.max.x - 4.0;
    if label_end + 6.0 <= chevron_x - 6.0 {
        p.text(egui::pos2(inner.min.x + 16.0, cy), Align2::LEFT_CENTER, "USAGE", label_font, color);
    }
    if let Some(peak) = u.peak() {
        let pct = (peak * 100.0).round() as i64;
        let text = format!("{pct}%");
        let font = theme::ui(theme::FS_SMALL);
        let right = inner.max.x - 16.0;
        if right - crate::util::measure(&ctx, &text, &font) >= label_end + 6.0 {
            p.text(egui::pos2(right, cy), Align2::RIGHT_CENTER, text, font, level_color(peak));
        }
    }
    icons::chevron(&p, egui::pos2(chevron_x, cy), 3.4, theme::DIM, if open { Arrow::Down } else { Arrow::Up });
    resp.on_hover_text(tip);

    if !open {
        return remote_clicked;
    }
    let panel = Rect::from_min_max(egui::pos2(rect.min.x, rect.min.y + 1.0), egui::pos2(rect.max.x, bar.min.y));
    let builder = egui::UiBuilder::new()
        .max_rect(panel)
        .layout(egui::Layout::top_down(egui::Align::Min));
    ui.scope_builder(builder, |ui| {
        egui::ScrollArea::vertical()
            .id_salt("usage-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if !u.online {
                    note(ui, "OpenUsage is not running", "Install it from github.com/robinebers/openusage and open it.");
                } else if u.providers.is_empty() {
                    note(ui, "No usage yet", "OpenUsage has not fetched any provider.");
                }
                for (_, prov) in &u.providers {
                    provider(ui, prov, now);
                }
            });
    });
    remote_clicked
}

fn remote_button(ui: &mut Ui, p: &egui::Painter, rect: Rect, font: &egui::FontId) -> bool {
    let (sharing, tip) = crate::share_dialog::remote_status();
    let resp = ui.interact(rect, Id::new("remote-bar"), Sense::click());
    if resp.hovered() {
        p.rect_filled(rect, 0.0, theme::chip_hover_bg());
    }
    let x = rect.min.x + theme::SIDEBAR_PAD;
    let cy = rect.center().y;
    icons::dot(p, egui::pos2(x + 4.0, cy), 3.0, if sharing { theme::GREEN } else { theme::DIM_2 });
    let color = if resp.hovered() { theme::TEXT_SOFT } else { theme::DIM };
    p.text(egui::pos2(x + 12.0, cy), Align2::LEFT_CENTER, "REMOTE", font.clone(), color);
    let resp = resp.on_hover_text(tip);
    resp.clicked()
}

fn note(ui: &mut Ui, title: &str, body: &str) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 56.0), Sense::hover());
    let p = ui.painter_at(rect);
    let x = rect.min.x + theme::SIDEBAR_PAD;
    p.text(egui::pos2(x, rect.min.y + 16.0), Align2::LEFT_CENTER, title, theme::ui(theme::FS_BODY), theme::TEXT_SOFT);
    let text = ellipsize(ui.ctx(), body, &theme::ui(theme::FS_TINY), rect.width() - 2.0 * theme::SIDEBAR_PAD, false);
    p.text(egui::pos2(x, rect.min.y + 34.0), Align2::LEFT_CENTER, text, theme::ui(theme::FS_TINY), theme::DIM_2);
}

fn provider(ui: &mut Ui, prov: &Provider, now: i64) {
    let w = ui.available_width();
    let pad = theme::SIDEBAR_PAD;

    let (head, _) = ui.allocate_exact_size(Vec2::new(w, HEAD_H), Sense::hover());
    let p = ui.painter_at(head);
    let name = prov.display_name.split(" — ").next().unwrap_or(&prov.display_name);
    let font = theme::ui_semibold(theme::FS_FOLDER);
    let plan_w = prov
        .plan
        .as_deref()
        .map(|s| crate::util::measure(ui.ctx(), s, &theme::ui(theme::FS_SMALL)) + 6.0)
        .unwrap_or(0.0);
    let name = ellipsize(ui.ctx(), name, &font, (w - 2.0 * pad - plan_w).max(20.0), false);
    p.text(egui::pos2(head.min.x + pad, head.center().y + 4.0), Align2::LEFT_CENTER, &name, font, theme::TEXT_SOFT);
    if let Some(plan) = &prov.plan {
        p.text(
            egui::pos2(head.max.x - pad, head.center().y + 4.0),
            Align2::RIGHT_CENTER,
            plan,
            theme::ui(theme::FS_SMALL),
            theme::DIM,
        );
    }

    for (key, r) in rows(prov) {
        let (row, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::hover());
        let p = ui.painter_at(row);
        let label = pretty(key);
        let x0 = row.min.x + pad;
        let x1 = row.max.x - pad;
        let cy = row.center().y;
        p.text(egui::pos2(x0, cy - 5.0), Align2::LEFT_CENTER, &label, theme::ui(theme::FS_BODY), theme::DIM);

        let value = match (r.kind.as_str(), r.utilization) {
            ("consumption", Some(f)) => format!("{}%", (f * 100.0).round() as i64),
            _ => balance_text(r),
        };
        let vcolor = r.utilization.map(level_color).unwrap_or(theme::TEXT_SOFT);
        p.text(egui::pos2(x1, cy - 5.0), Align2::RIGHT_CENTER, &value, theme::ui_semibold(theme::FS_BODY), vcolor);

        if let Some(f) = r.utilization {
            let track = Rect::from_min_max(egui::pos2(x0, cy + 6.0), egui::pos2(x1, cy + 11.0));
            p.rect_filled(track, CornerRadius::same(3), Color32::from_white_alpha(14));
            let fill = Rect::from_min_size(track.min, Vec2::new(track.width() * f.clamp(0.0, 1.0) as f32, track.height()));
            p.rect_filled(fill, CornerRadius::same(3), level_color(f));
        }
        if let Some(secs) = r.resets_at.as_deref().and_then(|s| usage::secs_until(s, now)) {
            resp.on_hover_text(format!("Resets in {}", usage::countdown(secs)));
        }
    }
    ui.add_space(8.0);
}

fn balance_text(r: &Resource) -> String {
    let n = r.available.or(r.used).unwrap_or(0.0);
    match r.unit.as_str() {
        "usd" => format!("${n:.2}"),
        "resets" => format!("{} available", n as i64),
        other => format!("{} {other}", n as i64),
    }
}

fn pretty(key: &str) -> String {
    match key {
        "rateLimitResets" => return "Limit resets".into(),
        "creditValue" => return "Credit value".into(),
        "sparkWeekly" => return "Spark weekly".into(),
        _ => {}
    }
    let mut out = String::new();
    for (i, c) in key.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else if i == 0 {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn level_color(f: f64) -> Color32 {
    if f >= 0.9 {
        theme::RED
    } else if f >= 0.7 {
        theme::AMBER
    } else {
        theme::GREEN
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
