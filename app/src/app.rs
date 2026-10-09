//! The eframe application: wiring network ↔ state ↔ hand-painted UI.

use egui::{Align2, Color32, Id, Key, Modifiers, Rect, Sense, Stroke, Ui, Vec2};
use ewebsock::WsMessage;
use lmux_proto::methods as m;
use lmux_proto::{BrowserInputEvent, ClientMsg, KeyAction, SurfaceKind};

use crate::net::{ConnState, Net};
use crate::state::{Action, AppState};
use crate::theme;
use crate::ui::{self, drag, icons, notifications, pane, sidebar, statusbar, tabstrip};
use crate::util::ellipsize;
use crate::{fonts, keys, wire};

/// One host's connection and the state of its tree. The app holds one per
/// host, all connected at once, and shows them in one sidebar.
struct HostSession {
    state: AppState,
    net: Net,
    was_connected: bool,
}

pub struct LmuxApp {
    hosts: Vec<HostSession>,
    /// The host whose workspace is on screen; keys and the top strip act on it.
    active: usize,
}

impl LmuxApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (ui_font, mono_font) = fonts::install(&cc.egui_ctx);
        configure(&cc.egui_ctx);
        theme::set_traffic_lights(cfg!(target_os = "macos"));
        eprintln!("[fonts] ui={ui_font} mono={mono_font}");
        crate::decode::decoder().set_wake(cc.egui_ctx.clone());

        let configs = crate::net::app_hosts();
        let local_error = if std::env::var("LMUX_WS_URL").is_err() {
            crate::net::ensure_local_daemon().err()
        } else {
            None
        };
        let multi = configs.len() > 1;
        let mut hosts: Vec<HostSession> = (0..configs.len())
            .map(|i| {
                let mut net = Net::for_host(configs.clone(), i);
                net.set_wake(cc.egui_ctx.clone());
                let mut state = AppState::new();
                // Local PTY readers wake the UI when their pane produces output.
                state.set_wake(cc.egui_ctx.clone());
                if multi {
                    state.host_salt = net.host_name();
                }
                net.begin_connect(&cc.egui_ctx);
                HostSession { state, net, was_connected: false }
            })
            .collect();
        // Start on this machine: lmux is a local tool that can also reach out.
        let active = hosts.iter().position(|h| h.net.is_local_host()).unwrap_or(0);
        if let (Some(err), Some(h)) = (local_error, hosts.get_mut(active)) {
            eprintln!("[local] {err}");
            h.state.banner = Some((err, true));
        }
        Self { hosts, active }
    }

    fn ctxs(&mut self) -> Vec<ui::Ctx<'_>> {
        self.hosts
            .iter_mut()
            .map(|h| ui::Ctx { state: &mut h.state, net: &mut h.net })
            .collect()
    }

    /// Put `index` on screen. The sidebar's width and visibility are the
    /// window's, not a host's, so they carry over.
    fn activate(&mut self, index: usize) {
        if index == self.active || index >= self.hosts.len() {
            return;
        }
        let (w, hidden) = {
            let s = &self.hosts[self.active].state;
            (s.sidebar_w, s.sidebar_hidden)
        };
        self.active = index;
        let s = &mut self.hosts[index].state;
        s.sidebar_w = w;
        s.sidebar_hidden = hidden;
    }
}

fn configure(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = theme::MAIN_BG;
        style.visuals.window_fill = theme::SIDEBAR_BG;
        style.visuals.extreme_bg_color = theme::MAIN_BG;
        style.visuals.override_text_color = Some(theme::TEXT);
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, theme::HAIRLINE);
        style.spacing.item_spacing = Vec2::new(4.0, 4.0);
        style.spacing.button_padding = Vec2::new(4.0, 2.0);
        style.spacing.scroll.bar_width = 6.0;
        style.text_styles = [
            (egui::TextStyle::Body, theme::ui(theme::FS_BODY)),
            (egui::TextStyle::Button, theme::ui(theme::FS_BODY)),
            (egui::TextStyle::Small, theme::ui(theme::FS_SMALL)),
            (egui::TextStyle::Monospace, theme::mono(theme::FS_TERM)),
            (egui::TextStyle::Heading, theme::ui_semibold(14.0)),
        ]
        .into();
    });
}

