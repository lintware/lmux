//! WebKit browsers on the host: a helper process (`helpers/webkit-mac`, a
//! Swift program with WKWebView, or `helpers/webkit-linux`, Python with
//! WebKitGTK under Xvfb) runs the pages off screen, so they live on the host
//! like terminals do. The app watches them through the same frame stream as
//! Chromium's, and agents drive them with the same commands
//! ([`crate::app_browser`]).
//!
//! The helper speaks JSON lines on stdin/stdout. Requests carry an `id` and
//! are answered with `{"id", "value"}` or `{"id", "error"}`:
//!
//! - `{"cmd":"create","surface","url"}` / `{"cmd":"close","surface"}`
//! - `{"cmd":"step","surface","op":AppBrowserOp}`: an eval answers with
//!   WebKit's JSON for the value, as text; a screenshot with `{"png_base64"}`
//! - `{"cmd":"screencast","surface","width","height","scale"}` (width 0 stops)
//!
//! - `{"cmd":"adopt","page","surface"}`: the window the helper announced with
//!   a `popup` event is now `surface`.
//!
//! Without an `id`, and unanswered: `{"cmd":"ack","surface","seq"}` (a frame
//! was drawn; the next may come) and `{"cmd":"input","surface","event"}`.
//!
//! The helper sends `{"event":"frame","surface","seq","width","height","jpeg"}`
//! (base64), `{"event":"meta","surface","url","title","loading",
//! "can_go_back","can_go_forward"}`, `{"event":"closed","surface"}` (the page
//! called `window.close()`) and `{"event":"popup","opener","page","url"}`: the
//! page opened a window. The helper keeps it related to its opener, so
//! `window.opener` and `postMessage` work, and names it `page` until `adopt`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use lmux_proto::{AppBrowserOp, BrowserEvent, BrowserInputEvent, Ref, RpcError, RpcErrorCode};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::app_browser::Steps;
use crate::browser::BrowserBackend;

/// How long a request may take before the agent is told the helper is stuck.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

type Pending = HashMap<u64, oneshot::Sender<Result<Value, String>>>;

#[derive(Default)]
struct Shared {
    /// Requests waiting for their answer, by id.
    pending: Pending,
    /// The running helper's stdin, as lines to write.
    tx: Option<mpsc::UnboundedSender<String>>,
    /// Where events go: one sender per `subscribe_events`.
    listeners: Vec<mpsc::UnboundedSender<BrowserEvent>>,
    /// Every page and the URL it is at, to reopen them in a new helper.
    pages: HashMap<Ref, String>,
    /// Screencasts running, to restart them in a new helper.
    casts: HashMap<Ref, (u32, u32, f32)>,
}

/// The WebKit helper, kept running: it is started again (with its pages) if
/// it exits.
pub struct WebKitBackend {
    shared: Arc<Mutex<Shared>>,
    next_id: AtomicU64,
}

impl WebKitBackend {
    /// Start `program` (the helper) and wait for it to say it is ready.
    pub async fn launch(program: std::path::PathBuf) -> anyhow::Result<Self> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (ready_tx, ready_rx) = oneshot::channel();
        tokio::spawn(supervise(program, shared.clone(), ready_tx));
        match tokio::time::timeout(Duration::from_secs(20), ready_rx).await {
            Ok(Ok(Ok(()))) => Ok(WebKitBackend { shared, next_id: AtomicU64::new(1) }),
            Ok(Ok(Err(e))) => Err(anyhow::anyhow!(e)),
            _ => Err(anyhow::anyhow!("the WebKit helper did not start")),
        }
    }

    fn send(&self, msg: Value) {
        if let Some(tx) = &self.shared.lock().unwrap().tx {
            let _ = tx.send(msg.to_string());
        }
    }

    async fn call(&self, mut msg: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        msg["id"] = json!(id);
        let rx = {
            let mut s = self.shared.lock().unwrap();
            let Some(tx) = s.tx.clone() else {
                return Err("the WebKit helper is not running".into());
            };
            let (otx, orx) = oneshot::channel();
            s.pending.insert(id, otx);
            let _ = tx.send(msg.to_string());
            orx
        };
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the WebKit helper exited".into()),
            Err(_) => {
                self.shared.lock().unwrap().pending.remove(&id);
                Err("the WebKit helper did not answer".into())
            }
        }
    }
}

