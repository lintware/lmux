//! Browser panes drawn by the app itself: a native WebKit view (WKWebView, via
//! wry) laid over the pane's content area, as cmux does it. The daemon only
//! keeps such a browser's URL and title (`in_app` surfaces); the page lives
//! here.
//!
//! egui draws the toolbar and records where the page goes
//! ([`AppState::native_rects`]); after the frame, [`Webviews::sync`] creates,
//! moves, shows and hides the native views to match, runs the toolbar's
//! commands and an agent's steps ([`AppState::app_calls`]), and turns WebKit's
//! callbacks into [`AppState`] updates and answers to the daemon.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use lmux_proto::methods as m;
use lmux_proto::{AppBrowserOp, ClientMsg, Ref};
use serde_json::{json, Value};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::raw_window_handle::HasWindowHandle;
use wry::{NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder, WebViewExtMacOS};

use crate::state::{AppState, NativeCmd};

/// Something a page did, reported from WebKit's callbacks.
enum Event {
    Loading(bool, String),
    Title(String),
    /// `window.open` or a `target=_blank` link: a new tab in the same pane.
    NewTab(String),
    /// The page was clicked: its pane takes focus.
    Clicked,
    /// The answer to an agent's step.
    Answer(u64, Result<Value, String>),
}

/// Tells the app when the page is clicked, since clicks on a native view never
/// reach egui.
const CLICK_SCRIPT: &str = "window.addEventListener('mousedown', () => window.ipc.postMessage('click'), true);";

/// A view an agent drives before the pane is on screen gets this size, so the
/// page lays out (and screenshots) as it would in a pane.
const HIDDEN_SIZE: egui::Vec2 = egui::vec2(1280.0, 800.0);

/// Where a view nobody should see goes: outside the window, at its usual size.
/// Hiding a WKWebView instead lets WebKit suspend its page (a new site's page
/// starts suspended), and an agent driving it would get no answers.
fn parked(size: egui::Vec2) -> egui::Rect {
    egui::Rect::from_min_size(egui::pos2(-30000.0, 0.0), size)
}

/// How long an agent's step waits for its surface to reach the tree.
const CALL_WAIT: Duration = Duration::from_secs(3);

struct View {
    webview: WebView,
    /// Where it is now: its pane, or [`parked`].
    bounds: egui::Rect,
    /// Its first page has started (committed). Until then wry holds scripts
    /// back and runs them later without their callbacks, so an agent's
    /// script would never be answered: those steps wait for this.
    committed: bool,
}

type Events = Arc<Mutex<Vec<(String, Ref, Event)>>>;

/// Every native browser, by (host, surface).
#[derive(Default)]
pub struct Webviews {
    views: HashMap<(String, Ref), View>,
    events: Events,
    /// Remote hosts whose pages run here: each has its own WebKit data store
    /// (cookies, storage) whose traffic goes through its SOCKS relay.
    remotes: HashMap<String, Remote>,
    /// Who last got the window's keyboard focus from [`Webviews::sync_focus`].
    focus: Option<FirstResponder>,
}

/// Who should be the window's first responder: the winit view (egui and
/// terminals) or one page's WKWebView.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirstResponder {
    Parent,
    Page(String, Ref),
}

/// The first responder for this frame. The page of the focused pane gets it,
/// unless egui needs the keys (a menu, dialog, address bar) or the focused
/// pane is not a native browser.
pub fn wanted_first_responder(host: &str, focused_native: Option<&str>, egui_has_keys: bool) -> FirstResponder {
    match focused_native {
        Some(s) if !egui_has_keys => FirstResponder::Page(host.to_owned(), s.to_owned()),
        _ => FirstResponder::Parent,
    }
}

/// A remote host's browser network: the relay to its tunnel, and the data
/// store WebKit uses for its pages.
struct Remote {
    relay: crate::socks::Relay,
    store_id: [u8; 16],
    /// WebKit keeps a store's proxy only on the object it was set on: a
    /// store freed and fetched again by id comes back without it.
    _store: objc2::rc::Retained<objc2_web_kit::WKWebsiteDataStore>,
}