impl eframe::App for LmuxApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump(&ctx);

        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));

        // --- input --------------------------------------------------------
        if !minimized {
            let window_focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
            let active = self.active;
            {
                let h = &mut self.hosts[active];
                let mut c = ui::Ctx { state: &mut h.state, net: &mut h.net };
                shortcuts(&ctx, &mut c);
                let text_editing = ctx.memory(|mem| mem.focused().is_some());
                if window_focused && !text_editing {
                    route_input(&ctx, &mut c);
                }
            }

            // --- OS notifications ----------------------------------------
            let queue: Vec<_> = self
                .hosts
                .iter_mut()
                .flat_map(|h| std::mem::take(&mut h.state.os_queue))
                .collect();
            if !window_focused {
                for n in queue {
                    // Post from a worker thread: showing a macOS notification
                    // synchronously inside winit's event handler re-enters the
                    // run loop and aborts the process.
                    std::thread::spawn(move || {
                        let mut notif = notify_rust::Notification::new();
                        notif.summary(&n.title);
                        if let Some(body) = n.body.as_deref() {
                            notif.body(body);
                        }
                        if let Some(sub) = n.subtitle.as_deref() {
                            notif.subtitle(sub);
                        }
                        let _ = notif.show();
                    });
                }
            }

            // --- paint ----------------------------------------------------
            let active = self.active;
            let mut hosts = self.ctxs();
            render_hosts(ui, &mut hosts, active);
        }

        // A host the user acted on (selected or created in) comes on screen.
        // The host already on screen wins a tie, so a click there is stable.
        let acted = self
            .hosts
            .iter_mut()
            .enumerate()
            .filter_map(|(i, h)| std::mem::take(&mut h.state.activated).then_some(i))
            .collect::<Vec<_>>();
        if !acted.contains(&self.active) {
            if let Some(&i) = acted.first() {
                self.activate(i);
            }
        }

        // Only the host on screen has visible panes, so every other host
        // unsubscribes and streams nothing. A minimised window stops them all.
        for h in &mut self.hosts {
            if minimized {
                h.state.stop_all();
            }
            h.state.sync_subscriptions(h.net.state == ConnState::Connected && !minimized);
        }
        self.flush();

        for from in 0..self.hosts.len() {
            for action in std::mem::take(&mut self.hosts[from].state.actions) {
                match action {
                    Action::SwitchHost(to) => self.create_on(from, to),
                    Action::Reconnect => self.hosts[from].net.begin_connect(&ctx),
                    Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                }
            }
        }
    }

    /// Runs even when the window is hidden (as long as a repaint was requested),
    /// so streaming stops promptly on minimise and reconnects are not missed.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);
        // While minimised, `ui` may not run at all; keep the link alive here and
        // tell the daemon to stop streaming.
        if ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
            for h in &mut self.hosts {
                h.state.stop_all();
            }
            self.flush();
        }
    }
}

impl LmuxApp {
    fn pump(&mut self, ctx: &egui::Context) {
        for h in &mut self.hosts {
            // Each host's tree carries its own name, which is how a create aimed
            // at a host knows when it can go out.
            h.state.set_current_host(&h.net.host_name());
            h.state.set_local_mode(h.net.is_local_host());
            for msg in h.net.poll(ctx) {
                match msg {
                    WsMessage::Text(t) => {
                        if let Some(sm) = wire::decode_server(&t) {
                            h.state.handle_server_msg(sm);
                        }
                    }
                    WsMessage::Binary(b) => h.state.handle_frame(&b),
                    _ => {}
                }
            }
            // Output from the local PTYs, which never goes through the websocket.
            h.state.pump_local_ptys();
            h.state.poll_decoded();
            let connected = h.net.state == ConnState::Connected;
            if !connected && h.was_connected {
                h.state.reset_subscriptions();
            }
            h.was_connected = connected;
        }
        // The create menu on any host lists every host's folders.
        let folders: Vec<(String, Vec<lmux_proto::Folder>)> = self
            .hosts
            .iter()
            .map(|h| (h.net.host_name(), h.state.tree.folders.clone()))
            .collect();
        for h in &mut self.hosts {
            for (name, list) in &folders {
                h.state.host_folders.insert(name.clone(), list.clone());
            }
        }
    }

    fn flush(&mut self) {
        for h in &mut self.hosts {
            for msg in std::mem::take(&mut h.state.pending) {
                h.net.send(&msg);
            }
        }
    }

