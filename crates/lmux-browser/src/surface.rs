//! Per-surface state and the task that translates CDP events into lmux events.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use base64::Engine;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::cdp::{CdpConnection, CdpEvent};
use crate::BrowserEvent;

const MAX_CONSOLE: usize = 1000;

/// One page target inside a surface. A surface starts with one tab; `browser.tabs`
/// new/switch/close manage extras.
pub(crate) struct Tab {
    pub target_id: String,
    pub session_id: String,
    pub url: String,
    pub title: String,
    pub main_frame: Option<String>,
}

/// Active screencast parameters (the viewer's logical size and scale factor).
pub(crate) struct Screencast {
    pub width: u32,
    pub height: u32,
    pub dsf: f32,
}

/// The newest frame Chrome produced, waiting to be delivered to the client.
pub(crate) struct LatestFrame {
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub jpeg: Vec<u8>,
}

/// A frame already delivered to the client, awaiting its ack. Only ever one.
pub(crate) struct InFlight {
    pub seq: u32,
    pub sent_at: Instant,
}

/// Screencast pacing state. See [`deliver_on_produce`] / [`deliver_on_ack`].
///
/// Chrome is acked locally the moment a frame arrives, so it keeps producing at
/// its own rate and the page's animation clock keeps running. Only the newest
/// frame is kept; the client is sent a frame only when none is outstanding, so a
/// slow client gets fewer, always-current frames instead of a backlog.
pub(crate) struct Pacing {
    pub latest: Option<LatestFrame>,
    pub in_flight: Option<InFlight>,
    /// EWMA of the client's ack latency in milliseconds.
    pub ewma_ms: f32,
    /// Adaptive quality/resolution level (0 = best).
    pub level: u32,
    /// `everyNthFrame` currently applied to the live screencast.
    pub nth: u32,
    pub last_reconfig: Option<Instant>,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            latest: None,
            in_flight: None,
            ewma_ms: 0.0,
            level: 0,
            nth: 1,
            last_reconfig: None,
        }
    }
}

impl Pacing {
    pub fn reset(&mut self) {
        self.latest = None;
        self.in_flight = None;
        self.ewma_ms = 0.0;
        self.level = 0;
        self.nth = 1;
        self.last_reconfig = None;
    }
}

/// Should a freshly produced frame be delivered now? Only when nothing is in
/// flight to the client; otherwise it just replaces the stored newest frame.
pub(crate) fn deliver_on_produce(in_flight: Option<u32>) -> bool {
    in_flight.is_none()
}

/// On the client acking `acked`, which frame should be delivered now? The newest
/// stored frame, but only if it actually replaced `acked`. Intermediate frames
/// are dropped; nothing is queued.
pub(crate) fn deliver_on_ack(latest: Option<u32>, acked: u32) -> Option<u32> {
    match latest {
        Some(l) if l > acked => Some(l),
        _ => None,
    }
}