/// Run the helper, and run it again when it exits, reopening its pages.
async fn supervise(
    program: std::path::PathBuf,
    shared: Arc<Mutex<Shared>>,
    ready: oneshot::Sender<Result<(), String>>,
) {
    let mut ready = Some(ready);
    loop {
        let child = Command::new(&program)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                if let Some(r) = ready.take() {
                    let _ = r.send(Err(format!("start {}: {e}", program.display())));
                    return;
                }
                tracing::warn!("WebKit helper: {e}; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let mut stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });

        let mut lines = BufReader::new(stdout).lines();
        let mut started = false;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                tracing::warn!("WebKit helper said: {line}");
                continue;
            };
            if !started && msg.get("event").and_then(Value::as_str) == Some("ready") {
                started = true;
                let (pages, casts) = {
                    let mut s = shared.lock().unwrap();
                    s.tx = Some(tx.clone());
                    (s.pages.clone(), s.casts.clone())
                };
                // Every page runs the console capture from its start.
                let init = json!({"cmd": "init", "scripts": [lmux_proto::browser_js::CAPTURE_INIT]});
                let _ = tx.send(init.to_string());
                // A helper started again gets back every page, at its URL.
                for (surface, url) in pages {
                    let _ = tx.send(json!({"cmd": "create", "surface": surface, "url": url}).to_string());
                }
                for (surface, (w, h, scale)) in casts {
                    let _ = tx.send(json!({"cmd": "screencast", "surface": surface, "width": w, "height": h, "scale": scale}).to_string());
                }
                tracing::info!("WebKit helper ready");
                if let Some(r) = ready.take() {
                    let _ = r.send(Ok(()));
                }
                continue;
            }
            handle_message(&shared, msg);
        }

        // Gone: fail what was waiting, and start it again.
        let status = child.wait().await;
        {
            let mut s = shared.lock().unwrap();
            s.tx = None;
            for (_, w) in s.pending.drain() {
                let _ = w.send(Err("the WebKit helper exited".into()));
            }
        }
        if let Some(r) = ready.take() {
            let _ = r.send(Err(format!("the WebKit helper exited at start ({status:?})")));
            return;
        }
        tracing::warn!("WebKit helper exited ({status:?}); starting it again");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn handle_message(shared: &Arc<Mutex<Shared>>, msg: Value) {
    if let Some(id) = msg.get("id").and_then(Value::as_u64) {
        let waiter = shared.lock().unwrap().pending.remove(&id);
        if let Some(w) = waiter {
            let r = match msg.get("error").and_then(Value::as_str) {
                Some(e) => Err(e.to_owned()),
                None => Ok(msg.get("value").cloned().unwrap_or(Value::Null)),
            };
            let _ = w.send(r);
        }
        return;
    }
    let s = |k: &str| msg.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let event = match msg.get("event").and_then(Value::as_str) {
        Some("frame") => {
            let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(s("jpeg")) else { return };
            let n = |k: &str| msg.get(k).and_then(Value::as_u64).unwrap_or(0);
            BrowserEvent::Frame {
                surface: s("surface"),
                seq: n("seq") as u32,
                width: n("width") as u16,
                height: n("height") as u16,
                jpeg,
            }
        }
        Some("meta") => {
            let b = |k: &str| msg.get(k).and_then(Value::as_bool).unwrap_or(false);
            let (surface, url) = (s("surface"), s("url"));
            if let Some(p) = shared.lock().unwrap().pages.get_mut(&surface) {
                if !url.is_empty() {
                    *p = url.clone();
                }
            }
            BrowserEvent::Meta {
                surface,
                url,
                title: s("title"),
                loading: b("loading"),
                can_go_back: b("can_go_back"),
                can_go_forward: b("can_go_forward"),
            }
        }
        Some("closed") => {
            // Gone for good (a popup calling `window.close()`): a restarted
            // helper must not reopen it.
            let surface = s("surface");
            let mut sh = shared.lock().unwrap();
            sh.pages.remove(&surface);
            sh.casts.remove(&surface);
            drop(sh);
            BrowserEvent::Closed { surface }
        }
        Some("popup") => BrowserEvent::Popup { opener: s("opener"), page: s("page"), url: s("url") },
        _ => return,
    };
    shared.lock().unwrap().listeners.retain(|l| l.send(event.clone()).is_ok());
}

fn internal(e: String) -> RpcError {
    RpcError { code: RpcErrorCode::Internal, message: e }
}

#[async_trait]
impl Steps for WebKitBackend {
    async fn step(&self, surface: &str, op: AppBrowserOp) -> Result<Value, RpcError> {
        self.call(json!({"cmd": "step", "surface": surface, "op": op})).await.map_err(internal)
    }
}

#[async_trait]
impl BrowserBackend for WebKitBackend {
    async fn create_surface(&self, surface: &str, url: Option<&str>) -> anyhow::Result<()> {
        let url = url.unwrap_or("about:blank").to_owned();
        self.shared.lock().unwrap().pages.insert(surface.to_owned(), url.clone());
        self.call(json!({"cmd": "create", "surface": surface, "url": url}))
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e))
    }

    async fn close_surface(&self, surface: &str) -> anyhow::Result<()> {
        {
            let mut s = self.shared.lock().unwrap();
            s.pages.remove(surface);
            s.casts.remove(surface);
        }
        self.call(json!({"cmd": "close", "surface": surface})).await.map(|_| ()).map_err(|e| anyhow::anyhow!(e))
    }

    async fn handle_rpc(&self, surface: &str, method: &str, params: Value) -> Result<Value, RpcError> {
        crate::app_browser::handle_rpc(self, surface, method, &params).await
    }

    async fn start_screencast(&self, surface: &str, width: u32, height: u32, scale: f32) -> anyhow::Result<()> {
        self.shared.lock().unwrap().casts.insert(surface.to_owned(), (width, height, scale));
        self.call(json!({"cmd": "screencast", "surface": surface, "width": width, "height": height, "scale": scale}))
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e))
    }

    async fn stop_screencast(&self, surface: &str) -> anyhow::Result<()> {
        self.shared.lock().unwrap().casts.remove(surface);
        self.call(json!({"cmd": "screencast", "surface": surface, "width": 0, "height": 0, "scale": 1.0}))
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e))
    }

    async fn ack_frame(&self, surface: &str, seq: u32) -> anyhow::Result<()> {
        self.send(json!({"cmd": "ack", "surface": surface, "seq": seq}));
        Ok(())
    }

    async fn input(&self, surface: &str, event: BrowserInputEvent) -> anyhow::Result<()> {
        self.send(json!({"cmd": "input", "surface": surface, "event": event}));
        Ok(())
    }

    async fn subscribe_events(&self) -> anyhow::Result<mpsc::UnboundedReceiver<BrowserEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.shared.lock().unwrap().listeners.push(tx);
        Ok(rx)
    }

    async fn adopt_popup(&self, page: &str, surface: &str, url: &str) -> anyhow::Result<()> {
        // Reopened at this URL (without its opener) if the helper restarts.
        self.shared.lock().unwrap().pages.insert(surface.to_owned(), url.to_owned());
        self.call(json!({"cmd": "adopt", "page": page, "surface": surface}))
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_messages_become_events_and_answers() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.lock().unwrap().listeners.push(tx);
        shared.lock().unwrap().pages.insert("surface:1".into(), "about:blank".into());

        let (otx, mut orx) = oneshot::channel();
        shared.lock().unwrap().pending.insert(7, otx);
        handle_message(&shared, json!({"id": 7, "value": "\"ok\""}));
        assert_eq!(orx.try_recv().unwrap(), Ok(json!("\"ok\"")));

        handle_message(&shared, json!({"event": "frame", "surface": "surface:1", "seq": 3, "width": 2, "height": 1, "jpeg": "AQID"}));
        match rx.try_recv().unwrap() {
            BrowserEvent::Frame { surface, seq, jpeg, .. } => assert_eq!((surface.as_str(), seq, jpeg), ("surface:1", 3, vec![1, 2, 3])),
            other => panic!("{other:?}"),
        }
        handle_message(&shared, json!({"event": "meta", "surface": "surface:1", "url": "https://a.test/", "title": "A", "loading": false}));
        assert!(matches!(rx.try_recv().unwrap(), BrowserEvent::Meta { .. }));
        // A new helper would reopen the page where it now is.
        assert_eq!(shared.lock().unwrap().pages["surface:1"], "https://a.test/");

        // A window the page opened is announced under the helper's own name.
        handle_message(&shared, json!({"event": "popup", "opener": "surface:1", "page": "popup:1", "url": "https://b.test/"}));
        match rx.try_recv().unwrap() {
            BrowserEvent::Popup { opener, page, url } => {
                assert_eq!((opener.as_str(), page.as_str(), url.as_str()), ("surface:1", "popup:1", "https://b.test/"))
            }
            other => panic!("{other:?}"),
        }
        // One that closed itself is gone for good: a restarted helper must
        // not reopen it.
        shared.lock().unwrap().pages.insert("surface:2".into(), "https://b.test/".into());
        handle_message(&shared, json!({"event": "closed", "surface": "surface:2"}));
        assert!(matches!(rx.try_recv().unwrap(), BrowserEvent::Closed { .. }));
        assert!(!shared.lock().unwrap().pages.contains_key("surface:2"));
    }
}
