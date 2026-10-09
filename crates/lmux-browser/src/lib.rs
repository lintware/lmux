//! lmux-browser: the server-side browser engine for lmux (milestone M3).
//!
//! Drives one headless Chromium over CDP. Each lmux *surface* is one page target;
//! agents command it with cmux-style `browser.*` RPCs and a viewer watches the
//! same page through a JPEG screencast whose cadence is driven by client acks
//! (see [`BrowserManager::ack_frame`]).
//!
//! See `scratchpad/notes-browser.md` for design decisions and deviations.

mod cdp;
mod chromium;
mod js;
mod surface;

pub use chromium::BrowserConfig;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};
use tokio::process::Child;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};

use lmux_proto::methods as m;
use lmux_proto::{BrowserInputEvent, KeyAction, Modifiers, MouseAction, MouseButton, RpcError, RpcErrorCode};

use cdp::{CdpConnection, CdpError, CdpEvent};
use surface::{deliver_on_ack, InFlight, LatestFrame, Screencast, SurfaceState, Tab};

/// Events a daemon/app can consume. The definition lives in `lmux-proto` so the
/// engine and the daemon share one type without depending on each other; it is
/// re-exported here to keep `lmux_browser::BrowserEvent` stable.
pub use lmux_proto::BrowserEvent;

// ---------------------------------------------------------------------------
// RPC error helpers
// ---------------------------------------------------------------------------

fn err_not_found(msg: impl Into<String>) -> RpcError {
    RpcError { code: RpcErrorCode::NotFound, message: msg.into() }
}
fn err_invalid(msg: impl Into<String>) -> RpcError {
    RpcError { code: RpcErrorCode::InvalidParams, message: msg.into() }
}
fn err_internal(msg: impl Into<String>) -> RpcError {
    RpcError { code: RpcErrorCode::Internal, message: msg.into() }
}
fn err_unsupported(msg: impl Into<String>) -> RpcError {
    RpcError { code: RpcErrorCode::Unsupported, message: msg.into() }
}
fn err_unknown(method: &str) -> RpcError {
    RpcError { code: RpcErrorCode::UnknownMethod, message: format!("unknown browser method `{method}`") }
}
fn rpc_from_cdp(e: CdpError) -> RpcError {
    match e {
        CdpError::Closed => err_internal("CDP connection closed"),
        CdpError::Io(m) => err_internal(m),
        CdpError::Remote { code, message } => match code {
            -32601 => err_unsupported(message),
            -32602 => err_invalid(message),
            _ => err_internal(format!("CDP error {code}: {message}")),
        },
    }
}

// ---------------------------------------------------------------------------
// Parameter helpers
// ---------------------------------------------------------------------------

fn pstr(p: &Value, k: &str) -> Option<String> {
    p.get(k).and_then(Value::as_str).map(str::to_string)
}
fn req(p: &Value, k: &str) -> Result<String, RpcError> {
    pstr(p, k).ok_or_else(|| err_invalid(format!("missing required param `{k}`")))
}
/// `e12` -> `[data-lmux-ref="e12"]`; anything else is treated as a CSS selector.
fn is_ref(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() >= 2 && b[0] == b'e' && b[1..].iter().all(u8::is_ascii_digit)
}
fn target_selector(target: &str) -> String {
    if is_ref(target) {
        format!("[data-lmux-ref=\"{target}\"]")
    } else {
        target.to_string()
    }
}

// ---------------------------------------------------------------------------
// Screencast pacing / adaptation
// ---------------------------------------------------------------------------

/// Above this EWMA ack latency (ms) the client is behind: reduce quality/size.
const EWMA_DEGRADE_MS: f32 = 120.0;
/// Below this EWMA ack latency (ms) the client has caught up: restore quality.
const EWMA_RESTORE_MS: f32 = 50.0;
/// Do not reconfigure more often than this (avoids flapping and churn on the CDP
/// session).
const RECONFIG_COOLDOWN_MS: u64 = 1000;
const MAX_LEVEL: u32 = 2;
/// Upper bound on `everyNthFrame` (Chrome would drop to 2 fps at this point).
const MAX_EVERY_NTH: u32 = 30;
/// Encoder slack: we let Chrome produce at most this fraction of the rate the
/// client can draw. The 5% headroom absorbs jitter, so the delivered fps stays
/// within measurement noise of the unthrottled case.
const ENCODER_SLACK: f32 = 0.95;

/// The adaptive ladder: (JPEG quality, capture scale). Level 0 preserves detail;
/// level 1 cuts bytes while keeping full resolution (text stays legible); level 2
/// also downscales the capture for a badly lagging client. The page's layout is
/// never changed — only the captured frame size.
fn level_params(level: u32) -> (u32, f32) {
    match level {
        0 => (70, 1.0),
        1 => (48, 1.0),
        _ => (48, 0.6),
    }
}

/// `everyNthFrame` for a given ack latency: how much to throttle Chrome's encoder.
///
/// Chrome captures at up to ~60 fps; a client that draws `1000/ms` fps can only
/// consume that many. We ask for the largest divisor that keeps production within
/// `ENCODER_SLACK` of the client's draw rate, so the frames the client receives —
/// and therefore its fps and bandwidth — are unchanged, while the encoder stops
/// working on frames nobody will see. For example 0–20 ms -> 1, 33 ms -> 2,
/// 60 ms -> 3, 200 ms -> 12.
fn every_nth_for_ack_ms(ms: f32) -> u32 {
    if !ms.is_finite() || ms <= 0.0 {
        return 1;
    }
    let n = (60.0 * ms / 1000.0 / ENCODER_SLACK).floor() as i64;
    n.clamp(1, MAX_EVERY_NTH as i64) as u32
}

