//! Browser panes drawn by the app itself: a native WebKit view (WKWebView, via
//! wry) laid over the pane's content area, as cmux does it. The daemon only
//! keeps such a browser's URL and title (`in_app` surfaces); the page lives
//! here.
//!
//! egui draws the toolbar and records where the page goes
//! ([`AppState::native_rects`]); after the frame, [`Webviews::sync`] creates,
//! moves, shows and hides the native views to match, runs the toolbar's
//! commands, and turns WebKit's callbacks into [`AppState`] updates.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use lmux_proto::methods as m;
use lmux_proto::Ref;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::raw_window_handle::HasWindowHandle;
use wry::{NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder};

use crate::state::{AppState, NativeCmd};

/// Something a page did, reported from WebKit's callbacks.
enum Event {
    Loading(bool, String),
    Title(String),
    /// `window.open` or a `target=_blank` link: a new tab in the same pane.
    NewTab(String),
    /// The page was clicked: its pane takes focus.
    Clicked,
}

/// Tells the app when the page is clicked, since clicks on a native view never
/// reach egui.
const INIT_SCRIPT: &str = "window.addEventListener('mousedown', () => window.ipc.postMessage('click'), true);";

struct View {
    webview: WebView,
    shown: bool,
    bounds: Option<egui::Rect>,
}

/// Every native browser, by (host, surface).
#[derive(Default)]
pub struct Webviews {
    views: HashMap<(String, Ref), View>,
    events: Arc<Mutex<Vec<(String, Ref, Event)>>>,
}

impl Webviews {
    /// Match the native views of `host` to this frame: create the ones newly
    /// on screen, place and show them, and hide the rest. With `hide_all`
    /// (a menu, dialog or drag over the panes) none is shown, since a native
    /// view always draws above egui.
    #[allow(clippy::too_many_arguments)]
    pub fn sync(
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
            let key = (host.to_owned(), surface.clone());
            if !self.views.contains_key(&key) {
                let url = state.browser_url(surface).unwrap_or_else(|| "about:blank".to_owned());
                match self.build(window, ctx, host, surface, &url, *rect, zoom) {
                    Ok(webview) => {
                        self.views.insert(key.clone(), View { webview, shown: true, bounds: Some(*rect) });
                    }
                    Err(e) => {
                        eprintln!("[webview] {surface}: {e}");
                        continue;
                    }
                }
            }
            let v = self.views.get_mut(&key).expect("view inserted above");
            if v.bounds != Some(*rect) {
                let _ = v.webview.set_bounds(bounds(*rect, zoom));
                v.bounds = Some(*rect);
            }
            let show = !hide_all;
            if v.shown != show {
                let _ = v.webview.set_visible(show);
                v.shown = show;
            }
        }

        // Off screen this frame: hidden. Closed: dropped, which removes it.
        let live: HashSet<Ref> = state
            .tree
            .workspaces
            .iter()
            .flat_map(|w| w.panes.iter())
            .flat_map(|p| p.surfaces.iter())
            .map(|s| s.id.clone())
            .collect();
        self.views.retain(|(h, s), v| {
            if h != host {
                return true;
            }
            if !rects.contains_key(s) && v.shown {
                let _ = v.webview.set_visible(false);
                v.shown = false;
            }
            // An empty tree is one not loaded yet (or reconnecting): keep.
            live.contains(s) || live.is_empty()
        });

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

        self.apply_events(host, state);
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
        let tag = (host.to_owned(), surface.to_owned());
        let push = {
            let events = self.events.clone();
            let ctx = ctx.clone();
            move |e: Event| {
                events.lock().unwrap().push((tag.0.clone(), tag.1.clone(), e));
                ctx.request_repaint();
            }
        };
        let (p1, p2, p3, p4) = (push.clone(), push.clone(), push.clone(), push);
        WebViewBuilder::new()
            .with_url(url)
            .with_bounds(bounds(rect, zoom))
            .with_accept_first_mouse(true)
            .with_devtools(true)
            .with_initialization_script(INIT_SCRIPT)
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

    /// WebKit's callbacks for `host`, applied to the toolbar and reported to
    /// the daemon so the tab comes back at its page after a restart.
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
                    if let Some(b) = state.browsers.get_mut(&surface) {
                        b.loading = loading;
                        b.url = url.clone();
                        b.can_back = true;
                        b.can_fwd = true;
                    }
                    state.rpc(m::BROWSER_REPORT, serde_json::json!({ "surface": surface, "url": url, "loading": loading }));
                }
                Event::Title(title) => {
                    state.rpc(m::BROWSER_REPORT, serde_json::json!({ "surface": surface, "title": title }));
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
            }
        }
    }
}

/// An egui rect (points) as a wry rect (logical pixels of the window).
fn bounds(rect: egui::Rect, zoom: f32) -> wry::Rect {
    wry::Rect {
        position: LogicalPosition::new((rect.min.x * zoom) as f64, (rect.min.y * zoom) as f64).into(),
        size: LogicalSize::new((rect.width() * zoom).max(1.0) as f64, (rect.height() * zoom).max(1.0) as f64).into(),
    }
}
