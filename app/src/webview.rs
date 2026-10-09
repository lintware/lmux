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
}

impl Webviews {
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

        for (surface, cmd) in std::mem::take(&mut state.native_cmds) {
            let Some(v) = self.views.get(&(host.to_owned(), surface)) else { continue };
            let _ = match cmd {
                NativeCmd::Back => v.webview.evaluate_script("history.back()"),
                NativeCmd::Forward => v.webview.evaluate_script("history.forward()"),
                NativeCmd::Reload => v.webview.reload(),
                NativeCmd::Stop => v.webview.evaluate_script("window.stop()"),
                NativeCmd::Goto(url) => v.webview.load_url(&url),
            };
        }

        self.run_calls(window, ctx, host, state, &live);
        self.apply_events(host, state);
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
        let url = state.browser_url(surface).unwrap_or_else(|| "about:blank".to_owned());
        match self.build(window, ctx, host, surface, &url, rect, ctx.zoom_factor()) {
            Ok(webview) => {
                self.views.insert(key, View { webview, bounds: rect, committed: false });
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
                    let r = v
                        .webview
                        .evaluate_script_with_callback(&script, move |raw| push(Event::Answer(id, Ok(Value::String(raw)))));
                    if let Err(e) = r {
                        answer(state, id, Err(e.to_string()));
                    }
                }
                AppBrowserOp::Navigate { url } => answer(state, id, done(v.webview.load_url(&url))),
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
    ) -> wry::Result<WebView> {
        let push = pusher(&self.events, ctx, host, surface);
        let (p1, p2, p3, p4) = (push.clone(), push.clone(), push.clone(), push);
        WebViewBuilder::new()
            .with_url(url)
            .with_bounds(bounds(rect, zoom))
            .with_accept_first_mouse(true)
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
    fn apply_events(&mut self, host: &str, state: &mut AppState) {
        let mine: Vec<(Ref, Event)> = {
            let mut all = self.events.lock().unwrap();
            let (mine, rest): (Vec<_>, Vec<_>) = all.drain(..).partition(|(h, _, _)| h == host);
            *all = rest;
            mine.into_iter().map(|(_, s, e)| (s, e)).collect()
        };
        for (surface, event) in mine {
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