    /// A create picked on host `from`'s menu for host `to`: hand it over and
    /// put `to` on screen. It goes out now if `to` is connected, otherwise when
    /// its tree arrives.
    fn create_on(&mut self, from: usize, to: usize) {
        if to >= self.hosts.len() {
            return;
        }
        let pending = self.hosts[from].state.pending_create.take();
        self.activate(to);
        let h = &mut self.hosts[to];
        h.state.pending_create = pending;
        if h.net.state == ConnState::Connected {
            h.state.fire_pending_create();
        }
    }
}

/// Paint the whole window into `ui` for a single host. Public so kittest
/// snapshot tests can drive the same code path as the running app.
pub fn render(ui: &mut Ui, c: &mut ui::Ctx) {
    render_hosts(ui, std::slice::from_mut(c), 0);
}

/// Paint the whole window: every host in the sidebar, and the workspace of
/// `hosts[active]` in the main area. Public for tests.
pub fn render_hosts(ui: &mut Ui, hosts: &mut [ui::Ctx], active: usize) {
    let ctx = ui.ctx().clone();
    let full = ui.max_rect();
    // Chip / browser-frame rects are recorded per frame so tests can drive the
    // real geometry.
    for h in hosts.iter_mut() {
        h.state.chip_rects.clear();
        h.state.card_rects.clear();
        h.state.folder_rects.clear();
        h.state.sidebar_rows.clear();
        h.state.sidebar_list_rect = None;
        h.state.folder_plus_rects.clear();
        h.state.pane_rects.clear();
        h.state.browser_image_rects.clear();
    }
    let active = active.min(hosts.len().saturating_sub(1));
    ui.painter().rect_filled(full, 0.0, theme::MAIN_BG);

    let status_rect = Rect::from_min_max(
        egui::pos2(full.min.x, full.max.y - theme::STATUS_H),
        full.max,
    );
    let body = Rect::from_min_max(full.min, egui::pos2(full.max.x, status_rect.min.y));

    let (sidebar_hidden, sidebar_w) = match hosts.get(active) {
        Some(h) => (h.state.sidebar_hidden, h.state.sidebar_w),
        None => return,
    };
    let sb_w = if sidebar_hidden {
        0.0
    } else {
        let max = (body.width() * 0.5).max(theme::SIDEBAR_MIN_W);
        sidebar_w.clamp(theme::SIDEBAR_MIN_W, theme::SIDEBAR_MAX_W).min(max)
    };
    let sidebar_rect = Rect::from_min_size(body.min, Vec2::new(sb_w, body.height()));
    let main_rect = Rect::from_min_max(egui::pos2(body.min.x + sb_w, body.min.y), body.max);

    if sb_w > 0.0 {
        sidebar::show_hosts(ui, sidebar_rect, hosts, active);
    }
    let (left, right) = hosts.split_at_mut(active);
    let Some((c, right)) = right.split_first_mut() else { return };

    if sb_w > 0.0 {

        let handle = Rect::from_min_size(
            egui::pos2(sidebar_rect.max.x - 3.0, body.min.y),
            Vec2::new(6.0, body.height()),
        );
        let resp = ui.interact(handle, Id::new("sidebar-resize"), Sense::drag());
        if resp.dragged() {
            c.state.sidebar_w = (c.state.sidebar_w + resp.drag_delta().x)
                .clamp(theme::SIDEBAR_MIN_W, theme::SIDEBAR_MAX_W);
            ctx.request_repaint();
        }
        ui.painter().line_segment(
            [
                egui::pos2(sidebar_rect.max.x - 0.5, body.min.y),
                egui::pos2(sidebar_rect.max.x - 0.5, body.max.y),
            ],
            Stroke::new(1.0, theme::HAIRLINE),
        );
    } else {
        // Nothing here: the sidebar toggle is drawn after the header row below.
        // The header paints its strip background across the whole row, so a
        // toggle drawn before it was invisible — a hidden sidebar then had no
        // way back.
    }

    // The workspace gets its own header row; each pane carries its own strip.
    let header_rect = Rect::from_min_size(main_rect.min, Vec2::new(main_rect.width(), theme::WS_HEADER_H));
    tabstrip::workspace_header(ui, header_rect, c);

    if c.state.sidebar_hidden {
        let hit = Rect::from_min_size(
            egui::pos2(header_rect.min.x + theme::titlebar_inset() + 4.0, header_rect.min.y + 4.0),
            Vec2::splat(22.0),
        );
        let resp = ui.interact(hit, Id::new("show-sidebar"), Sense::click());
        let color = if resp.hovered() { theme::TEXT } else { theme::DIM };
        icons::sidebar_toggle(
            &ui.painter(),
            Rect::from_center_size(hit.center(), Vec2::splat(13.0)),
            color,
        );
        if resp.clicked() {
            c.state.sidebar_hidden = false;
        }
    }

    let mut content = Rect::from_min_max(egui::pos2(main_rect.min.x, header_rect.max.y), main_rect.max);

    if c.net.state != ConnState::Connected {
        let h = 22.0;
        let banner = Rect::from_min_size(content.min, Vec2::new(content.width(), h));
        let (bg, fg, text) = match c.net.state {
            ConnState::Connecting => (
                Color32::from_rgb(0x3A, 0x2E, 0x10),
                theme::AMBER,
                format!("connecting to {}…", c.net.host_name()),
            ),
            _ => (
                Color32::from_rgb(0x3A, 0x16, 0x16),
                Color32::from_rgb(0xFF, 0xB4, 0xB4),
                match c.net.last_error.as_deref() {
                    Some(err) => format!("disconnected · {err}"),
                    None => "disconnected — reconnecting…".to_owned(),
                },
            ),
        };
        let p = ui.painter_at(banner);
        p.rect_filled(banner, 0.0, bg);
        p.text(
            egui::pos2(banner.min.x + 12.0, banner.center().y),
            Align2::LEFT_CENTER,
            ellipsize(&ctx, &text, &theme::ui(theme::FS_SMALL), banner.width() - 24.0, false),
            theme::ui(theme::FS_SMALL),
            fg,
        );
        content = Rect::from_min_max(egui::pos2(content.min.x, banner.max.y), content.max);
    }

    pane::show(ui, content, c);

    if c.state.notif_open {
        let w = notifications::WIDTH.min(content.width());
        let nrect = Rect::from_min_max(egui::pos2(content.max.x - w, content.min.y), content.max);
        notifications::show(ui, nrect, c);
    }

    statusbar::show(ui, status_rect, c);

    // Drag feedback runs last, over everything a drag can land on. The sidebar
    // rows and the panes have both been drawn, so one pass resolves against the
    // geometry on screen and the ghost is free to cross between them. A row
    // dragged on another host's group belongs to that host's state.
    drag::show(ui, c);
    for other in left.iter_mut().chain(right.iter_mut()) {
        if other.state.tab_drag.is_some() {
            drag::show(ui, other);
        }
    }
}

