//! Browser surface: app-drawn toolbar plus the JPEG frame stream.

use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, Id, Rect, Sense, Stroke, Ui, Vec2};
use lmux_proto::methods as m;
use lmux_proto::{BrowserInputEvent, MouseAction, MouseButton, Modifiers};

use super::Ctx;
use crate::theme;
use crate::util::ellipsize;

use super::icons::{self, Arrow};

const TOOLBAR_H: f32 = 28.0;

/// A window-edge drag sends a resize every frame; wait this long for the size to
/// settle before telling the daemon, so the screencast is not restarted or
/// stopped on every intermediate size (review #9).
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);

pub fn show(ui: &mut Ui, rect: Rect, surface: &str, _focused: bool, c: &mut Ctx) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::TERM_BG);

    let toolbar = Rect::from_min_size(rect.min, Vec2::new(rect.width(), TOOLBAR_H));
    paint_toolbar(ui, toolbar, surface, c);

    let area = Rect::from_min_max(egui::pos2(rect.min.x, toolbar.max.y), rect.max);
    let painter = ui.painter_at(area);
    painter.rect_filled(area, 0.0, Color32::from_rgb(0x24, 0x24, 0x24));

    // Upload any newly decoded frame before drawing, then ack it so the daemon
    // releases the next one.
    c.state.ensure_browser(surface, "");
    let mut ack = None;
    if let Some(view) = c.state.browsers.get_mut(surface) {
        if let Some(image) = view.pending_image.take() {
            match &mut view.texture {
                Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                None => {
                    view.texture =
                        Some(ui.ctx().load_texture(surface, image, egui::TextureOptions::LINEAR));
                }
            }
        }
        ack = view.pending_ack.take();
    }
    if let Some(seq) = ack {
        c.state.pending.push(lmux_proto::ClientMsg::BrowserFrameAck {
            surface: surface.to_owned(),
            seq,
        });
    }
    c.state.visible.insert(surface.to_owned());

    let (frame_w, frame_h, has_frame, tex) = {
        let view = c.state.browsers.get(surface).expect("browser view ensured above");
        (view.frame_size.0, view.frame_size.1, view.have_frame, view.texture.clone())
    };

    if !has_frame {
        let msg = "waiting for the first frame…";
        painter.text(
            area.center(),
            Align2::CENTER_CENTER,
            msg,
            theme::ui(theme::FS_SMALL),
            theme::DIM,
        );
        return;
    }

    // Letterbox the frame inside the area, preserving aspect.
    let (fw, fh) = (frame_w.max(1) as f32, frame_h.max(1) as f32);
    let scale = (area.width() / fw).min(area.height() / fh);
    let size = Vec2::new(fw * scale, fh * scale);
    let image_rect = Rect::from_center_size(area.center(), size);
    c.state.browser_image_rects.insert(surface.to_owned(), image_rect);
    if let Some(tex) = &tex {
        ui.painter_at(area).image(
            tex.id(),
            image_rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    // Report the canvas size (CSS px) after it settles. The stream is 1× by
    // default (review #9): a Retina pixels_per_point would quadruple the bytes.
    let want = (
        area.width().round().max(1.0) as u32,
        area.height().round().max(1.0) as u32,
        c.net.browser_scale(),
    );
    let now = Instant::now();
    let (send, schedule) = match c.state.browsers.get_mut(surface) {
        Some(view) => view.resize_decision(want, now, RESIZE_DEBOUNCE),
        None => (false, None),
    };
    if send {
        c.state.pending.push(lmux_proto::ClientMsg::BrowserResize {
            surface: surface.to_owned(),
            width: want.0,
            height: want.1,
            device_scale_factor: want.2,
        });
    }
    if let Some(d) = schedule {
        // Keep the timer alive even when the window is otherwise idle.
        ui.ctx().request_repaint_after(d);
    }

    // Input mapping.
    let to_css = |p: egui::Pos2| -> (f64, f64) {
        let x = (p.x - image_rect.min.x) / scale;
        let y = (p.y - image_rect.min.y) / scale;
        (x as f64, y as f64)
    };
    let resp = ui.interact(image_rect, Id::new(("browser", surface)), Sense::click_and_drag());
    if resp.clicked() {
        if let Some(pane) = c.state.pane_of_surface(surface) {
            c.state.focus_pane(&pane);
        }
    }
    let mods = ui.input(|i| i.modifiers);
    let egui_mods = Modifiers {
        alt: mods.alt,
        ctrl: mods.ctrl,
        meta: mods.command,
        shift: mods.shift,
    };

    let pointer = resp.interact_pointer_pos();
    if let Some(p) = pointer {
        let (x, y) = to_css(p);
        if resp.double_clicked() {
            send_mouse(c, surface, MouseAction::Down, x, y, MouseButton::Left, 2, egui_mods);
            send_mouse(c, surface, MouseAction::Up, x, y, MouseButton::Left, 2, egui_mods);
        } else if resp.clicked() {
            send_mouse(c, surface, MouseAction::Down, x, y, MouseButton::Left, 1, egui_mods);
            send_mouse(c, surface, MouseAction::Up, x, y, MouseButton::Left, 1, egui_mods);
        } else if resp.dragged() || resp.drag_started() || resp.drag_stopped() {
            send_mouse(c, surface, MouseAction::Move, x, y, MouseButton::Left, 0, egui_mods);
        }
        if resp.drag_started() {
            send_mouse(c, surface, MouseAction::Down, x, y, MouseButton::Left, 1, egui_mods);
        }
        if resp.drag_stopped() {
            send_mouse(c, surface, MouseAction::Up, x, y, MouseButton::Left, 1, egui_mods);
        }
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        if scroll.y.abs() > 0.5 || scroll.x.abs() > 0.5 {
            // Wheel at the pointer, not the pane centre, so scrolling a panel
            // inside the page affects the element under the cursor (review #5).
            let p = resp.hover_pos().unwrap_or_else(|| image_rect.center());
            let (x, y) = to_css(p);
            c.state.pending.push(lmux_proto::ClientMsg::BrowserInput {
                surface: surface.to_owned(),
                event: BrowserInputEvent::Wheel {
                    x,
                    y,
                    delta_x: scroll.x as f64,
                    delta_y: scroll.y as f64,
                    modifiers: egui_mods,
                },
            });
        }
    }
}

fn paint_toolbar(ui: &mut Ui, rect: Rect, surface: &str, c: &mut Ctx) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::STRIP_BG);
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.max.y - 0.5),
            egui::pos2(rect.max.x, rect.max.y - 0.5),
        ],
        Stroke::new(1.0, theme::HAIRLINE),
    );

    let (url, loading, can_back, can_fwd) = c
        .state
        .browsers
        .get(surface)
        .map(|b| (b.url.clone(), b.loading, b.can_back, b.can_fwd))
        .unwrap_or_default();

    let cy = rect.center().y;
    let mut x = rect.min.x + 8.0;
    let buttons: [(&str, bool); 3] = [("back", can_back), ("forward", can_fwd), ("reload", true)];
    for (name, enabled) in buttons {
        let hit = Rect::from_center_size(egui::pos2(x + 10.0, cy), Vec2::splat(24.0));
        let resp = ui.interact(hit, Id::new(("browserbtn", surface, name)), Sense::click());
        let color = if !enabled {
            theme::DIM_2
        } else if resp.hovered() {
            theme::TEXT
        } else {
            theme::DIM
        };
        let icon = Rect::from_center_size(hit.center(), Vec2::splat(13.0));
        match name {
            "back" => icons::arrow(&painter, icon, color, Arrow::Left),
            "forward" => icons::arrow(&painter, icon, color, Arrow::Right),
            _ => reload_icon(&painter, icon, color),
        }
        if resp.clicked() && enabled {
            let method = match name {
                "back" => m::BROWSER_BACK,
                "forward" => m::BROWSER_FORWARD,
                _ => m::BROWSER_RELOAD,
            };
            c.state.rpc(method, serde_json::json!({ "surface": surface }));
        }
        x += 26.0;
    }

    // Loading indicator.
    if loading {
        let centre = egui::pos2(x + 8.0, cy);
        let phase = ui.ctx().input(|i| i.time) as f32;
        icons::spinner(&painter, centre, 6.0, theme::ACCENT, phase.fract().abs());
        x += 22.0;
    }

    // URL field.
    let field = Rect::from_min_max(
        egui::pos2(x, rect.min.y + 4.0),
        egui::pos2(rect.max.x - 8.0, rect.max.y - 4.0),
    );
    painter.rect_filled(field, CornerRadius::same(5), Color32::from_rgb(0x2A, 0x2A, 0x2A));
    painter.text(
        egui::pos2(field.min.x + 8.0, field.center().y),
        Align2::LEFT_CENTER,
        ellipsize(ui.ctx(), &url, &theme::ui(theme::FS_SMALL), field.width() - 16.0, false),
        theme::ui(theme::FS_SMALL),
        theme::TEXT_SOFT,
    );

    // Clicking the field starts an edit.
    let resp = ui.interact(field, Id::new(("browserurl", surface)), Sense::click());
    if resp.clicked() {
        if let Some(b) = c.state.browsers.get_mut(surface) {
            b.url_draft = Some(b.url.clone());
        }
    }
    let draft = c.state.browsers.get(surface).and_then(|b| b.url_draft.clone());
    if let Some(mut text) = draft {
        let mut submitted = false;
        let mut cancelled = false;
        let edit_rect = field;
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(edit_rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.add_space(6.0);
                ui.style_mut().visuals.extreme_bg_color = Color32::from_rgb(0x2A, 0x2A, 0x2A);
                let out = ui.add(
                    egui::TextEdit::singleline(&mut text)
                        .font(theme::ui(theme::FS_SMALL))
                        .text_color(theme::TEXT)
                        .desired_width(edit_rect.width() - 12.0),
                );
                if out.lost_focus() {
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submitted = true;
                    } else {
                        cancelled = true;
                    }
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancelled = true;
                }
            },
        );
        if submitted {
            c.state.rpc(m::BROWSER_GOTO, serde_json::json!({ "surface": surface, "url": text }));
            if let Some(b) = c.state.browsers.get_mut(surface) {
                b.url_draft = None;
            }
        } else if cancelled {
            if let Some(b) = c.state.browsers.get_mut(surface) {
                b.url_draft = None;
            }
        } else if let Some(b) = c.state.browsers.get_mut(surface) {
            b.url_draft = Some(text);
        }
    }
}