fn screencast_params(width: u32, height: u32, dsf: f32, level: u32, nth: u32) -> Value {
    let (quality, scale) = level_params(level);
    let max_w = ((width as f32) * dsf * scale).round().max(1.0) as u32;
    let max_h = ((height as f32) * dsf * scale).round().max(1.0) as u32;
    json!({
        "format": "jpeg",
        "quality": quality,
        "maxWidth": max_w,
        "maxHeight": max_h,
        "everyNthFrame": nth.max(1),
    })
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

struct Inner {
    config: BrowserConfig,
    events: broadcast::Sender<BrowserEvent>,
    surfaces: Mutex<HashMap<String, Arc<SurfaceState>>>,
    conn: RwLock<Option<Arc<CdpConnection>>>,
    child: Mutex<Option<Child>>,
    /// PID of the current Chromium main process. Because it is spawned with
    /// `process_group(0)`, its process group id equals this pid, so `shutdown`
    /// can signal the whole tree. Kept here (not only in `supervise`) so shutdown
    /// can always reach it.
    child_pid: Mutex<Option<u32>>,
    shutting_down: AtomicBool,
    restarts: AtomicU32,
}

impl Inner {
    async fn conn(&self) -> Option<Arc<CdpConnection>> {
        self.conn.read().await.clone()
    }
}

/// The browser engine. Cheap to clone; all clones share one Chromium.
#[derive(Clone)]
pub struct BrowserManager {
    inner: Arc<Inner>,
}

impl BrowserManager {
    /// Launch Chromium and connect to its DevTools endpoint.
    pub async fn launch(config: BrowserConfig) -> anyhow::Result<Self> {
        let (events, _rx) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            config,
            events,
            surfaces: Mutex::new(HashMap::new()),
            conn: RwLock::new(None),
            child: Mutex::new(None),
            child_pid: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            restarts: AtomicU32::new(0),
        });
        start_process(&inner).await?;
        let child = inner
            .child
            .lock()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("chromium child missing after launch"))?;
        let mgr = BrowserManager { inner: inner.clone() };
        tokio::spawn(supervise(inner, child));
        Ok(mgr)
    }

    /// Subscribe to frames / meta / closed events.
    pub fn events(&self) -> broadcast::Receiver<BrowserEvent> {
        self.inner.events.subscribe()
    }

    /// Number of times Chromium had to be restarted after an unexpected exit.
    pub fn restart_count(&self) -> u32 {
        self.inner.restarts.load(Ordering::SeqCst)
    }

    /// Create a surface backed by a fresh page target.
    pub async fn create_surface(&self, surface: &str, url: Option<&str>) -> anyhow::Result<()> {
        if self
            .inner
            .surfaces
            .lock()
            .await
            .contains_key(surface)
        {
            self.close_surface(surface).await?;
        }
        let conn = self
            .inner
            .conn()
            .await
            .ok_or_else(|| anyhow::anyhow!("chromium not running"))?;
        let s = Arc::new(SurfaceState::new(
            surface,
            self.inner.config.window_width,
            self.inner.config.window_height,
        ));
        let (tx, rx) = mpsc::unbounded_channel();
        *s.evt_tx.lock().await = Some(tx.clone());
        surface::spawn_event_loop(s.clone(), conn.clone(), self.inner.events.clone(), rx);

        let url = url.unwrap_or("about:blank");
        create_tab(&conn, &s, &tx, url)
            .await
            .map_err(|e| anyhow::anyhow!("create target: {e}"))?;
        s.loading.store(true, Ordering::SeqCst);
        *s.url.lock().await = url.to_string();
        self.inner
            .surfaces
            .lock()
            .await
            .insert(surface.to_string(), s.clone());
        s.emit_meta(&self.inner.events).await;
        Ok(())
    }

    /// Close a surface and destroy its page targets.
    pub async fn close_surface(&self, surface: &str) -> anyhow::Result<()> {
        let s = match self.inner.surfaces.lock().await.remove(surface) {
            Some(s) => s,
            None => return Ok(()),
        };
        s.closed.store(true, Ordering::SeqCst);
        let pairs: Vec<(String, String)> = s
            .tabs
            .lock()
            .await
            .iter()
            .map(|t| (t.target_id.clone(), t.session_id.clone()))
            .collect();
        s.tabs.lock().await.clear();
        if let Some(conn) = self.inner.conn().await {
            for (tid, sid) in pairs {
                conn.unregister(&sid).await;
                let _ = conn
                    .call(None, "Target.closeTarget", json!({ "targetId": tid }))
                    .await;
            }
        }
        let _ = self.inner.events.send(BrowserEvent::Closed { surface: s.id.clone() });
        Ok(())
    }

    /// Begin a JPEG screencast sized to the viewer. Call again to resize.
    pub async fn start_screencast(
        &self,
        surface: &str,
        width: u32,
        height: u32,
        device_scale_factor: f32,
    ) -> anyhow::Result<()> {
        let s = self.surface_anyhow(surface).await?;
        s.pacing.lock().await.reset();
        self.page_call_a(
            &s,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width,
                "height": height,
                "deviceScaleFactor": device_scale_factor,
                "mobile": false,
            }),
        )
        .await?;
        self.page_call_a(
            &s,
            "Page.startScreencast",
            screencast_params(width, height, device_scale_factor, 0, 1),
        )
        .await?;
        *s.screencast.lock().await = Some(Screencast {
            width,
            height,
            dsf: device_scale_factor,
        });
        s.emit_meta(&self.inner.events).await;
        Ok(())
    }

    /// Stop the screencast (no viewer = no encoding cost).
    pub async fn stop_screencast(&self, surface: &str) -> anyhow::Result<()> {
        let s = self.surface_anyhow(surface).await?;
        s.pacing.lock().await.reset();
        *s.screencast.lock().await = None;
        let _ = self.page_call_a(&s, "Page.stopScreencast", json!({})).await;
        Ok(())
    }

    /// Acknowledge a received frame: "the client drew this; it is ready for what is
    /// current".
    ///
    /// Chrome was already acked when the frame arrived (see `surface.rs`), so this
    /// does not gate Chrome. It releases the **newest** frame that replaced `seq`,
    /// dropping anything in between: a slow client sees fewer, always-current
    /// frames and never a backlog. The observed ack latency also feeds the adaptive
    /// quality/resolution ladder.
    pub async fn ack_frame(&self, surface: &str, seq: u32) -> anyhow::Result<()> {
        let s = self.surface_anyhow(surface).await?;
        let mut deliver: Option<LatestFrame> = None;
        {
            let mut p = s.pacing.lock().await;
            if let Some(inf) = p.in_flight.take() {
                let lat = inf.sent_at.elapsed().as_secs_f32() * 1000.0;
                p.ewma_ms = if p.ewma_ms <= 0.0 {
                    lat
                } else {
                    p.ewma_ms * 0.7 + lat * 0.3
                };
            }
            let latest_seq = p.latest.as_ref().map(|f| f.seq);
            if deliver_on_ack(latest_seq, seq).is_some() {
                deliver = p.latest.take();
                if let Some(f) = &deliver {
                    p.in_flight = Some(InFlight {
                        seq: f.seq,
                        sent_at: Instant::now(),
                    });
                }
            }
        }
        if let Some(f) = deliver {
            let _ = self.inner.events.send(BrowserEvent::Frame {
                surface: s.id.clone(),
                seq: f.seq,
                width: f.width,
                height: f.height,
                jpeg: f.jpeg,
            });
        }
        self.maybe_adapt(&s).await;
        Ok(())
    }

    /// Adapt quality, capture resolution and `everyNthFrame` from the client's
    /// observed ack latency (the same EWMA used for delivery pacing — no second
    /// estimator). Degrade when consistently behind, restore when it catches up,
    /// with hysteresis and a cooldown so we do not reconfigure on every frame.
    async fn maybe_adapt(&self, s: &Arc<SurfaceState>) {
        let (level, nth) = {
            let mut p = s.pacing.lock().await;
            if let Some(t) = p.last_reconfig {
                if t.elapsed() < Duration::from_millis(RECONFIG_COOLDOWN_MS) {
                    return;
                }
            }
            let new_level = if p.ewma_ms > EWMA_DEGRADE_MS {
                (p.level + 1).min(MAX_LEVEL)
            } else if p.ewma_ms > 0.0 && p.ewma_ms < EWMA_RESTORE_MS {
                p.level.saturating_sub(1)
            } else {
                p.level
            };
            // Only throttle the encoder as far as the client's draw rate allows.
            let new_nth = every_nth_for_ack_ms(p.ewma_ms);
            if new_level == p.level && new_nth == p.nth {
                return;
            }
            p.level = new_level;
            p.nth = new_nth;
            p.last_reconfig = Some(Instant::now());
            (new_level, new_nth)
        };
        let cfg = {
            let sc = s.screencast.lock().await;
            sc.as_ref().map(|c| (c.width, c.height, c.dsf))
        };
        if let Some((w, h, dsf)) = cfg {
            // Re-issuing startScreencast while it is running is a no-op on this
            // Chrome, so stop first to make the new params take effect.
            let _ = self.page_call_a(s, "Page.stopScreencast", json!({})).await;
            let _ = self
                .page_call_a(s, "Page.startScreencast", screencast_params(w, h, dsf, level, nth))
                .await;
        }
    }

    /// Cumulative count of frames Chrome has produced for this surface (encoder
    /// work, independent of what the client draws). Diagnostics only.
    pub async fn frames_produced(&self, surface: &str) -> anyhow::Result<u64> {
        let s = self.surface_anyhow(surface).await?;
        Ok(s.produced.load(Ordering::SeqCst))
    }

    /// Forward a raw input event (mouse / wheel / key / IME) to the page.
    pub async fn input(&self, surface: &str, event: BrowserInputEvent) -> anyhow::Result<()> {
        let s = self.surface_anyhow(surface).await?;
        match event {
            BrowserInputEvent::Mouse {
                action,
                x,
                y,
                button,
                click_count,
                modifiers,
            } => {
                let typ = match action {
                    MouseAction::Down => "mousePressed",
                    MouseAction::Up => "mouseReleased",
                    MouseAction::Move => "mouseMoved",
                };
                let btn = mouse_button_name(button);
                let buttons = if matches!(action, MouseAction::Down) {
                    mouse_button_mask(button)
                } else {
                    0
                };
                self.page_call_a(
                    &s,
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": typ, "x": x, "y": y, "button": btn,
                        "clickCount": click_count, "buttons": buttons,
                        "modifiers": cdp_mods(&modifiers),
                    }),
                )
                .await?;
            }
            BrowserInputEvent::Wheel {
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            } => {
                self.page_call_a(
                    &s,
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mouseWheel", "x": x, "y": y,
                        "deltaX": delta_x, "deltaY": delta_y,
                        "button": "none", "clickCount": 0,
                        "modifiers": cdp_mods(&modifiers),
                    }),
                )
                .await?;
            }
            BrowserInputEvent::Key {
                action,
                key,
                code,
                text,
                modifiers,
            } => {
                let typ = match action {
                    KeyAction::Up => "keyUp",
                    KeyAction::Down => {
                        if text.as_deref().map_or(false, |t| !t.is_empty()) {
                            "keyDown"
                        } else {
                            "rawKeyDown"
                        }
                    }
                };
                let vk = key_vk(&key);
                let mut p = json!({
                    "type": typ, "key": key, "code": code,
                    "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
                    "modifiers": cdp_mods(&modifiers),
                });
                if let Some(t) = text {
                    if !t.is_empty() {
                        p["text"] = json!(t);
                        p["unmodifiedText"] = json!(t);
                    }
                }
                self.page_call_a(&s, "Input.dispatchKeyEvent", p).await?;
            }
            BrowserInputEvent::InsertText { text } => {
                self.page_call_a(&s, "Input.insertText", json!({ "text": text }))
                    .await?;
            }
        }
        Ok(())
    }

    // -- internals ----------------------------------------------------------

    async fn surface_anyhow(&self, id: &str) -> anyhow::Result<Arc<SurfaceState>> {
        self.inner
            .surfaces
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown surface {id}"))
    }

    async fn surface(&self, id: &str) -> Result<Arc<SurfaceState>, RpcError> {
        self.inner
            .surfaces
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| err_not_found(format!("unknown surface {id}")))
    }

    async fn page_call(
        &self,
        s: &SurfaceState,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let conn = self
            .inner
            .conn()
            .await
            .ok_or_else(|| err_internal("chromium not running"))?;
        let sid = s
            .current_session()
            .await
            .ok_or_else(|| err_not_found("surface has no page"))?;
        conn.call(Some(&sid), method, params)
            .await
            .map_err(rpc_from_cdp)
    }

    async fn page_call_a(
        &self,
        s: &SurfaceState,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        let conn = self
            .inner
            .conn()
            .await
            .ok_or_else(|| anyhow::anyhow!("chromium not running"))?;
        let sid = s
            .current_session()
            .await
            .ok_or_else(|| anyhow::anyhow!("surface has no page"))?;
        conn.call(Some(&sid), method, params)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// `Runtime.evaluate` returning a JSON value.
    async fn eval(&self, s: &SurfaceState, expr: &str) -> Result<Value, RpcError> {
        let r = self
            .page_call(
                s,
                "Runtime.evaluate",
                json!({
                    "expression": expr,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
            )
            .await?;
        if let Some(exc) = r.get("exceptionDetails") {
            let msg = exc
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| exc.get("text").and_then(Value::as_str))
                .unwrap_or("evaluation failed");
            return Err(err_internal(msg.to_string()));
        }
        let result = r.get("result").cloned().unwrap_or(Value::Null);
        if let Some(v) = result.get("value") {
            return Ok(v.clone());
        }
        if result.get("subtype").and_then(Value::as_str) == Some("error") {
            return Err(err_internal(
                result
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("evaluation error"),
            ));
        }
        Ok(Value::Null)
    }

    async fn dispatch_mouse(
        &self,
        s: &SurfaceState,
        typ: &str,
        x: f64,
        y: f64,
        button: &str,
        click_count: u32,
        buttons: u32,
    ) -> Result<(), RpcError> {
        self.page_call(
            s,
            "Input.dispatchMouseEvent",
            json!({
                "type": typ, "x": x, "y": y, "button": button,
                "clickCount": click_count, "buttons": buttons, "modifiers": 0,
            }),
        )
        .await?;
        Ok(())
    }

    /// Real mouse events at the element's centre so clicks show in the screencast.
    async fn mouse_action(
        &self,
        s: &SurfaceState,
        selector: &str,
        click_count: u32,
    ) -> Result<(), RpcError> {
        let geo = self.eval(s, &js::geometry_expr(selector)).await?;
        if geo.get("found").and_then(Value::as_bool) != Some(true) {
            return Err(err_not_found(format!("no element for `{selector}`")));
        }
        let visible = geo.get("visible").and_then(Value::as_bool).unwrap_or(false);
        let x = geo.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let y = geo.get("y").and_then(Value::as_f64).unwrap_or(0.0);
        if !visible {
            if click_count >= 1 {
                let _ = self.eval(s, &js::js_click_expr(selector)).await;
            }
            return Ok(());
        }
        self.dispatch_mouse(s, "mouseMoved", x, y, "none", 0, 0).await?;
        if click_count == 0 {
            return Ok(());
        }
        for i in 1..=click_count {
            self.dispatch_mouse(s, "mousePressed", x, y, "left", i, 1).await?;
            self.dispatch_mouse(s, "mouseReleased", x, y, "left", i, 0).await?;
        }
        Ok(())
    }

    async fn dispatch_key(&self, s: &SurfaceState, spec: &KeySpec, mods: i64) -> Result<(), RpcError> {
        let mut down = json!({
            "type": "keyDown", "key": spec.key, "code": spec.code,
            "windowsVirtualKeyCode": spec.vk, "nativeVirtualKeyCode": spec.vk,
            "modifiers": mods,
        });
        if let Some(t) = &spec.text {
            down["text"] = json!(t);
            down["unmodifiedText"] = json!(t);
        }
        self.page_call(s, "Input.dispatchKeyEvent", down).await?;
        let up = json!({
            "type": "keyUp", "key": spec.key, "code": spec.code,
            "windowsVirtualKeyCode": spec.vk, "nativeVirtualKeyCode": spec.vk,
            "modifiers": mods,
        });
        self.page_call(s, "Input.dispatchKeyEvent", up).await?;
        Ok(())
    }

    async fn press_combo(&self, s: &SurfaceState, combo: &str) -> Result<(), RpcError> {
        let (mods, base) = parse_combo(combo);
        let spec = key_spec(base, mods).ok_or_else(|| err_invalid(format!("unknown key `{combo}`")))?;
        self.dispatch_key(s, &spec, mods).await
    }

    async fn wait_condition(&self, s: &SurfaceState, params: &Value) -> Result<bool, RpcError> {
        if let Some(sel) = pstr(params, "selector") {
            let expr = format!(
                "!!document.querySelector({})",
                js::json_string(&target_selector(&sel))
            );
            if self.eval(s, &expr).await?.as_bool() != Some(true) {
                return Ok(false);
            }
        }
        if let Some(text) = pstr(params, "text") {
            let expr = format!(
                "!!document.body && document.body.innerText.includes({})",
                js::json_string(&text)
            );
            if self.eval(s, &expr).await?.as_bool() != Some(true) {
                return Ok(false);
            }
        }
        if let Some(frag) = pstr(params, "url_contains") {
            let url = self
                .eval(s, "location.href")
                .await?
                .as_str()
                .unwrap_or("")
                .to_string();
            if !url.contains(&frag) {
                return Ok(false);
            }
        }
        if let Some(state) = pstr(params, "load_state") {
            let ready = self
                .eval(s, "document.readyState")
                .await?
                .as_str()
                .unwrap_or("")
                .to_string();
            let ok = match state.as_str() {
                "load" | "networkidle" => ready == "complete",
                "domcontentloaded" => ready == "interactive" || ready == "complete",
                _ => true,
            };
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // -- RPC ----------------------------------------------------------------

    /// Implement every `browser.*` method in `lmux_proto::methods` except
    /// `browser.open` (the daemon owns surface creation).
    pub async fn handle_rpc(
        &self,
        surface: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let s = self.surface(surface).await?;
        match method {
            m::BROWSER_GOTO => {
                let url = req(&params, "url")?;
                self.page_call(&s, "Page.navigate", json!({ "url": url })).await?;
                s.loading.store(true, Ordering::SeqCst);
                *s.url.lock().await = url.clone();
                s.emit_meta(&self.inner.events).await;
                Ok(json!({ "url": url }))
            }
            m::BROWSER_BACK | m::BROWSER_FORWARD => {
                let back = method == m::BROWSER_BACK;
                let h = self.page_call(&s, "Page.getNavigationHistory", json!({})).await?;
                let idx = h.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
                let entries = h
                    .get("entries")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let target_idx = if back { idx - 1 } else { idx + 1 };
                if target_idx < 0 || target_idx as usize >= entries.len() {
                    return Err(err_not_found("no such history entry"));
                }
                let entry_id = entries[target_idx as usize]
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| err_internal("history entry missing id"))?;
                self.page_call(
                    &s,
                    "Page.navigateToHistoryEntry",
                    json!({ "entryId": entry_id }),
                )
                .await?;
                Ok(json!({}))
            }
            m::BROWSER_RELOAD => {
                self.page_call(&s, "Page.reload", json!({ "ignoreCache": false })).await?;
                Ok(json!({}))
            }
            m::BROWSER_URL => {
                let mut url = s.url.lock().await.clone();
                let mut title = s.title.lock().await.clone();
                if let Ok(v) = self
                    .eval(&s, "({url: location.href, title: document.title})")
                    .await
                {
                    if let Some(u) = v.get("url").and_then(Value::as_str) {
                        url = u.to_string();
                    }
                    if let Some(t) = v.get("title").and_then(Value::as_str) {
                        title = t.to_string();
                    }
                }
                Ok(json!({ "url": url, "title": title }))
            }
            m::BROWSER_SNAPSHOT => {
                let interactive = params
                    .get("interactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let v = self.eval(&s, &js::snapshot_expr(interactive)).await?;
                let text = v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                let refs = v.get("refs").cloned().unwrap_or_else(|| json!([]));
                Ok(json!({ "text": text, "refs": refs }))
            }
            m::BROWSER_CLICK | m::BROWSER_DBLCLICK | m::BROWSER_HOVER => {
                let target = req(&params, "target")?;
                let sel = target_selector(&target);
                let count = match method {
                    m::BROWSER_CLICK => 1,
                    m::BROWSER_DBLCLICK => 2,
                    _ => 0,
                };
                self.mouse_action(&s, &sel, count).await?;
                Ok(json!({}))
            }
            m::BROWSER_TYPE => {
                let target = req(&params, "target")?;
                let text = req(&params, "text")?;
                let sel = target_selector(&target);
                let focused = self.eval(&s, &js::focus_expr(&sel)).await?;
                if focused.get("found").and_then(Value::as_bool) != Some(true) {
                    return Err(err_not_found(format!("no element for `{sel}`")));
                }
                for ch in text.chars() {
                    let spec = char_key(ch);
                    self.dispatch_key(&s, &spec, 0).await?;
                }
                Ok(json!({ "typed": text.chars().count() }))
            }
            m::BROWSER_FILL => {
                let target = req(&params, "target")?;
                let text = pstr(&params, "text").unwrap_or_default();
                let sel = target_selector(&target);
                let cleared = self.eval(&s, &js::clear_expr(&sel)).await?;
                if cleared.get("found").and_then(Value::as_bool) != Some(true) {
                    return Err(err_not_found(format!("no element for `{sel}`")));
                }
                self.page_call(&s, "Input.insertText", json!({ "text": text })).await?;
                Ok(json!({ "filled": text.chars().count() }))
            }
            m::BROWSER_PRESS => {
                let key = req(&params, "key")?;
                if let Some(target) = pstr(&params, "target") {
                    let sel = target_selector(&target);
                    let _ = self.eval(&s, &js::focus_expr(&sel)).await;
                }
                self.press_combo(&s, &key).await?;
                Ok(json!({}))
            }
            m::BROWSER_SELECT => {
                let target = req(&params, "target")?;
                let value = req(&params, "value")?;
                let sel = target_selector(&target);
                let r = self.eval(&s, &js::select_expr(&sel, &value)).await?;
                if r.get("found").and_then(Value::as_bool) != Some(true) {
                    return Err(err_not_found(format!("no element for `{sel}`")));
                }
                if r.get("ok").and_then(Value::as_bool) != Some(true) {
                    return Err(err_not_found(format!("no option matching `{value}`")));
                }
                Ok(json!({ "value": r.get("value").cloned().unwrap_or(Value::Null) }))
            }
            m::BROWSER_SCROLL => {
                let dx = params.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
                let dy = params.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
                let sel = pstr(&params, "target").map(|t| target_selector(&t));
                let r = self.eval(&s, &js::scroll_expr(sel.as_deref(), dx, dy)).await?;
                if r.get("found").and_then(Value::as_bool) == Some(false) {
                    return Err(err_not_found("scroll target not found"));
                }
                Ok(r)
            }
            m::BROWSER_WAIT => {
                let timeout = params
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(5000);
                let start = Instant::now();
                loop {
                    if self.wait_condition(&s, &params).await? {
                        return Ok(json!({
                            "waited_ms": start.elapsed().as_millis() as u64
                        }));
                    }
                    if start.elapsed() >= Duration::from_millis(timeout) {
                        return Err(err_not_found("wait timed out"));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            m::BROWSER_EVAL => {
                let script = req(&params, "script")?;
                let v = self.eval(&s, &script).await?;
                Ok(json!({ "value": v }))
            }
            m::BROWSER_SCREENSHOT => {
                let r = self
                    .page_call(&s, "Page.captureScreenshot", json!({ "format": "png" }))
                    .await?;
                let b64 = r
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| err_internal("screenshot returned no data"))?;
                if let Some(path) = pstr(&params, "path") {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .map_err(|e| err_internal(format!("bad screenshot base64: {e}")))?;
                    tokio::fs::write(&path, bytes)
                        .await
                        .map_err(|e| err_internal(format!("write {path}: {e}")))?;
                    Ok(json!({ "path": path }))
                } else {
                    Ok(json!({ "png_base64": b64 }))
                }
            }
            m::BROWSER_GET => {
                let what = req(&params, "what")?;
                let target = pstr(&params, "target");
                let name = pstr(&params, "name");
                let sel = target.as_deref().map(target_selector);
                let expr = js::get_expr(&what, sel.as_deref(), name.as_deref())
                    .ok_or_else(|| err_invalid(format!("unknown what `{what}`")))?;
                let v = self.eval(&s, &expr).await?;
                Ok(json!({ "value": v }))
            }
            m::BROWSER_CONSOLE => {
                let clear = params.get("clear").and_then(Value::as_bool).unwrap_or(false);
                let entries: Vec<Value> = s.console.lock().await.iter().cloned().collect();
                if clear {
                    s.console.lock().await.clear();
                }
                Ok(json!({ "entries": entries }))
            }
            m::BROWSER_ERRORS => {
                let clear = params.get("clear").and_then(Value::as_bool).unwrap_or(false);
                let entries: Vec<Value> = s.errors.lock().await.iter().cloned().collect();
                if clear {
                    s.errors.lock().await.clear();
                }
                Ok(json!({ "entries": entries }))
            }
            m::BROWSER_TABS => self.handle_tabs(&s, &params).await,
            m::BROWSER_OPEN => Err(err_unsupported("browser.open is routed by the daemon")),
            _ => Err(err_unknown(method)),
        }
    }

    async fn handle_tabs(
        &self,
        s: &Arc<SurfaceState>,
        params: &Value,
    ) -> Result<Value, RpcError> {
        let action = req(params, "action")?;
        let conn = self
            .inner
            .conn()
            .await
            .ok_or_else(|| err_internal("chromium not running"))?;
        match action.as_str() {
            "list" => {
                let tabs = s.tabs.lock().await;
                let active = s.active.load(Ordering::SeqCst);
                let arr: Vec<Value> = tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        json!({
                            "index": i,
                            "url": t.url,
                            "title": t.title,
                            "active": i == active,
                        })
                    })
                    .collect();
                Ok(json!({ "tabs": arr, "active": active }))
            }
            "new" => {
                let url = pstr(params, "url").unwrap_or_else(|| "about:blank".to_string());
                let tx = s
                    .event_sender()
                    .await
                    .ok_or_else(|| err_internal("surface not attached"))?;
                create_tab(&conn, s, &tx, &url)
                    .await
                    .map_err(|e| err_internal(e.to_string()))?;
                let idx = s.tabs.lock().await.len() - 1;
                s.active.store(idx, Ordering::SeqCst);
                self.sync_active_meta(s).await;
                Ok(json!({ "index": idx }))
            }
            "switch" => {
                let idx = params
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| err_invalid("tabs switch needs `index`"))? as usize;
                if idx >= s.tabs.lock().await.len() {
                    return Err(err_invalid("tab index out of range"));
                }
                s.active.store(idx, Ordering::SeqCst);
                self.sync_active_meta(s).await;
                Ok(json!({ "index": idx }))
            }
            "close" => {
                let idx = match params.get("index").and_then(Value::as_u64) {
                    Some(i) => i as usize,
                    None => s.current_tab_index().await,
                };
                let removed = {
                    let mut tabs = s.tabs.lock().await;
                    if idx >= tabs.len() {
                        return Err(err_invalid("tab index out of range"));
                    }
                    tabs.remove(idx)
                };
                conn.unregister(&removed.session_id).await;
                let _ = conn
                    .call(None, "Target.closeTarget", json!({ "targetId": removed.target_id }))
                    .await;
                let len = s.tabs.lock().await.len();
                let cur = s.active.load(Ordering::SeqCst);
                if len == 0 {
                    let tx = s
                        .event_sender()
                        .await
                        .ok_or_else(|| err_internal("surface not attached"))?;
                    create_tab(&conn, s, &tx, "about:blank")
                        .await
                        .map_err(|e| err_internal(e.to_string()))?;
                    s.active.store(0, Ordering::SeqCst);
                } else if idx <= cur && cur > 0 {
                    s.active.store(cur - 1, Ordering::SeqCst);
                } else if cur >= len {
                    s.active.store(len - 1, Ordering::SeqCst);
                }
                self.sync_active_meta(s).await;
                Ok(json!({}))
            }
            other => Err(err_invalid(format!("unknown tabs action `{other}`"))),
        }
    }

    async fn sync_active_meta(&self, s: &Arc<SurfaceState>) {
        let idx = s.active.load(Ordering::SeqCst);
        let (url, title) = {
            let tabs = s.tabs.lock().await;
            match tabs.get(idx) {
                Some(t) => (t.url.clone(), t.title.clone()),
                None => ("about:blank".to_string(), String::new()),
            }
        };
        *s.url.lock().await = url;
        *s.title.lock().await = title;
        s.loading.store(false, Ordering::SeqCst);
        s.emit_meta(&self.inner.events).await;
    }

    /// Stop Chromium and all background tasks. Idempotent; safe to call more than
    /// once, and a fresh `launch()` on the same profile works afterwards.
    pub async fn shutdown(&self) {
        // Mark first so the supervisor treats the exit as expected, not a crash.
        self.inner.shutting_down.store(true, Ordering::SeqCst);

        // Kill the whole process group (browser + renderers + gpu). The pid is the
        // group id because Chromium was spawned with process_group(0).
        if let Some(pid) = *self.inner.child_pid.lock().await {
            terminate_group(pid).await;
        }
        // Fallback for the (unexpected) case where no pid was recorded.
        if let Some(mut c) = self.inner.child.lock().await.take() {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }
        *self.inner.child_pid.lock().await = None;
        *self.inner.conn.write().await = None;

        let surfaces: Vec<Arc<SurfaceState>> =
            self.inner.surfaces.lock().await.drain().map(|(_, v)| v).collect();
        for s in surfaces {
            s.closed.store(true, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Process-group termination
// ---------------------------------------------------------------------------

/// SIGTERM the group, then SIGKILL if it does not exit promptly.
#[cfg(unix)]
async fn terminate_group(pid: u32) {
    signal_group(pid, libc::SIGTERM);
    if wait_pid_gone(pid, Duration::from_millis(1500)).await {
        return;
    }
    signal_group(pid, libc::SIGKILL);
    let _ = wait_pid_gone(pid, Duration::from_millis(2000)).await;
}

#[cfg(not(unix))]
async fn terminate_group(_pid: u32) {}

#[cfg(unix)]
fn signal_group(pid: u32, sig: i32) {
    // Negative pid signals the whole process group.
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    false
}

async fn wait_pid_gone(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    !pid_alive(pid)
}

// ---------------------------------------------------------------------------
// Tab helpers
// ---------------------------------------------------------------------------

async fn create_tab(
    conn: &Arc<CdpConnection>,
    s: &SurfaceState,
    tx: &mpsc::UnboundedSender<CdpEvent>,
    url: &str,
) -> anyhow::Result<()> {
    let created = conn
        .call(
            None,
            "Target.createTarget",
            json!({ "url": url }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let target_id = created
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("createTarget returned no targetId"))?
        .to_string();
    let att = conn
        .call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let session_id = att
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("attachToTarget returned no sessionId"))?
        .to_string();

    conn.register(&session_id, tx.clone()).await;
    for domain in ["Page.enable", "Runtime.enable", "Log.enable"] {
        let _ = conn.call(Some(&session_id), domain, json!({})).await;
    }
    let _ = conn
        .call(
            Some(&session_id),
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": s.default_width,
                "height": s.default_height,
                "deviceScaleFactor": 1.0,
                "mobile": false,
            }),
        )
        .await;
    // Chrome 154 creates the target at about:blank and does not always honour the
    // `url` passed to Target.createTarget; navigate explicitly.
    if url != "about:blank" {
        conn.call(Some(&session_id), "Page.navigate", json!({ "url": url }))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    s.tabs.lock().await.push(Tab {
        target_id,
        session_id,
        url: url.to_string(),
        title: String::new(),
        main_frame: None,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Process supervision
// ---------------------------------------------------------------------------

async fn start_process(inner: &Arc<Inner>) -> anyhow::Result<()> {
    chromium::clear_stale_lock(&inner.config.user_data_dir);
    let (child, ws_url) = chromium::launch(&inner.config).await?;
    let conn = match CdpConnection::connect(&ws_url).await {
        Ok(c) => Arc::new(c),
        Err(e) => return Err(anyhow::anyhow!("connect to DevTools: {e}")),
    };
    let (btx, brx) = mpsc::unbounded_channel();
    conn.set_browser_sink(btx).await;
    if let Err(e) = conn
        .call(None, "Target.setDiscoverTargets", json!({ "discover": true }))
        .await
    {
        eprintln!("lmux-browser: Target.setDiscoverTargets failed: {e}");
    }
    *inner.conn.write().await = Some(conn);
    *inner.child_pid.lock().await = child.id();
    *inner.child.lock().await = Some(child);
    tokio::spawn(browser_events_loop(inner.clone(), brx));
    Ok(())
}

/// An unexpected exit restarts Chromium; a shutdown-triggered exit does not.
fn should_restart_after_exit(shutting_down: bool) -> bool {
    !shutting_down
}

async fn supervise(inner: Arc<Inner>, mut child: Child) {
    loop {
        let status = child.wait().await;
        if !should_restart_after_exit(inner.shutting_down.load(Ordering::SeqCst)) {
            *inner.child_pid.lock().await = None;
            break;
        }
        eprintln!("lmux-browser: chromium exited ({status:?}); restarting");
        let mut ok = false;
        for attempt in 0..5u32 {
            tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
            if !should_restart_after_exit(inner.shutting_down.load(Ordering::SeqCst)) {
                return;
            }
            match start_process(&inner).await {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => eprintln!(
                    "lmux-browser: chromium relaunch attempt {} failed: {e}",
                    attempt + 1
                ),
            }
        }
        if !ok {
            emit_all_closed(&inner).await;
            break;
        }
        inner.restarts.fetch_add(1, Ordering::SeqCst);
        rebuild_surfaces(&inner).await;
        child = match inner.child.lock().await.take() {
            Some(c) => c,
            None => break,
        };
    }
}

async fn emit_all_closed(inner: &Arc<Inner>) {
    let surfaces: Vec<Arc<SurfaceState>> =
        inner.surfaces.lock().await.drain().map(|(_, v)| v).collect();
    for s in surfaces {
        s.closed.store(true, Ordering::SeqCst);
        let _ = inner.events.send(BrowserEvent::Closed { surface: s.id.clone() });
    }
}

async fn rebuild_surfaces(inner: &Arc<Inner>) {
    let Some(conn) = inner.conn().await else { return };
    let surfaces: Vec<Arc<SurfaceState>> =
        inner.surfaces.lock().await.values().cloned().collect();
    for s in surfaces {
        if s.closed.load(Ordering::SeqCst) {
            continue;
        }
        let url = s.url.lock().await.clone();
        s.tabs.lock().await.clear();
        s.pacing.lock().await.reset();
        *s.screencast.lock().await = None;
        let (tx, rx) = mpsc::unbounded_channel();
        *s.evt_tx.lock().await = Some(tx.clone());
        surface::spawn_event_loop(s.clone(), conn.clone(), inner.events.clone(), rx);
        let url = if url.is_empty() { "about:blank".to_string() } else { url };
        match create_tab(&conn, &s, &tx, &url).await {
            Ok(()) => {
                s.loading.store(true, Ordering::SeqCst);
                s.emit_meta(&inner.events).await;
            }
            Err(e) => {
                eprintln!("lmux-browser: could not rebuild surface {}: {e}", s.id);
                s.closed.store(true, Ordering::SeqCst);
                let _ = inner.events.send(BrowserEvent::Closed { surface: s.id.clone() });
            }
        }
    }
}

async fn browser_events_loop(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<CdpEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev.method.as_str() {
            "Target.targetDestroyed" | "Target.targetCrashed" => {
                if let Some(tid) = ev.params.get("targetId").and_then(Value::as_str) {
                    on_target_gone(&inner, tid).await;
                }
            }
            "Target.targetInfoChanged" => {
                let info = &ev.params["targetInfo"];
                if info.get("type").and_then(Value::as_str) == Some("page") {
                    if let Some(tid) = info.get("targetId").and_then(Value::as_str) {
                        let url = info.get("url").and_then(Value::as_str).unwrap_or("");
                        let title = info.get("title").and_then(Value::as_str).unwrap_or("");
                        on_target_info(&inner, tid, url, title).await;
                    }
                }
            }
            _ => {}
        }
    }
}

async fn on_target_gone(inner: &Arc<Inner>, target_id: &str) {
    let surface = {
        let surfaces = inner.surfaces.lock().await;
        let mut found = None;
        for s in surfaces.values() {
            if s.tabs
                .lock()
                .await
                .iter()
                .any(|t| t.target_id == target_id)
            {
                found = Some(s.id.clone());
                break;
            }
        }
        found
    };
    let Some(id) = surface else { return };
    if let Some(s) = inner.surfaces.lock().await.remove(&id) {
        s.closed.store(true, Ordering::SeqCst);
        let _ = inner.events.send(BrowserEvent::Closed { surface: s.id.clone() });
    }
}

async fn on_target_info(inner: &Arc<Inner>, target_id: &str, url: &str, title: &str) {
    let found = {
        let surfaces = inner.surfaces.lock().await;
        let mut f = None;
        for s in surfaces.values() {
            if s.tabs
                .lock()
                .await
                .iter()
                .any(|t| t.target_id == target_id)
            {
                f = Some(s.clone());
                break;
            }
        }
        f
    };
    let Some(s) = found else { return };
    let active = s.active.load(Ordering::SeqCst);
    let mut is_active = false;
    {
        let mut tabs = s.tabs.lock().await;
        if let Some(pos) = tabs.iter().position(|t| t.target_id == target_id) {
            if !url.is_empty() && url != "about:blank" {
                tabs[pos].url = url.to_string();
            }
            if !title.is_empty() {
                tabs[pos].title = title.to_string();
            }
            is_active = pos == active;
        }
    }
    if is_active {
        if !url.is_empty() && url != "about:blank" {
            *s.url.lock().await = url.to_string();
        }
        if !title.is_empty() {
            *s.title.lock().await = title.to_string();
        }
        s.emit_meta(&inner.events).await;
    }
}

// ---------------------------------------------------------------------------
// Input mapping helpers
// ---------------------------------------------------------------------------

fn cdp_mods(m: &Modifiers) -> i64 {
    (m.alt as i64) | ((m.ctrl as i64) << 1) | ((m.meta as i64) << 2) | ((m.shift as i64) << 3)
}

fn mouse_button_name(b: MouseButton) -> &'static str {
    match b {
        MouseButton::None => "none",
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
    }
}

fn mouse_button_mask(b: MouseButton) -> u32 {
    match b {
        MouseButton::None => 0,
        MouseButton::Left => 1,
        MouseButton::Middle => 4,
        MouseButton::Right => 2,
    }
}

/// A resolved key press: DOM `key`, DOM `code`, legacy virtual key code, and text.
struct KeySpec {
    key: String,
    code: String,
    vk: i64,
    text: Option<String>,
}

fn named_key(name: &str) -> Option<(&'static str, &'static str, i64)> {
    Some(match name {
        "Enter" | "Return" => ("Enter", "Enter", 13),
        "Tab" => ("Tab", "Tab", 9),
        "Escape" | "Esc" => ("Escape", "Escape", 27),
        "Backspace" => ("Backspace", "Backspace", 8),
        "Delete" => ("Delete", "Delete", 46),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39),
        "Home" => ("Home", "Home", 36),
        "End" => ("End", "End", 35),
        "PageUp" => ("PageUp", "PageUp", 33),
        "PageDown" => ("PageDown", "PageDown", 34),
        "Space" => (" ", "Space", 32),
        _ => return None,
    })
}

fn char_key(ch: char) -> KeySpec {
    let s = ch.to_string();
    if ch == ' ' {
        return KeySpec {
            key: " ".into(),
            code: "Space".into(),
            vk: 32,
            text: Some(" ".into()),
        };
    }
    if ch.is_ascii_alphabetic() {
        let up = ch.to_ascii_uppercase();
        return KeySpec {
            key: s.clone(),
            code: format!("Key{up}"),
            vk: up as i64,
            text: Some(s),
        };
    }
    if ch.is_ascii_digit() {
        return KeySpec {
            key: s.clone(),
            code: format!("Digit{ch}"),
            vk: ch as i64,
            text: Some(s),
        };
    }
    KeySpec {
        key: s.clone(),
        code: String::new(),
        vk: 0,
        text: Some(s),
    }
}

/// Build the key spec for the non-modifier part of a combo, honouring Shift for
/// the produced text.
fn key_spec(base: &str, mods: i64) -> Option<KeySpec> {
    let shift = mods & 8 != 0;
    let ctrl_like = mods & (2 | 4) != 0;
    let alt = mods & 1 != 0;
    if let Some((key, code, vk)) = named_key(base) {
        let text = match base {
            "Enter" | "Return" => Some("\r".to_string()),
            "Space" => Some(" ".to_string()),
            "Tab" => Some("\t".to_string()),
            _ => None,
        };
        // Control/alt combos produce no text for named keys except Enter/Space/Tab.
        let text = if ctrl_like || alt { None } else { text };
        return Some(KeySpec { key: key.into(), code: code.into(), vk, text });
    }
    let mut chars = base.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let mut spec = char_key(ch);
    if shift && ch.is_ascii_alphabetic() {
        // Shift+letter produces the uppercase character.
        let up = ch.to_ascii_uppercase();
        spec.key = up.to_string();
        spec.code = format!("Key{up}");
        spec.text = Some(up.to_string());
    }
    if ctrl_like || alt {
        spec.text = None;
    }
    Some(spec)
}

/// Parse `Control+Shift+k` into (modifier bitmask, base key).
fn parse_combo(combo: &str) -> (i64, &str) {
    let mut mods = 0i64;
    let parts: Vec<&str> = combo.split('+').collect();
    let mut idx = 0;
    while idx + 1 < parts.len() {
        let p = parts[idx];
        if p.eq_ignore_ascii_case("ctrl") || p.eq_ignore_ascii_case("control") {
            mods |= 2;
        } else if p.eq_ignore_ascii_case("alt") || p.eq_ignore_ascii_case("option") {
            mods |= 1;
        } else if p.eq_ignore_ascii_case("meta")
            || p.eq_ignore_ascii_case("cmd")
            || p.eq_ignore_ascii_case("command")
            || p.eq_ignore_ascii_case("super")
        {
            mods |= 4;
        } else if p.eq_ignore_ascii_case("shift") {
            mods |= 8;
        } else {
            // Not a modifier: the whole remainder is the base key.
            return (mods, p);
        }
        idx += 1;
    }
    (mods, parts.last().copied().unwrap_or(""))
}

fn key_vk(key: &str) -> i64 {
    if let Some((_, _, vk)) = named_key(key) {
        return vk;
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => c.to_ascii_uppercase() as i64,
        (Some(c), None) if c.is_ascii_digit() => c as i64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_detection() {
        assert!(is_ref("e1"));
        assert!(is_ref("e42"));
        assert!(!is_ref("e"));
        assert!(!is_ref("e1x"));
        assert!(!is_ref(".button"));
        assert_eq!(target_selector("e3"), "[data-lmux-ref=\"e3\"]");
        assert_eq!(target_selector("#go"), "#go");
    }

    #[test]
    fn combo_parsing() {
        assert_eq!(parse_combo("Enter"), (0, "Enter"));
        assert_eq!(parse_combo("Control+a"), (2, "a"));
        assert_eq!(parse_combo("Shift+Tab"), (8, "Tab"));
        assert_eq!(parse_combo("Control+Shift+k"), (2 | 8, "k"));
    }

    #[test]
    fn key_specs() {
        let enter = key_spec("Enter", 0).unwrap();
        assert_eq!(enter.key, "Enter");
        assert_eq!(enter.vk, 13);
        assert_eq!(enter.text.as_deref(), Some("\r"));

        let a = key_spec("a", 8).unwrap();
        assert_eq!(a.code, "KeyA");
        // Shift+letter keeps text (uppercase comes from the browser's shifted lookup).
        assert!(a.text.is_some());

        let ctrl_a = key_spec("a", 2).unwrap();
        assert_eq!(ctrl_a.text, None);
    }

    #[test]
    fn mods_bitmask() {
        let m = Modifiers { alt: true, ctrl: true, meta: false, shift: true };
        assert_eq!(cdp_mods(&m), 1 | 2 | 8);
    }

    #[test]
    fn shutdown_is_not_a_crash() {
        // The supervisor restarts only unexpected exits.
        assert!(should_restart_after_exit(false));
        assert!(!should_restart_after_exit(true));
    }

    #[test]
    fn every_nth_follows_ack_latency() {
        // Prompt clients get every frame.
        assert_eq!(every_nth_for_ack_ms(0.0), 1);
        assert_eq!(every_nth_for_ack_ms(10.0), 1);
        assert_eq!(every_nth_for_ack_ms(20.0), 1);
        // Slower clients throttle the encoder...
        assert_eq!(every_nth_for_ack_ms(33.0), 2);
        assert_eq!(every_nth_for_ack_ms(60.0), 3);
        assert_eq!(every_nth_for_ack_ms(200.0), 12);
        // ...capped.
        assert_eq!(every_nth_for_ack_ms(1.0e9), MAX_EVERY_NTH);
        // ...and never starving the client by more than the slack: a 60 fps encoder
        // divided by `nth` must stay within `ENCODER_SLACK` of the client's draw rate.
        for ms in [1.0f32, 5.0, 10.0, 16.0, 20.0, 33.0, 60.0, 120.0, 200.0, 500.0] {
            let nth = every_nth_for_ack_ms(ms) as f32;
            let production = 60.0 / nth;
            let ideal = (1000.0 / ms).min(60.0);
            assert!(
                production >= ENCODER_SLACK * ideal - 0.01,
                "ms={ms} nth={nth} production={production} ideal={ideal}"
            );
        }
    }

    /// `terminate_group` must kill a whole process group (not just the leader) and
    /// must be safe to call again. Uses a plain `sleep` so no Chromium is needed.
    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_group_kills_and_is_idempotent() {
        use std::process::Stdio;
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("pid");
        assert!(pid_alive(pid));

        // Reap concurrently, otherwise the child stays a zombie and `kill(pid,0)`
        // keeps reporting it as alive.
        let reaper = tokio::spawn(async move { child.wait().await });

        terminate_group(pid).await;
        assert!(!pid_alive(pid), "process group should be gone");

        // Idempotent: a second call must not panic or hang.
        terminate_group(pid).await;

        let _ = reaper.await;
        assert!(!pid_alive(pid));
    }
}
