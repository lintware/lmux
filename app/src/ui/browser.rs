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

/// Shown in the address bar of a blank page.
const PLACEHOLDER: &str = "Search or enter address";

/// Where the address bar's text should go, as cmux's omnibar decides: a URL
/// with a scheme as typed, a bare host (`example.com`, `localhost:3000`,
/// `10.0.0.2/x`) with a scheme added, and anything else as a web search.
/// `None` for blank input.
pub fn address_to_url(input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    if t.chars().any(char::is_whitespace) {
        return Some(search_url(t));
    }
    let lower = t.to_ascii_lowercase();
    if lower.contains("://") {
        return Some(t.to_owned());
    }
    const SCHEMES: [&str; 6] = ["about:", "data:", "file:", "view-source:", "chrome:", "mailto:"];
    if SCHEMES.iter().any(|s| lower.starts_with(s)) {
        return Some(t.to_owned());
    }
    // The host part: up to the first '/', '?' or '#', without a port or user.
    let authority = lower.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host_port, None),
    };
    if port.is_some_and(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return Some(search_url(t));
    }
    let is_ipv4 = host.split('.').count() == 4
        && host.split('.').all(|o| !o.is_empty() && o.len() <= 3 && o.chars().all(|c| c.is_ascii_digit()));
    let local = host == "localhost" || host.ends_with(".localhost") || is_ipv4;
    if local {
        return Some(format!("http://{t}"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    let tld = labels.last().copied().unwrap_or("");
    let hostish = labels.len() >= 2
        && labels
            .iter()
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic());
    if hostish {
        return Some(format!("https://{t}"));
    }
    Some(search_url(t))
}

fn search_url(query: &str) -> String {
    let mut q = String::with_capacity(query.len() * 3);
    for b in query.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => q.push(b as char),
            b' ' => q.push('+'),
            _ => q.push_str(&format!("%{b:02X}")),
        }
    }
    format!("https://www.google.com/search?q={q}")
}

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

    // Report the canvas size (CSS px) after it settles, before any frame has
    // come: the size is what starts the stream. The stream is 1× by default
    // (review #9): a Retina pixels_per_point would quadruple the bytes.
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

    // Input mapping.
    let to_css = |p: egui::Pos2| -> (f64, f64) {
        let x = (p.x - image_rect.min.x) / scale;
        let y = (p.y - image_rect.min.y) / scale;
        (x as f64, y as f64)
    };
    let resp = ui.interact(image_rect, Id::new(("browser", surface)), Sense::click_and_drag());
    let mods = ui.input(|i| i.modifiers);
    let middle = resp.clicked_by(egui::PointerButton::Middle);
    if resp.clicked() || middle {
        if let Some(pane) = c.state.pane_of_surface(surface) {
            c.state.focus_pane(&pane);
        }
    }
    // Middle- or Cmd-click opens the link under the pointer as a new tab in this
    // pane, as in cmux. The page never sees that click: headless Chromium would
    // open the link in a window nobody can see.
    let open_link = middle || (resp.clicked() && mods.command);
    if open_link {
        let p = resp.interact_pointer_pos().or_else(|| resp.hover_pos());
        if let (Some(p), Some(pane)) = (p, c.state.pane_of_surface(surface)) {
            let (x, y) = to_css(p);
            c.state.browser_request(
                m::BROWSER_LINK_AT,
                serde_json::json!({ "surface": surface, "x": x, "y": y }),
                crate::state::BrowserReply::OpenLink { pane },
            );
        }
    }
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
        } else if resp.clicked() && !open_link {
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
    // Reload turns into Stop while the page loads, as in cmux and Safari.
    let reload = if loading { "stop" } else { "reload" };
    let buttons: [(&str, bool); 3] = [("back", can_back), ("forward", can_fwd), (reload, true)];
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
            "stop" => icons::close(&painter, icon, color),
            _ => reload_icon(&painter, icon, color),
        }
        if resp.clicked() && enabled {
            let method = match name {
                "back" => m::BROWSER_BACK,
                "forward" => m::BROWSER_FORWARD,
                "stop" => m::BROWSER_STOP,
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
    let draft = c.state.browsers.get(surface).and_then(|b| b.url_draft.clone());
    if draft.is_none() {
        let blank = url.is_empty() || url == "about:blank";
        let (shown, color) = if blank {
            (PLACEHOLDER, theme::DIM)
        } else {
            (url.as_str(), theme::TEXT_SOFT)
        };
        painter.text(
            egui::pos2(field.min.x + 8.0, field.center().y),
            Align2::LEFT_CENTER,
            ellipsize(ui.ctx(), shown, &theme::ui(theme::FS_SMALL), field.width() - 16.0, false),
            theme::ui(theme::FS_SMALL),
            color,
        );
    }

    // Clicking the field starts an edit with the whole address selected.
    let resp = ui.interact(field, Id::new(("browserurl", surface)), Sense::click());
    if resp.clicked() {
        c.state.focus_address_bar(surface);
    }
    let draft = c.state.browsers.get(surface).and_then(|b| b.url_draft.clone());
    if let Some(mut text) = draft {
        let focus_now = c
            .state
            .browsers
            .get_mut(surface)
            .map(|b| std::mem::take(&mut b.focus_url))
            .unwrap_or(false);
        let mut submitted = false;
        let mut cancelled = false;
        let edit_rect = field;
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(edit_rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.add_space(6.0);
                ui.style_mut().visuals.extreme_bg_color = Color32::from_rgb(0x2A, 0x2A, 0x2A);
                let edit_id = Id::new(("browserurl-edit", surface));
                let mut out = egui::TextEdit::singleline(&mut text)
                    .id(edit_id)
                    .hint_text(PLACEHOLDER)
                    .font(theme::ui(theme::FS_SMALL))
                    .text_color(theme::TEXT)
                    .desired_width(edit_rect.width() - 12.0)
                    .show(ui);
                if focus_now {
                    out.response.request_focus();
                    let end = egui::text::CCursor::new(text.chars().count());
                    out.state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        end,
                    )));
                    out.state.store(ui.ctx(), edit_id);
                }
                // Escape also drops focus, so both end the edit here; Enter
                // navigates and Escape puts the current address back.
                if out.response.lost_focus() {
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
            if let Some(target) = address_to_url(&text) {
                c.state.rpc(m::BROWSER_GOTO, serde_json::json!({ "surface": surface, "url": target }));
            }
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

#[cfg(test)]
mod tests {
    use super::address_to_url as u;

    fn s(x: &str) -> Option<String> {
        Some(x.to_owned())
    }

    #[test]
    fn urls_with_a_scheme_go_as_typed() {
        assert_eq!(u("https://example.com/a?b=1"), s("https://example.com/a?b=1"));
        assert_eq!(u("  http://x.org  "), s("http://x.org"));
        assert_eq!(u("about:blank"), s("about:blank"));
        assert_eq!(u("file:///tmp/x.html"), s("file:///tmp/x.html"));
    }

    #[test]
    fn bare_hosts_get_a_scheme() {
        assert_eq!(u("example.com"), s("https://example.com"));
        assert_eq!(u("docs.rs/egui"), s("https://docs.rs/egui"));
        assert_eq!(u("sub.example.co.uk:8443/x"), s("https://sub.example.co.uk:8443/x"));
        assert_eq!(u("localhost:3000"), s("http://localhost:3000"));
        assert_eq!(u("localhost"), s("http://localhost"));
        assert_eq!(u("127.0.0.1:8080/api"), s("http://127.0.0.1:8080/api"));
    }

    #[test]
    fn words_search() {
        assert_eq!(u("rust egui"), s("https://www.google.com/search?q=rust+egui"));
        assert_eq!(u("hello"), s("https://www.google.com/search?q=hello"));
        assert_eq!(u("c++ & go"), s("https://www.google.com/search?q=c%2B%2B+%26+go"));
        // A version number is not a host.
        assert_eq!(u("1.2"), s("https://www.google.com/search?q=1.2"));
        assert_eq!(u("what:is"), s("https://www.google.com/search?q=what%3Ais"));
    }

    #[test]
    fn blank_input_goes_nowhere() {
        assert_eq!(u(""), None);
        assert_eq!(u("   "), None);
    }
}
