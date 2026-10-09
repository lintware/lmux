//! The eframe application: wiring network ↔ state ↔ hand-painted UI.

use egui::{Align2, Color32, Id, Key, Modifiers, Rect, Sense, Stroke, Ui, Vec2};
use ewebsock::WsMessage;
use lmux_proto::methods as m;
use lmux_proto::{BrowserInputEvent, ClientMsg, KeyAction, SurfaceKind};

use crate::net::{ConnState, Net};
use crate::state::{Action, AppState};
use crate::theme;
use crate::ui::{self, drag, icons, notifications, pane, sidebar, tabstrip};
use crate::util::ellipsize;
use crate::{fonts, keys, wire};

/// What the window remembers between launches (eframe's storage): layout
/// and view preferences, never anything a daemon owns.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct UiPrefs {
    sidebar_w: Option<f32>,
    sidebar_hidden: bool,
    term_font: Option<f32>,
    /// The host on screen, by name.
    active_host: Option<String>,
    /// Hosts whose sidebar group is folded, by name.
    collapsed_hosts: Vec<String>,
    /// Folders folded in the sidebar, per host: `(host, folder ref)`.
    collapsed_folders: Vec<(String, String)>,
}

const PREFS_KEY: &str = "lmux-ui";

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
        // Restore the window's own preferences, then start on the host that
        // was on screen last (this machine the first time).
        let prefs: UiPrefs = cc.storage.and_then(|s| eframe::get_value(s, PREFS_KEY)).unwrap_or_default();
        for h in &mut hosts {
            let name = h.net.host_name();
            if let Some(w) = prefs.sidebar_w {
                h.state.sidebar_w = w;
            }
            h.state.sidebar_hidden = prefs.sidebar_hidden;
            if let Some(f) = prefs.term_font {
                h.state.term_font = f;
            }
            h.state.host_collapsed = prefs.collapsed_hosts.contains(&name);
            for (host, folder) in &prefs.collapsed_folders {
                if *host == name {
                    h.state.collapsed.insert(folder.clone());
                }
            }
        }
        let active = prefs
            .active_host
            .as_ref()
            .and_then(|name| hosts.iter().position(|h| h.net.host_name() == *name))
            .or_else(|| hosts.iter().position(|h| h.net.is_local_host()))
            .unwrap_or(0);
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

    /// Put `index` on screen. The sidebar's width and visibility and the
    /// terminal font size are the window's, not a host's, so they carry over.
    fn activate(&mut self, index: usize) {
        if index == self.active || index >= self.hosts.len() {
            return;
        }
        let (w, hidden, font) = {
            let s = &self.hosts[self.active].state;
            (s.sidebar_w, s.sidebar_hidden, s.term_font)
        };
        self.active = index;
        let s = &mut self.hosts[index].state;
        s.sidebar_w = w;
        s.sidebar_hidden = hidden;
        s.term_font = font;
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

        let actions: Vec<(usize, Action)> = self
            .hosts
            .iter_mut()
            .enumerate()
            .flat_map(|(i, h)| std::mem::take(&mut h.state.actions).into_iter().map(move |a| (i, a)))
            .collect();
        // Removing a host shifts the indices after it, so removals go last,
        // highest index first.
        let mut removals: Vec<usize> = Vec::new();
        for (from, action) in actions {
            match action {
                Action::SwitchHost(to) => self.create_on(from, to),
                Action::Reconnect => self.hosts[from].net.begin_connect(&ctx),
                Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Action::AddHost(target) => self.add_host(&target, &ctx),
                Action::RemoveHost => removals.push(from),
            }
        }
        removals.sort_unstable();
        removals.dedup();
        for i in removals.into_iter().rev() {
            self.remove_host(i);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let Some(on_screen) = self.hosts.get(self.active) else { return };
        let prefs = UiPrefs {
            sidebar_w: Some(on_screen.state.sidebar_w),
            sidebar_hidden: on_screen.state.sidebar_hidden,
            term_font: Some(on_screen.state.term_font),
            active_host: Some(on_screen.net.host_name()),
            collapsed_hosts: self
                .hosts
                .iter()
                .filter(|h| h.state.host_collapsed)
                .map(|h| h.net.host_name())
                .collect(),
            collapsed_folders: self
                .hosts
                .iter()
                .flat_map(|h| {
                    let name = h.net.host_name();
                    h.state.collapsed.iter().map(move |f| (name.clone(), f.clone()))
                })
                .collect(),
        };
        eframe::set_value(storage, PREFS_KEY, &prefs);
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

    fn host_configs(&self) -> Vec<crate::net::HostConfig> {
        self.hosts.first().map(|h| h.net.hosts().to_vec()).unwrap_or_default()
    }

    /// Point every connection at the new host list; each keeps its own host.
    fn reindex(&mut self, configs: &[crate::net::HostConfig]) {
        let multi = configs.len() > 1;
        for (i, h) in self.hosts.iter_mut().enumerate() {
            h.net.set_hosts(configs.to_vec(), i);
            h.state.host_salt = if multi { h.net.host_name() } else { String::new() };
        }
    }

    /// Add a remote host over SSH, connect to it and put it on screen. A host
    /// that is already configured is just brought on screen.
    fn add_host(&mut self, target: &str, ctx: &egui::Context) {
        let target = target.trim();
        if target.is_empty() {
            return;
        }
        if let Some(i) = self.hosts.iter().position(|h| {
            h.net.current_host().is_some_and(|c| c.name == target || c.ssh_target.as_deref() == Some(target))
        }) {
            self.activate(i);
            return;
        }
        let mut configs = self.host_configs();
        configs.push(crate::net::ssh_host(target));
        if let Err(e) = crate::net::save_hosts(&configs) {
            self.hosts[self.active].state.banner = Some((format!("could not save hosts.json: {e}"), true));
        }
        let mut net = Net::for_host(configs.clone(), configs.len() - 1);
        net.set_wake(ctx.clone());
        let mut state = AppState::new();
        state.set_wake(ctx.clone());
        net.begin_connect(ctx);
        self.hosts.push(HostSession { state, net, was_connected: false });
        self.reindex(&configs);
        self.activate(self.hosts.len() - 1);
    }

    /// Forget a remote host: close its connection and drop it from hosts.json.
    /// Its daemon and terminals keep running on that machine.
    fn remove_host(&mut self, index: usize) {
        if index >= self.hosts.len() || self.hosts[index].net.is_local_host() || self.hosts.len() == 1 {
            return;
        }
        let mut configs = self.host_configs();
        configs.remove(index);
        if let Err(e) = crate::net::save_hosts(&configs) {
            self.hosts[self.active].state.banner = Some((format!("could not save hosts.json: {e}"), true));
        }
        let was_active = self.active == index;
        let (w, hidden) = (self.hosts[self.active].state.sidebar_w, self.hosts[self.active].state.sidebar_hidden);
        self.hosts.remove(index);
        if self.active > index || self.active >= self.hosts.len() {
            self.active = self.active.saturating_sub(1);
        }
        if was_active {
            let s = &mut self.hosts[self.active].state;
            s.sidebar_w = w;
            s.sidebar_hidden = hidden;
        }
        self.reindex(&configs);
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

    // No bottom bar, as in cmux: host and connection state live in the
    // sidebar's host headers and the disconnected banner.
    let body = full;

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
        // The panel lists every host's notifications, newest first.
        let mut all: Vec<&mut ui::Ctx> = left.iter_mut().collect();
        all.push(&mut *c);
        all.extend(right.iter_mut());
        notifications::show_hosts(ui, nrect, &mut all);
    }

    // Opened from any host's menu; drawn once, on the host on screen.
    for other in left.iter_mut().chain(right.iter_mut()) {
        if let Some(draft) = other.state.add_host_draft.take() {
            c.state.add_host_draft = Some(draft);
        }
    }
    add_host_dialog(&ctx, c);

    // The palette draws over everything; what it picks runs on its host.
    let picked = {
        let mut all: Vec<&mut ui::Ctx> = left.iter_mut().collect();
        let at = all.len();
        all.push(&mut *c);
        all.extend(right.iter_mut());
        ui::palette::show(ui, full, &mut all, at)
    };
    match picked {
        Some(ui::palette::Pick::Command(name)) => run_command(c, name),
        Some(ui::palette::Pick::Workspace(host, id)) => {
            let target = if host < active {
                left.get_mut(host)
            } else if host == active {
                Some(&mut *c)
            } else {
                right.get_mut(host - active - 1)
            };
            if let Some(h) = target {
                h.state.select_workspace(&id);
            }
        }
        None => {}
    }

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

/// Global keybindings, matching cmux's defaults. Public so kittest interaction
/// tests can drive it.
pub fn shortcuts(ctx: &egui::Context, c: &mut ui::Ctx) {
    let cmd = Modifiers::COMMAND;
    let cmd_shift = Modifiers { command: true, shift: true, ..Default::default() };
    let cmd_alt = Modifiers { command: true, alt: true, ..Default::default() };
    let cmd_ctrl = Modifiers { command: true, ctrl: true, ..Default::default() };
    let ctrl = Modifiers::CTRL;
    let key = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));

    let focused_pane = c.state.focused_pane();
    let active_surface = focused_pane
        .as_deref()
        .and_then(|p| c.state.pane(p))
        .and_then(|p| c.state.active_surface(p));

    // Most specific modifiers first: consume_key ignores extra Shift/Alt.
    // Panes: split, move focus.
    if key(cmd_shift, Key::D) {
        split(c, &focused_pane, &active_surface, "down");
    }
    for (k, dir) in [
        (Key::ArrowLeft, (-1, 0)),
        (Key::ArrowRight, (1, 0)),
        (Key::ArrowUp, (0, -1)),
        (Key::ArrowDown, (0, 1)),
    ] {
        if key(cmd_alt, k) {
            c.state.focus_direction(dir);
        }
    }
    // Tabs: browser tab, next/previous, rename.
    if key(cmd_shift, Key::B) {
        // A browser chip (a tab in the focused pane), not a new pane.
        if let Some(pane) = &focused_pane {
            c.state.new_browser_chip(pane);
        }
    }
    if key(cmd_shift, Key::CloseBracket) {
        c.state.cycle_tab(1);
    }
    if key(cmd_shift, Key::OpenBracket) {
        c.state.cycle_tab(-1);
    }
    // Workspaces: next/previous, close, rename.
    if key(cmd_ctrl, Key::CloseBracket) {
        c.state.cycle_workspace(1);
    }
    if key(cmd_ctrl, Key::OpenBracket) {
        c.state.cycle_workspace(-1);
    }
    if key(cmd_shift, Key::W) {
        if let Some(ws) = c.state.selected.clone() {
            c.state.rpc(m::WORKSPACE_CLOSE, serde_json::json!({ "workspace": ws }));
        }
    }
    if key(cmd_shift, Key::R) {
        c.state.begin_ws_rename(None);
    }
    if key(cmd_shift, Key::Enter) {
        c.state.toggle_zoom();
    }
    // Notifications: jump to the newest unread.
    if key(cmd_shift, Key::U) {
        c.state.jump_to_unread();
    }
    // The palette: commands, or go to a workspace.
    if key(cmd_shift, Key::P) {
        c.state.palette = Some(crate::state::Palette {
            mode: crate::state::PaletteMode::Commands,
            query: String::new(),
            index: 0,
        });
    }
    if key(cmd, Key::P) {
        c.state.palette = Some(crate::state::Palette {
            mode: crate::state::PaletteMode::Workspaces,
            query: String::new(),
            index: 0,
        });
    }

    if key(cmd, Key::D) {
        split(c, &focused_pane, &active_surface, "right");
    }
    if key(cmd, Key::T) {
        if let Some(pane) = &focused_pane {
            c.state.new_terminal_chip(pane);
        }
    }
    if key(cmd, Key::W) {
        // Closes the focused pane's active chip, with focus following the close.
        c.state.close_active_chip();
    }
    if key(cmd, Key::N) {
        let folder = c.state.default_new_folder();
        c.state.new_session(folder);
    }
    if key(cmd, Key::B) {
        c.state.sidebar_hidden = !c.state.sidebar_hidden;
    }
    if key(cmd, Key::I) {
        c.state.notif_open = !c.state.notif_open;
    }
    if key(cmd, Key::R) {
        if let Some(sid) = &active_surface {
            let title = c.state.surface(sid).map(|s| s.title.clone()).unwrap_or_default();
            c.state.begin_rename(sid, &title);
        }
    }
    // History: back/forward through the workspaces you visited.
    if key(cmd, Key::OpenBracket) {
        c.state.navigate(-1);
    }
    if key(cmd, Key::CloseBracket) {
        c.state.navigate(1);
    }
    // Terminal font size.
    if key(cmd, Key::Plus) || key(cmd, Key::Equals) {
        c.state.zoom_terminal(1.0);
    }
    if key(cmd, Key::Minus) {
        c.state.zoom_terminal(-1.0);
    }
    if key(cmd, Key::Num0) {
        c.state.zoom_terminal(0.0);
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
    for (i, k) in digits.iter().enumerate() {
        // Cmd+1…9: the n-th workspace as the sidebar shows them (9: the last).
        if key(cmd, *k) {
            let order = c.state.sidebar_order();
            let pick = if i == 8 { order.last() } else { order.get(i) };
            if let Some(id) = pick.cloned() {
                c.state.select_workspace(&id);
            }
        }
        // Ctrl+1…9: the n-th tab of the focused pane.
        if key(ctrl, *k) {
            c.state.select_tab_index(i);
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

/// The "Add Remote Host" dialog: one field for an SSH target. Adding it saves
/// the host and connects; the daemon must already run there (see
/// `deploy/install-remote.sh`).
fn add_host_dialog(ctx: &egui::Context, c: &mut ui::Ctx) {
    let Some(mut draft) = c.state.add_host_draft.clone() else { return };
    let mut add = false;
    let mut cancel = false;
    egui::Window::new("Add Remote Host")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(Vec2::new(320.0, 0.0))
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new("SSH alias or user@host. The host needs lmux running (deploy/install-remote.sh).")
                    .font(theme::ui(theme::FS_SMALL))
                    .color(theme::DIM),
            );
            ui.add_space(6.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut draft)
                    .id(Id::new("add-host-field"))
                    .hint_text("my-server")
                    .desired_width(f32::INFINITY),
            );
            if !edit.has_focus() && !edit.lost_focus() {
                edit.request_focus();
            }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                add = true;
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Add").clicked() {
                    add = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                cancel = true;
            }
        });
    if add && !draft.trim().is_empty() {
        c.state.actions.push(Action::AddHost(draft.trim().to_owned()));
        c.state.add_host_draft = None;
    } else if cancel {
        c.state.add_host_draft = None;
    } else {
        c.state.add_host_draft = Some(draft);
    }
}