/// Read `(width, height)` from a JPEG's SOF marker without a full decode.
///
/// The screencast frame metadata reports the *device* size, which can differ from
/// the captured JPEG when `maxWidth`/`maxHeight` downscale it; the app needs the
/// real pixel size to scale the frame onto its canvas.
pub(crate) fn jpeg_dimensions(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // Markers without a length field.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = ((data[i + 2] as usize) << 8) | data[i + 3] as usize;
        if len < 2 {
            return None;
        }
        // SOF0..SOF15 except DHT(C4)/JPG(C8)/DAC(CC).
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            let h = ((data[i + 5] as u16) << 8) | data[i + 6] as u16;
            let w = ((data[i + 7] as u16) << 8) | data[i + 8] as u16;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

pub(crate) struct SurfaceState {
    pub id: String,
    pub tabs: Mutex<Vec<Tab>>,
    pub active: AtomicUsize,
    pub closed: AtomicBool,
    pub url: Mutex<String>,
    pub title: Mutex<String>,
    pub loading: AtomicBool,
    pub can_go_back: AtomicBool,
    pub can_go_forward: AtomicBool,
    pub console: Mutex<VecDeque<Value>>,
    pub errors: Mutex<VecDeque<Value>>,
    pub screencast: Mutex<Option<Screencast>>,
    pub seq: AtomicU32,
    /// Cumulative count of `Page.screencastFrame` events Chrome produced. Used as a
    /// CPU proxy: encoder work is production, not what the client draws.
    pub produced: AtomicU64,
    pub pacing: Mutex<Pacing>,
    pub evt_tx: Mutex<Option<mpsc::UnboundedSender<CdpEvent>>>,
    pub default_width: u32,
    pub default_height: u32,
    /// Page zoom as `f64` bits (1.0 = 100%). Applied as a device-scale change, the
    /// way Chrome zooms, so pages reflow; see `BrowserManager::apply_metrics`.
    pub zoom: AtomicU64,
}

impl SurfaceState {
    pub fn zoom(&self) -> f64 {
        f64::from_bits(self.zoom.load(Ordering::SeqCst))
    }

    pub fn set_zoom(&self, z: f64) {
        self.zoom.store(z.to_bits(), Ordering::SeqCst);
    }

    pub fn new(id: &str, width: u32, height: u32) -> Self {
        Self {
            id: id.to_string(),
            tabs: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            url: Mutex::new("about:blank".to_string()),
            title: Mutex::new(String::new()),
            loading: AtomicBool::new(false),
            can_go_back: AtomicBool::new(false),
            can_go_forward: AtomicBool::new(false),
            console: Mutex::new(VecDeque::new()),
            errors: Mutex::new(VecDeque::new()),
            screencast: Mutex::new(None),
            seq: AtomicU32::new(0),
            produced: AtomicU64::new(0),
            pacing: Mutex::new(Pacing::default()),
            evt_tx: Mutex::new(None),
            default_width: width,
            default_height: height,
            zoom: AtomicU64::new(1.0f64.to_bits()),
        }
    }

    pub async fn current_session(&self) -> Option<String> {
        let tabs = self.tabs.lock().await;
        let idx = self.active.load(Ordering::SeqCst);
        tabs.get(idx).map(|t| t.session_id.clone())
    }

    pub async fn current_tab_index(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub async fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub async fn event_sender(&self) -> Option<mpsc::UnboundedSender<CdpEvent>> {
        self.evt_tx.lock().await.clone()
    }

    pub async fn push_console(&self, entry: Value) {
        let mut c = self.console.lock().await;
        c.push_back(entry);
        while c.len() > MAX_CONSOLE {
            c.pop_front();
        }
    }

    pub async fn push_error(&self, entry: Value) {
        let mut e = self.errors.lock().await;
        e.push_back(entry);
        while e.len() > MAX_CONSOLE {
            e.pop_front();
        }
    }

    pub fn meta(&self, url: String, title: String) -> BrowserEvent {
        BrowserEvent::Meta {
            surface: self.id.clone(),
            url,
            title,
            loading: self.loading.load(Ordering::SeqCst),
            can_go_back: self.can_go_back.load(Ordering::SeqCst),
            can_go_forward: self.can_go_forward.load(Ordering::SeqCst),
        }
    }

    pub async fn emit_meta(&self, tx: &broadcast::Sender<BrowserEvent>) {
        let url = self.url.lock().await.clone();
        let title = self.title.lock().await.clone();
        let _ = tx.send(self.meta(url, title));
    }
}

/// Spawn the per-surface CDP event loop.
pub(crate) fn spawn_event_loop(
    surface: Arc<SurfaceState>,
    conn: Arc<CdpConnection>,
    events: broadcast::Sender<BrowserEvent>,
    mut rx: mpsc::UnboundedReceiver<CdpEvent>,
) {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            handle_event(&surface, &conn, &events, ev).await;
        }
    });
}