fn reload_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    use std::f32::consts::TAU;
    let c = rect.center();
    let r = rect.width().min(rect.height()) * 0.32;
    let mut pts = Vec::new();
    for i in 0..=20 {
        let a = -TAU * 0.15 + (i as f32 / 20.0) * TAU * 0.8;
        pts.push(egui::pos2(c.x + r * a.cos(), c.y + r * a.sin()));
    }
    painter.add(egui::Shape::line(pts, Stroke::new(1.3, color)));
    // Arrow head at the open end.
    let tip = egui::pos2(c.x + r * (-TAU * 0.15).cos(), c.y + r * (-TAU * 0.15).sin());
    painter.add(egui::Shape::line(
        vec![
            egui::pos2(tip.x - 3.0, tip.y - 1.0),
            tip,
            egui::pos2(tip.x - 1.0, tip.y - 3.5),
        ],
        Stroke::new(1.3, color),
    ));
}

fn send_mouse(
    c: &mut Ctx,
    surface: &str,
    action: MouseAction,
    x: f64,
    y: f64,
    button: MouseButton,
    click_count: u32,
    modifiers: Modifiers,
) {
    c.state.pending.push(lmux_proto::ClientMsg::BrowserInput {
        surface: surface.to_owned(),
        event: BrowserInputEvent::Mouse { action, x, y, button, click_count, modifiers },
    });
}