/// What a page needs to belong to a remote host.
#[derive(Clone, Copy)]
struct Route {
    store_id: [u8; 16],
}

/// WebKit takes a proxy per data store only from macOS 14.
pub fn proxies_supported() -> bool {
    use objc2_foundation::NSProcessInfo;
    NSProcessInfo::processInfo().operatingSystemVersion().majorVersion >= 14
}

/// A stable data store id for a host, so its logins survive restarts.
fn store_id(host: &str) -> [u8; 16] {
    use std::hash::{Hash, Hasher};
    let mut out = [0u8; 16];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        ("lmux-browser-store", i, host).hash(&mut h);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    // An RFC 4122 version 4 layout, so WebKit takes it as a plain UUID.
    out[6] = (out[6] & 0x0F) | 0x40;
    out[8] = (out[8] & 0x3F) | 0x80;
    out
}

/// The data store `id`, pointed at the SOCKS5 proxy on `127.0.0.1:port`.
/// Network.framework's C API, loaded at run time so the app still starts on
/// macOS 12 and 13.
fn proxied_store(
    id: [u8; 16],
    port: u16,
) -> Result<objc2::rc::Retained<objc2_web_kit::WKWebsiteDataStore>, String> {
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, MainThreadMarker};
    use objc2_foundation::{NSString, NSUUID};
    use objc2_web_kit::WKWebsiteDataStore;

    type EndpointFn = unsafe extern "C" fn(*const std::ffi::c_char, *const std::ffi::c_char) -> *mut AnyObject;
    type SocksFn = unsafe extern "C" fn(*mut AnyObject) -> *mut AnyObject;
    extern "C" {
        fn dlsym(handle: *mut std::ffi::c_void, symbol: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    }
    // RTLD_DEFAULT on macOS.
    let default = -2isize as *mut std::ffi::c_void;
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    // SAFETY: both symbols are Network.framework C functions with these
    // signatures (macOS 14+); null checks guard older systems. The objects
    // they return are retained and handed to KVC, which retains them again.
    unsafe {
        let ep = dlsym(default, c"nw_endpoint_create_host".as_ptr());
        let socks = dlsym(default, c"nw_proxy_config_create_socksv5".as_ptr());
        if ep.is_null() || socks.is_null() {
            return Err("this macOS has no per-store proxies".into());
        }
        let ep: EndpointFn = std::mem::transmute(ep);
        let socks: SocksFn = std::mem::transmute(socks);
        let port = std::ffi::CString::new(port.to_string()).map_err(|e| e.to_string())?;
        let endpoint = ep(c"127.0.0.1".as_ptr(), port.as_ptr());
        if endpoint.is_null() {
            return Err("no proxy endpoint".into());
        }
        let config = socks(endpoint);
        if config.is_null() {
            return Err("no proxy config".into());
        }
        let store = WKWebsiteDataStore::dataStoreForIdentifier(&NSUUID::from_bytes(id), mtm);
        let list: *mut AnyObject = msg_send![objc2::class!(NSArray), arrayWithObject: config];
        let key = NSString::from_str("proxyConfigurations");
        let _: () = msg_send![&*store, setValue: list, forKey: &*key];
        Ok(store)
    }
}