async fn handle_event(
    surface: &Arc<SurfaceState>,
    conn: &Arc<CdpConnection>,
    events: &broadcast::Sender<BrowserEvent>,
    ev: CdpEvent,
) {
    let sid = match ev.session_id.clone() {
        Some(s) => s,
        None => return,
    };

    // Is this event for the active tab?
    let active = {
        let tabs = surface.tabs.lock().await;
        let idx = surface.active.load(Ordering::SeqCst);
        tabs.get(idx).map(|t| t.session_id == sid).unwrap_or(false)
    };

    match ev.method.as_str() {
        "Page.frameNavigated" => {
            let frame = &ev.params["frame"];
            let is_main = frame.get("parentId").map_or(true, Value::is_null);
            if is_main {
                let url = frame.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                let frame_id = frame.get("id").and_then(Value::as_str).map(str::to_string);
                let mut tabs = surface.tabs.lock().await;
                if let Some(t) = tabs.iter_mut().find(|t| t.session_id == sid) {
                    t.main_frame = frame_id;
                    t.url = url.clone();
                }
                drop(tabs);
                if active {
                    *surface.url.lock().await = url;
                }
                surface.emit_meta_active(events, active, &sid).await;
            }
        }
        "Page.frameStartedLoading" => {
            if has_main_frame(surface, &sid, &ev.params).await {
                let mut tabs = surface.tabs.lock().await;
                if let Some(t) = tabs.iter_mut().find(|t| t.session_id == sid) {
                    t.url = surface.url.lock().await.clone();
                }
                drop(tabs);
                if active {
                    surface.loading.store(true, Ordering::SeqCst);
                    surface.emit_meta(events).await;
                }
            }
        }
        "Page.frameStoppedLoading" => {
            if has_main_frame(surface, &sid, &ev.params).await && active {
                surface.loading.store(false, Ordering::SeqCst);
            }
            if active {
                refresh_state(surface, conn, events, &sid).await;
            }
        }
        "Page.loadEventFired" => {
            if active {
                surface.loading.store(false, Ordering::SeqCst);
                refresh_state(surface, conn, events, &sid).await;
            }
        }
        "Page.navigatedWithinDocument" => {
            if let Some(url) = ev.params.get("url").and_then(Value::as_str) {
                {
                    let mut tabs = surface.tabs.lock().await;
                    if let Some(t) = tabs.iter_mut().find(|t| t.session_id == sid) {
                        t.url = url.to_string();
                    }
                }
                if active {
                    *surface.url.lock().await = url.to_string();
                }
                surface.emit_meta_active(events, active, &sid).await;
            }
        }
        "Page.screencastFrame" => {
            handle_screencast_frame(surface, conn, &sid, events, &ev.params).await;
        }
        "Runtime.consoleAPICalled" => {
            let entry = console_entry(&ev.params);
            surface.push_console(entry).await;
        }
        "Runtime.exceptionThrown" => {
            let entry = exception_entry(&ev.params);
            surface.push_console(entry.clone()).await;
            surface.push_error(entry).await;
        }
        "Log.entryAdded" => {
            let entry = &ev.params["entry"];
            let level = entry.get("level").and_then(Value::as_str).unwrap_or("info");
            let rec = json!({
                "type": "log",
                "level": level,
                "text": entry.get("text").and_then(Value::as_str).unwrap_or(""),
                "url": entry.get("url").cloned().unwrap_or(Value::Null),
                "line": entry.get("lineNumber").cloned().unwrap_or(Value::Null),
            });
            surface.push_console(rec.clone()).await;
            if level == "error" {
                surface.push_error(rec).await;
            }
        }
        _ => {}
    }
}

impl SurfaceState {
    /// Update surface-level url/title from the owning tab's stored values when the
    /// event belonged to the active tab.
    async fn emit_meta_active(
        &self,
        events: &broadcast::Sender<BrowserEvent>,
        active: bool,
        sid: &str,
    ) {
        if !active {
            return;
        }
        let tabs = self.tabs.lock().await;
        if let Some(t) = tabs.iter().find(|t| t.session_id == sid) {
            *self.url.lock().await = t.url.clone();
            *self.title.lock().await = t.title.clone();
        }
        drop(tabs);
        self.emit_meta(events).await;
    }
}

async fn has_main_frame(surface: &Arc<SurfaceState>, sid: &str, params: &Value) -> bool {
    let frame_id = match params.get("frameId").and_then(Value::as_str) {
        Some(f) => f,
        None => return false,
    };
    let tabs = surface.tabs.lock().await;
    match tabs.iter().find(|t| t.session_id == sid) {
        Some(t) => t.main_frame.as_deref() == Some(frame_id),
        None => false,
    }
}