/// Run a palette command on the host on screen. Names are those in
/// `ui::palette::COMMANDS`.
pub fn run_command(c: &mut ui::Ctx, name: &str) {
    let focused_pane = c.state.focused_pane();
    let active_surface = focused_pane
        .as_deref()
        .and_then(|p| c.state.pane(p))
        .and_then(|p| c.state.active_surface(p));
    match name {
        "New Workspace" => {
            let folder = c.state.default_new_folder();
            c.state.new_session(folder);
        }
        "Rename Workspace" => c.state.begin_ws_rename(None),
        "Close Workspace" => {
            if let Some(ws) = c.state.selected.clone() {
                c.state.rpc(m::WORKSPACE_CLOSE, serde_json::json!({ "workspace": ws }));
            }
        }
        "Next Workspace" => c.state.cycle_workspace(1),
        "Previous Workspace" => c.state.cycle_workspace(-1),
        "New Terminal Tab" => {
            if let Some(p) = &focused_pane {
                c.state.new_terminal_chip(p);
            }
        }
        "New Browser Tab" => {
            if let Some(p) = &focused_pane {
                c.state.new_browser_chip(p);
            }
        }
        "Rename Tab" => {
            if let Some(sid) = &active_surface {
                let title = c.state.surface(sid).map(|s| s.title.clone()).unwrap_or_default();
                c.state.begin_rename(sid, &title);
            }
        }
        "Close Tab" => c.state.close_active_chip(),
        "Next Tab" => c.state.cycle_tab(1),
        "Previous Tab" => c.state.cycle_tab(-1),
        "Split Right" => split(c, &focused_pane, &active_surface, "right"),
        "Split Down" => split(c, &focused_pane, &active_surface, "down"),
        "Toggle Pane Zoom" => c.state.toggle_zoom(),
        "Toggle Sidebar" => c.state.sidebar_hidden = !c.state.sidebar_hidden,
        "Show Notifications" => c.state.notif_open = true,
        "Jump to Latest Unread" => {
            c.state.jump_to_unread();
        }
        "New Folder" => {
            let parent = c.state.default_new_folder();
            c.state.new_folder(parent);
        }
        "Add Remote Host…" => c.state.add_host_draft = Some(String::new()),
        "Zoom In" => c.state.zoom_terminal(1.0),
        "Zoom Out" => c.state.zoom_terminal(-1.0),
        "Reset Zoom" => c.state.zoom_terminal(0.0),
        _ => {}
    }
}