impl Webviews {
    /// Say whether `host`'s pages run here with its network: `socks` is the
    /// SOCKS port of its ssh tunnel while it is up. Call every frame, before
    /// [`Webviews::place`] and [`Webviews::service`].
    pub fn set_network(&mut self, host: &str, remote: bool, socks: Option<u16>) {
        if !remote {
            return;
        }
        if !self.remotes.contains_key(host) {
            let relay = match crate::socks::Relay::start() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[webview] {host}: no SOCKS relay: {e}");
                    return;
                }
            };
            let store_id = store_id(host);
            let store = match proxied_store(store_id, relay.port()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[webview] {host}: {e}");
                    return;
                }
            };
            self.remotes.insert(host.to_owned(), Remote { relay, store_id, _store: store });
        }
        if let Some(r) = self.remotes.get(host) {
            r.relay.set_upstream(socks.unwrap_or(0));
        }
    }

    /// Make AppKit's first responder follow lmux's focus, so typed keys and
    /// Cmd+V reach the focused pane: its page when that is a native browser
    /// on `host`, the winit view otherwise. Call every frame after
    /// [`Webviews::place`], for the host on screen.
    pub fn sync_focus(&mut self, host: &str, focused_native: Option<&str>, egui_has_keys: bool) {
        let want = wanted_first_responder(host, focused_native, egui_has_keys);
        match &want {
            FirstResponder::Parent => {
                // Checked against AppKit every frame: a page can regain the
                // first responder behind our back (a new view, the window
                // becoming key again).
                if let Some(v) = self.views.values().find(|v| holds_focus(&v.webview)) {
                    let _ = v.webview.focus_parent();
                }
                self.focus = Some(want);
            }
            FirstResponder::Page(h, s) => {
                if self.focus.as_ref() == Some(&want) {
                    return;
                }
                // Not built yet: try again next frame.
                if let Some(v) = self.views.get(&(h.clone(), s.clone())) {
                    let _ = v.webview.focus();
                    self.focus = Some(want);
                }
            }
        }
    }

    fn route(&self, host: &str) -> Option<Route> {
        self.remotes.get(host).map(|r| Route { store_id: r.store_id })
    }

    /// Match the native views of `host` to this frame: create the ones newly
    /// on screen, place them, and park the rest. With `hide_all` (a menu,
    /// dialog or drag over the panes) all are parked, since a native view
    /// always draws above egui. Called after drawing.
    pub fn place(
        &mut self,
        window: &impl HasWindowHandle,
        ctx: &egui::Context,
        host: &str,
        state: &mut AppState,
        hide_all: bool,
    ) {
        let zoom = ctx.zoom_factor();
        let rects = std::mem::take(&mut state.native_rects);
        for (surface, rect) in &rects {
            if !self.ensure(window, ctx, host, state, surface, *rect) {
                continue;
            }
            let v = self.views.get_mut(&(host.to_owned(), surface.clone())).expect("ensured");
            let target = if hide_all { parked(rect.size()) } else { *rect };
            if v.bounds != target {
                let _ = v.webview.set_bounds(bounds(target, zoom));
                v.bounds = target;
            }
        }

        // The toolbar's commands run in `service`, on the next pass.
        if !state.native_cmds.is_empty() {
            ctx.request_repaint();
        }

        // Off screen this frame: parked.
        for ((h, s), v) in self.views.iter_mut() {
            if h == host && !rects.contains_key(s) && v.bounds != parked(v.bounds.size()) {
                v.bounds = parked(v.bounds.size());
                let _ = v.webview.set_bounds(bounds(v.bounds, zoom));
            }
        }
    }

    /// Everything but placement, which must happen even while the window is
    /// covered or minimised and not drawn: the toolbar's commands, an agent's
    /// steps, WebKit's callbacks, and dropping the views of closed tabs.
    pub fn service(&mut self, window: &impl HasWindowHandle, ctx: &egui::Context, host: &str, state: &mut AppState) {
        let live: HashSet<Ref> = state
            .tree
            .workspaces
            .iter()
            .flat_map(|w| w.panes.iter())
            .flat_map(|p| p.surfaces.iter())
            .map(|s| s.id.clone())
            .collect();
        // An empty tree is one not loaded yet (or reconnecting): keep all.
        self.views.retain(|(h, s), _| h != host || live.contains(s) || live.is_empty());

        let route = self.route(host);
        for (surface, cmd) in std::mem::take(&mut state.native_cmds) {
            let Some(v) = self.views.get(&(host.to_owned(), surface)) else { continue };
            let _ = match cmd {
                NativeCmd::Back => v.webview.evaluate_script("history.back()"),
                NativeCmd::Forward => v.webview.evaluate_script("history.forward()"),
                NativeCmd::Reload => v.webview.reload(),
                NativeCmd::Stop => v.webview.evaluate_script("window.stop()"),
                NativeCmd::Goto(url) => v.webview.load_url(&page_url(route, &url)),
            };
        }

        self.run_calls(window, ctx, host, state, &live);
        self.apply_events(ctx, host, state);
    }

    /// The view for `surface`, made now if there is none: placed at `rect`,
    /// at the page the daemon has for it. False if WebKit would not make it.
    fn ensure(
        &mut self,
        window: &impl HasWindowHandle,
        ctx: &egui::Context,
        host: &str,
        state: &AppState,
        surface: &str,
        rect: egui::Rect,
    ) -> bool {
        let key = (host.to_owned(), surface.to_owned());
        if self.views.contains_key(&key) {
            return true;
        }
        let route = self.route(host);
        let url = state.browser_url(surface).unwrap_or_else(|| "about:blank".to_owned());
        let url = page_url(route, &url);
        match self.build(window, ctx, host, surface, &url, rect, ctx.zoom_factor(), route) {
            Ok(webview) => {
                self.views.insert(key, View { webview, bounds: rect, committed: false });
                // wry made the new view the first responder.
                self.focus = None;
                true
            }
            Err(e) => {
                eprintln!("[webview] {surface}: {e}");
                false
            }
        }
    }

    /// An agent's steps for `host`'s browsers. A browser not on screen gets a
    /// parked view, so agents can work in workspaces nobody is looking at.
    fn run_calls(
        &mut self,
        window: &impl HasWindowHandle,
        ctx: &egui::Context,
        host: &str,
        state: &mut AppState,
        live: &HashSet<Ref>,
    ) {
        let calls = std::mem::take(&mut state.app_calls);
        for call in calls {
            // A browser just opened may reach the tree after the step that
            // drives it; its page is not known until then.
            if !live.contains(&call.surface) {
                if call.since.elapsed() < CALL_WAIT {
                    state.app_calls.push(call);
                    ctx.request_repaint_after(Duration::from_millis(50));
                } else {
                    answer(state, call.id, Err(format!("no browser {} here", call.surface)));
                }
                continue;
            }
            let key = (host.to_owned(), call.surface.clone());
            if !self.ensure(window, ctx, host, state, &call.surface, parked(HIDDEN_SIZE)) {
                answer(state, call.id, Err("WebKit could not make a view".into()));
                continue;
            }
            let v = &self.views[&key];
            let scripted = matches!(call.op, AppBrowserOp::Eval { .. } | AppBrowserOp::Screenshot);
            if scripted && !v.committed {
                state.app_calls.push(call);
                ctx.request_repaint_after(Duration::from_millis(50));
                continue;
            }
            let id = call.id;
            let done = |r: wry::Result<()>| r.map(|_| json!({})).map_err(|e| e.to_string());
            match call.op {
                AppBrowserOp::Eval { script } => {
                    let push = pusher(&self.events, ctx, host, &call.surface);
                    // Agents read the host's own name for its loopback.
                    let remote = self.route(host).is_some();
                    let r = v.webview.evaluate_script_with_callback(&script, move |raw| {
                        let raw = if remote { crate::socks::unalias_text(&raw) } else { raw };
                        push(Event::Answer(id, Ok(Value::String(raw))))
                    });
                    if let Err(e) = r {
                        answer(state, id, Err(e.to_string()));
                    }
                }
                AppBrowserOp::Navigate { url } => {
                    answer(state, id, done(v.webview.load_url(&page_url(self.route(host), &url))))
                }
                AppBrowserOp::Back => answer(state, id, done(v.webview.evaluate_script("history.back()"))),
                AppBrowserOp::Forward => answer(state, id, done(v.webview.evaluate_script("history.forward()"))),
                AppBrowserOp::Reload => answer(state, id, done(v.webview.reload())),
                AppBrowserOp::Stop => answer(state, id, done(v.webview.evaluate_script("window.stop()"))),
                AppBrowserOp::Screenshot => {
                    let push = pusher(&self.events, ctx, host, &call.surface);
                    snapshot(&v.webview, move |r| push(Event::Answer(id, r)));
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        window: &impl HasWindowHandle,
        ctx: &egui::Context,
        host: &str,
        surface: &str,
        url: &str,
        rect: egui::Rect,
        zoom: f32,
        route: Option<Route>,
    ) -> wry::Result<WebView> {
        use wry::WebViewBuilderExtDarwin;
        let push = pusher(&self.events, ctx, host, surface);
        let (p1, p2, p3, p4) = (push.clone(), push.clone(), push.clone(), push);
        let mut builder = WebViewBuilder::new();
        if let Some(r) = route {
            builder = builder.with_data_store_identifier(r.store_id);
        }
        builder
            .with_url(url)
            .with_bounds(bounds(rect, zoom))
            .with_accept_first_mouse(true)
            // wry makes each new view the first responder on macOS whatever
            // this says; `sync_focus` hands it back right after.
            .with_focused(false)
            .with_devtools(true)
            .with_initialization_script(CLICK_SCRIPT)
            .with_initialization_script(lmux_proto::browser_js::CAPTURE_INIT)
            .with_on_page_load_handler(move |e, url| p1(Event::Loading(matches!(e, PageLoadEvent::Started), url)))
            .with_document_title_changed_handler(move |t| p2(Event::Title(t)))
            .with_new_window_req_handler(move |url, _| {
                p3(Event::NewTab(url));
                NewWindowResponse::Deny
            })
            .with_ipc_handler(move |req| {
                if req.body() == "click" {
                    p4(Event::Clicked);
                }
            })
            .build_as_child(window)
    }

    /// WebKit's callbacks for `host`, applied to the toolbar, reported to the
    /// daemon so the tab comes back at its page after a restart, and agents'
    /// answers sent back.
    fn apply_events(&mut self, ctx: &egui::Context, host: &str, state: &mut AppState) {
        let mine: Vec<(Ref, Event)> = {
            let mut all = self.events.lock().unwrap();
            let (mine, rest): (Vec<_>, Vec<_>) = all.drain(..).partition(|(h, _, _)| h == host);
            *all = rest;
            mine.into_iter().map(|(_, s, e)| (s, e)).collect()
        };
        let remote = self.route(host).is_some();
        for (surface, event) in mine {
            // A remote host's page asks for its `localhost` by another name;
            // the toolbar and the daemon see the real one.
            let event = match event {
                Event::Loading(l, url) if remote => Event::Loading(l, crate::socks::from_page_url(&url)),
                Event::NewTab(url) if remote => Event::NewTab(crate::socks::from_page_url(&url)),
                e => e,
            };
            match event {
                Event::Loading(loading, url) => {
                    if let Some(v) = self.views.get_mut(&(host.to_owned(), surface.clone())) {
                        v.committed = true;
                    }
                    let b = state.ensure_browser(&surface, &url);
                    b.loading = loading;
                    b.url = url.clone();
                    b.can_back = true;
                    b.can_fwd = true;
                    state.rpc(m::BROWSER_REPORT, json!({ "surface": surface, "url": url, "loading": loading }));
                }
                Event::Title(title) => {
                    state.rpc(m::BROWSER_REPORT, json!({ "surface": surface, "title": title }));
                }
                Event::NewTab(url) => {
                    if let Some(pane) = state.pane_of_surface(&surface) {
                        state.open_browser_tab(&pane, &url);
                    }
                }
                Event::Clicked => {
                    if let Some(v) = self.views.get(&(host.to_owned(), surface.clone())) {
                        let _ = v.webview.focus();
                        self.focus = Some(FirstResponder::Page(host.to_owned(), surface.clone()));
                        // A click on the page never reaches egui, so a field
                        // there would keep the keys.
                        ctx.memory_mut(|m| {
                            if let Some(id) = m.focused() {
                                m.surrender_focus(id);
                            }
                        });
                    }
                    if let Some(pane) = state.pane_of_surface(&surface) {
                        state.focus_pane(&pane);
                        state.select_tab(&pane, &surface);
                    }
                }
                Event::Answer(id, r) => answer(state, id, r),
            }
        }
    }
}

/// Whether this page's WKWebView is its window's first responder.
fn holds_focus(webview: &WebView) -> bool {
    use objc2::rc::Retained;
    use objc2_app_kit::NSResponder;
    let view = webview.webview();
    let Some(window) = view.window() else { return false };
    let Some(current) = window.firstResponder() else { return false };
    let page: &NSResponder = &view;
    std::ptr::eq(Retained::as_ptr(&current), page)
}

/// The URL a page loads: a remote host's loopback goes to the host.
fn page_url(route: Option<Route>, url: &str) -> String {
    match route {
        Some(_) => crate::socks::to_page_url(url),
        None => url.to_owned(),
    }
}

/// A callback-side sender of events for one (host, surface).
fn pusher(events: &Events, ctx: &egui::Context, host: &str, surface: &str) -> impl Fn(Event) + Clone + Send + 'static {
    let (events, ctx, host, surface) = (events.clone(), ctx.clone(), host.to_owned(), surface.to_owned());
    move |e: Event| {
        events.lock().unwrap().push((host.clone(), surface.clone(), e));
        ctx.request_repaint();
    }
}

fn answer(state: &mut AppState, id: u64, r: Result<Value, String>) {
    let (value, error) = match r {
        Ok(v) => (v, None),
        Err(e) => (Value::Null, Some(e)),
    };
    state.pending.push(ClientMsg::AppBrowserResult { id, value, error });
}

/// The visible page as a PNG, `{"png_base64": ...}`, from WKWebView's own
/// snapshot (it works for a hidden view too).
fn snapshot(webview: &WebView, done: impl Fn(Result<Value, String>) + 'static) {
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
    use objc2_foundation::{NSDictionary, NSError};

    let wk = webview.webview();
    let handler = block2::RcBlock::new(move |image: *mut NSImage, _err: *mut NSError| {
        // SAFETY: WebKit passes a valid (or null) image for the call's length.
        let png = unsafe { image.as_ref() }
            .and_then(|img| img.TIFFRepresentation())
            .and_then(|tiff| NSBitmapImageRep::imageRepWithData(&tiff))
            .and_then(|rep| unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) });
        done(match png {
            Some(data) => Ok(json!({ "png_base64": base64::engine::general_purpose::STANDARD.encode(data.to_vec()) })),
            None => Err("WebKit gave no snapshot".into()),
        });
    });
    // SAFETY: a nil configuration snapshots the visible bounds.
    unsafe { wk.takeSnapshotWithConfiguration_completionHandler(None, &handler) };
}

/// An egui rect (points) as a wry rect (logical pixels of the window).
fn bounds(rect: egui::Rect, zoom: f32) -> wry::Rect {
    wry::Rect {
        position: LogicalPosition::new((rect.min.x * zoom) as f64, (rect.min.y * zoom) as f64).into(),
        size: LogicalSize::new((rect.width() * zoom).max(1.0) as f64, (rect.height() * zoom).max(1.0) as f64).into(),
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;

    #[test]
    fn page_takes_focus_only_when_its_pane_is_focused_and_egui_wants_no_keys() {
        assert_eq!(wanted_first_responder("h", Some("surface:2"), false), FirstResponder::Page("h".into(), "surface:2".into()));
        assert_eq!(wanted_first_responder("h", Some("surface:2"), true), FirstResponder::Parent);
        assert_eq!(wanted_first_responder("h", None, false), FirstResponder::Parent);
    }
}