/// Global keybindings. Public so kittest interaction tests can drive it.
pub fn shortcuts(ctx: &egui::Context, c: &mut ui::Ctx) {
    let cmd = Modifiers::COMMAND;
    let cmd_shift = Modifiers { command: true, shift: true, ..Default::default() };

    let focused_pane = c.state.focused_pane();
    let active_surface = focused_pane
        .as_deref()
        .and_then(|p| c.state.pane(p))
        .and_then(|p| c.state.active_surface(p));

    // Most specific first (consume_key ignores extra Shift/Alt otherwise).
    if ctx.input_mut(|i| i.consume_key(cmd_shift, Key::D)) {
        split(c, &focused_pane, &active_surface, "down");
    }
    if ctx.input_mut(|i| i.consume_key(cmd_shift, Key::B)) {
        // A browser chip (a tab in the focused pane), not a new pane.
        if let Some(pane) = &focused_pane {
            c.state.new_browser_chip(pane);
        }
    }
    if ctx.input_mut(|i| i.consume_key(cmd_shift, Key::N)) {
        c.state.rpc(
            m::NOTIFY,
            serde_json::json!({ "title": "lmux", "body": "Test notification from the app." }),
        );
    }
    if ctx.input_mut(|i| i.consume_key(cmd, Key::D)) {
        split(c, &focused_pane, &active_surface, "right");
    }
    if ctx.input_mut(|i| i.consume_key(cmd, Key::T)) {
        if let Some(pane) = &focused_pane {
            c.state.new_terminal_chip(pane);
        }
    }
    if ctx.input_mut(|i| i.consume_key(cmd, Key::W)) {
        // Closes the focused pane's active chip, with focus following the close.
        c.state.close_active_chip();
    }

    let digits = [
        Key::Num1,
        Key::Num2,
        Key::Num3,
        Key::Num4,
        Key::Num5,
        Key::Num6,
        Key::Num7,
        Key::Num8,
        Key::Num9,
    ];
    for (i, key) in digits.iter().enumerate() {
        if ctx.input_mut(|inp| inp.consume_key(cmd, *key)) {
            if let Some(ws) = c.state.tree.workspaces.get(i) {
                let id = ws.id.clone();
                c.state.select_workspace(&id);
            }
        }
    }
}