async fn refresh_state(
    surface: &Arc<SurfaceState>,
    conn: &Arc<CdpConnection>,
    events: &broadcast::Sender<BrowserEvent>,
    sid: &str,
) {
    let active = {
        let tabs = surface.tabs.lock().await;
        let idx = surface.active.load(Ordering::SeqCst);
        tabs.get(idx).map(|t| t.session_id == sid).unwrap_or(false)
    };
    if !active {
        return;
    }

    // url + title straight from the page.
    if let Ok(v) = conn
        .call(
            Some(sid),
            "Runtime.evaluate",
            json!({
                "expression": "({url: location.href, title: document.title})",
                "returnByValue": true,
            }),
        )
        .await
    {
        if let Some(val) = v.get("result").and_then(|r| r.get("value")) {
            if let Some(url) = val.get("url").and_then(Value::as_str) {
                *surface.url.lock().await = url.to_string();
            }
            if let Some(title) = val.get("title").and_then(Value::as_str) {
                *surface.title.lock().await = title.to_string();
            }
        }
    }

    // history flags.
    if let Ok(h) = conn
        .call(Some(sid), "Page.getNavigationHistory", json!({}))
        .await
    {
        let idx = h.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
        let len = h
            .get("entries")
            .and_then(Value::as_array)
            .map(|a| a.len() as i64)
            .unwrap_or(1);
        surface.can_go_back.store(idx > 0, Ordering::SeqCst);
        surface.can_go_forward.store(idx < len - 1, Ordering::SeqCst);
    }

    {
        let mut tabs = surface.tabs.lock().await;
        if let Some(t) = tabs.iter_mut().find(|t| t.session_id == sid) {
            t.url = surface.url.lock().await.clone();
            t.title = surface.title.lock().await.clone();
        }
    }
    surface.emit_meta(events).await;
}

async fn handle_screencast_frame(
    surface: &Arc<SurfaceState>,
    conn: &Arc<CdpConnection>,
    sid: &str,
    events: &broadcast::Sender<BrowserEvent>,
    params: &Value,
) {
    let data = match params.get("data").and_then(Value::as_str) {
        Some(d) => d,
        None => return,
    };
    let cdp_session = params.get("sessionId").and_then(Value::as_i64).unwrap_or(0);
    let metadata = params.get("metadata");
    let fallback = {
        let sc = surface.screencast.lock().await;
        match sc.as_ref() {
            Some(s) => (s.width as u16, s.height as u16),
            None => (surface.default_width as u16, surface.default_height as u16),
        }
    };

    let jpeg = match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(j) => j,
        Err(_) => return,
    };
    // Count encoder work: every frame Chrome produced, whether or not it is drawn.
    surface.produced.fetch_add(1, Ordering::SeqCst);

    // The real captured pixel size, from the JPEG itself; the metadata's device
    // size can be larger when the capture is downscaled.
    let (width, height) = jpeg_dimensions(&jpeg)
        .or_else(|| {
            let w = metadata
                .and_then(|m| m.get("deviceWidth"))
                .and_then(Value::as_u64)
                .map(|w| w as u16)?;
            let h = metadata
                .and_then(|m| m.get("deviceHeight"))
                .and_then(Value::as_u64)
                .map(|h| h as u16)?;
            Some((w, h))
        })
        .unwrap_or(fallback);

    // Ack Chrome immediately, before any client round-trip, so it keeps producing
    // at its own rate and the page's animation clock keeps running. Delivery to
    // the client is paced separately below.
    if cdp_session > 0 {
        let _ = conn
            .call(
                Some(sid),
                "Page.screencastFrameAck",
                json!({ "sessionId": cdp_session }),
            )
            .await;
    }

    let seq = surface.next_seq().await;
    let mut pacing = surface.pacing.lock().await;
    if deliver_on_produce(pacing.in_flight.as_ref().map(|f| f.seq)) {
        pacing.in_flight = Some(InFlight {
            seq,
            sent_at: Instant::now(),
        });
        pacing.latest = None;
        drop(pacing);
        let _ = events.send(BrowserEvent::Frame {
            surface: surface.id.clone(),
            seq,
            width,
            height,
            jpeg,
        });
    } else {
        // Keep only the newest; this replaces any older undelivered frame.
        pacing.latest = Some(LatestFrame {
            seq,
            width,
            height,
            jpeg,
        });
    }
}