fn split(
    c: &mut ui::Ctx,
    focused_pane: &Option<String>,
    active_surface: &Option<String>,
    side: &str,
) {
    let mut params = serde_json::json!({ "side": side, "type": "terminal" });
    if let Some(obj) = params.as_object_mut() {
        if let Some(p) = focused_pane {
            obj.insert("pane".into(), serde_json::json!(p));
        }
        if let Some(s) = active_surface {
            obj.insert("surface".into(), serde_json::json!(s));
        }
    }
    c.state.rpc(m::SPLIT_NEW, params);
}

/// Route keyboard events to the focused surface. Public for tests.
pub fn route_input(ctx: &egui::Context, c: &mut ui::Ctx) {
    let Some(pane_id) = c.state.focused_pane() else { return };
    let Some(pane) = c.state.pane(&pane_id).cloned() else { return };
    let Some(sid) = c.state.active_surface(&pane) else { return };
    let kind = c.state.surface(&sid).map(|s| s.kind.clone());
    let mods = ctx.input(|i| i.modifiers);
    let egui_mods = lmux_proto::Modifiers {
        alt: mods.alt,
        ctrl: mods.ctrl,
        meta: mods.command,
        shift: mods.shift,
    };

    match kind {
        Some(SurfaceKind::Terminal { .. }) => {
            let mut bytes: Vec<u8> = Vec::new();
            ctx.input(|i| {
                for ev in &i.events {
                    match ev {
                        egui::Event::Text(t) => {
                            // Control characters arrive as text too; the key handler owns them.
                            if t.chars().all(|ch| ch >= ' ') {
                                if let Some(b) = keys::text_input(t, i.modifiers) {
                                    bytes.extend_from_slice(&b);
                                }
                            }
                        }
                        egui::Event::Key { key, pressed: true, modifiers, .. } => {
                            if modifiers.command {
                                continue;
                            }
                            if let Some(b) = keys::terminal_key(*key, *modifiers) {
                                bytes.extend_from_slice(&b);
                            }
                        }
                        egui::Event::Paste(t) => bytes.extend_from_slice(t.as_bytes()),
                        _ => {}
                    }
                }
            });
            if !bytes.is_empty() {
                // A local pane is attached by this process: its keys go straight
                // to the PTY. Remote panes keep the daemon's input path.
                let wrote = c.state.local_terminal(&sid) && c.state.write_local(&sid, &bytes);
                if !wrote {
                    let data = String::from_utf8_lossy(&bytes).into_owned();
                    c.state.pending.push(ClientMsg::TermInput { surface: sid, data });
                }
            }
        }
        Some(SurfaceKind::Browser { .. }) => {
            let mut events: Vec<BrowserInputEvent> = Vec::new();
            ctx.input(|i| {
                for ev in &i.events {
                    match ev {
                        egui::Event::Key { key, pressed: true, modifiers, .. } => {
                            if modifiers.command {
                                continue;
                            }
                            if let Some((k, code)) = keys::dom_key(*key) {
                                events.push(BrowserInputEvent::Key {
                                    action: KeyAction::Down,
                                    key: k.to_owned(),
                                    code: code.to_owned(),
                                    text: None,
                                    modifiers: egui_mods,
                                });
                            }
                        }
                        egui::Event::Text(t) if !t.is_empty() => {
                            events.push(BrowserInputEvent::InsertText { text: t.clone() });
                        }
                        _ => {}
                    }
                }
            });
            for event in events {
                c.state.pending.push(ClientMsg::BrowserInput { surface: sid.clone(), event });
            }
        }
        _ => {}
    }
}