fn arg_text(arg: &Value) -> String {
    if let Some(s) = arg.get("value").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(v) = arg.get("value") {
        if !v.is_null() {
            return v.to_string();
        }
    }
    if let Some(d) = arg.get("description").and_then(Value::as_str) {
        return d.to_string();
    }
    if let Some(n) = arg.get("unserializableValue").and_then(Value::as_str) {
        return n.to_string();
    }
    String::new()
}

fn console_entry(params: &Value) -> Value {
    let kind = params.get("type").and_then(Value::as_str).unwrap_or("log");
    let text = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .map(arg_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    json!({
        "type": kind,
        "level": if kind == "error" { "error" } else if kind == "warning" { "warning" } else { "info" },
        "text": text,
        "timestamp": params.get("timestamp").cloned().unwrap_or(Value::Null),
    })
}

fn exception_entry(params: &Value) -> Value {
    let d = &params["exceptionDetails"];
    let text = d
        .get("exception")
        .and_then(|e| e.get("description"))
        .and_then(Value::as_str)
        .or_else(|| d.get("text").and_then(Value::as_str))
        .unwrap_or("uncaught exception");
    json!({
        "type": "exception",
        "level": "error",
        "text": text,
        "url": d.get("url").cloned().unwrap_or(Value::Null),
        "line": d.get("lineNumber").cloned().unwrap_or(Value::Null),
        "column": d.get("columnNumber").cloned().unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod jpeg_tests {
    use super::jpeg_dimensions;

    #[test]
    fn reads_dimensions_from_sof() {
        let j = [
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, // APP0 (skipped)
            0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x90, 0x02, 0x80, 0x03, 0x00, 0x00, 0x00, // SOF0
            0xFF, 0xD9, // EOI
        ];
        assert_eq!(jpeg_dimensions(&j), Some((640, 400)));
    }

    #[test]
    fn rejects_non_jpeg() {
        assert_eq!(jpeg_dimensions(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]), None);
        assert_eq!(jpeg_dimensions(&[]), None);
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::{deliver_on_ack, deliver_on_produce};

    #[test]
    fn only_one_frame_in_flight() {
        // Nothing outstanding: the first produced frame is delivered.
        assert!(deliver_on_produce(None));
        // While one is outstanding, later frames are stored, not delivered.
        assert!(!deliver_on_produce(Some(1)));
        assert!(!deliver_on_produce(Some(9)));
    }

    #[test]
    fn ack_delivers_the_newest_replacement() {
        // A stored newer frame is delivered on ack.
        assert_eq!(deliver_on_ack(Some(5), 1), Some(5));
        // Nothing stored: nothing to deliver.
        assert_eq!(deliver_on_ack(None, 5), None);
        // Stored frame is not newer than the acked one: nothing to deliver.
        assert_eq!(deliver_on_ack(Some(5), 5), None);
        assert_eq!(deliver_on_ack(Some(4), 5), None);
    }

    /// Simulate a slow client that acks every third produced frame. Delivery must
    /// track the newest frame and must never accumulate a backlog.
    #[test]
    fn simulation_drops_to_newest_without_backlog() {
        let mut in_flight: Option<u32> = None;
        let mut latest: Option<u32> = None;
        let mut delivered: Vec<u32> = Vec::new();
        let produced = 12u32;

        for seq in 1..=produced {
            if deliver_on_produce(in_flight) {
                in_flight = Some(seq);
                latest = None;
                delivered.push(seq);
            } else {
                latest = Some(seq);
            }
            if seq % 3 == 0 {
                if let Some(acked) = in_flight.take() {
                    if let Some(new) = deliver_on_ack(latest, acked) {
                        in_flight = Some(new);
                        latest = None;
                        delivered.push(new);
                    }
                }
            }
        }

        // Only the newest available frame is ever sent; intermediates are dropped.
        assert_eq!(delivered, vec![1, 3, 6, 9, 12]);
        // Fewer frames delivered than produced: no backlog.
        assert!(delivered.len() < produced as usize);
        // The last delivered frame is the newest produced frame.
        assert_eq!(*delivered.last().unwrap(), produced);
        // At most one frame is ever outstanding.
        assert!(latest.is_none() || in_flight.is_some());
    }
}
