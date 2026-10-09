//! Minimal Chrome DevTools Protocol client over a single browser-level WebSocket.
//!
//! Uses CDP "flat" sessions: one `WebSocket` to the browser endpoint carries all
//! page targets, multiplexed by `sessionId`. Commands are matched to responses by
//! `id`; events are routed to the per-session channel registered with
//! [`CdpConnection::register`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

pub(crate) type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

#[derive(Debug, Clone)]
pub(crate) enum CdpError {
    /// The WebSocket closed (Chromium exited or crashed).
    Closed,
    /// Transport-level failure while sending.
    Io(String),
    /// Chrome replied with a JSON-RPC error.
    Remote { code: i64, message: String },
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Closed => write!(f, "CDP connection closed"),
            CdpError::Io(e) => write!(f, "CDP transport error: {e}"),
            CdpError::Remote { code, message } => write!(f, "CDP error {code}: {message}"),
        }
    }
}

impl std::error::Error for CdpError {}

/// A CDP event, routed either to a session channel or the browser channel.
#[derive(Debug, Clone)]
pub(crate) struct CdpEvent {
    pub session_id: Option<String>,
    pub method: String,
    pub params: Value,
}

struct ConnectionInner {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>,
    sink: Mutex<WsSink>,
    /// sessionId -> event channel for the owning surface.
    router: Mutex<HashMap<String, mpsc::UnboundedSender<CdpEvent>>>,
    /// Browser-level events (no sessionId): target lifecycle.
    browser_tx: Mutex<Option<mpsc::UnboundedSender<CdpEvent>>>,
    dead: AtomicBool,
}

/// Cheaply cloneable handle to one browser-level CDP connection.
#[derive(Clone)]
pub(crate) struct CdpConnection {
    inner: Arc<ConnectionInner>,
}

impl CdpConnection {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let (ws, _resp) = connect_async(url).await?;
        let (sink, stream) = ws.split();
        let inner = Arc::new(ConnectionInner {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            sink: Mutex::new(sink),
            router: Mutex::new(HashMap::new()),
            browser_tx: Mutex::new(None),
            dead: AtomicBool::new(false),
        });
        let conn = CdpConnection { inner: inner.clone() };
        tokio::spawn(read_loop(inner, stream));
        Ok(conn)
    }

    /// Send a command and await its result. `session` is `None` for browser-level
    /// commands (e.g. `Target.createTarget`).
    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, CdpError> {
        if self.inner.dead.load(Ordering::SeqCst) {
            return Err(CdpError::Closed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);

        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        let text = msg.to_string();
        {
            let mut sink = self.inner.sink.lock().await;
            if let Err(e) = sink.send(Message::Text(text)).await {
                self.inner.pending.lock().await.remove(&id);
                return Err(CdpError::Io(e.to_string()));
            }
        }
        rx.await.map_err(|_| CdpError::Closed)?
    }

    pub async fn register(&self, session_id: &str, tx: mpsc::UnboundedSender<CdpEvent>) {
        self.inner
            .router
            .lock()
            .await
            .insert(session_id.to_string(), tx);
    }

    pub async fn unregister(&self, session_id: &str) {
        self.inner.router.lock().await.remove(session_id);
    }

    pub async fn set_browser_sink(&self, tx: mpsc::UnboundedSender<CdpEvent>) {
        *self.inner.browser_tx.lock().await = Some(tx);
    }
}

async fn read_loop(inner: Arc<ConnectionInner>, mut stream: WsStream) {
    while let Some(m) = stream.next().await {
        let text = match m {
            Ok(Message::Text(t)) => t,
            Ok(Message::Binary(b)) => match String::from_utf8(b) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Ok(Message::Ping(p)) => {
                let _ = inner.sink.lock().await.send(Message::Pong(p)).await;
                continue;
            }
            Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => continue,
            Ok(Message::Close(_)) | Err(_) => break,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            if let Some(tx) = inner.pending.lock().await.remove(&id) {
                if let Some(err) = v.get("error") {
                    let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
                    let message = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("cdp error")
                        .to_string();
                    let _ = tx.send(Err(CdpError::Remote { code, message }));
                } else {
                    let _ = tx.send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
                }
            }
            continue;
        }

        let Some(method) = v.get("method").and_then(Value::as_str) else {
            continue;
        };
        let session_id = v
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let params = v.get("params").cloned().unwrap_or(Value::Null);

        if let Some(sid) = &session_id {
            let tx = inner.router.lock().await.get(sid).cloned();
            if let Some(tx) = tx {
                let _ = tx.send(CdpEvent {
                    session_id,
                    method: method.to_string(),
                    params,
                });
            }
        } else {
            let tx = inner.browser_tx.lock().await.clone();
            if let Some(tx) = tx {
                let _ = tx.send(CdpEvent {
                    session_id: None,
                    method: method.to_string(),
                    params,
                });
            }
        }
    }

    // Teardown: fail every in-flight command and close event channels so the
    // owning surface tasks exit and the supervisor can restart Chromium.
    inner.dead.store(true, Ordering::SeqCst);
    let pending: Vec<_> = inner.pending.lock().await.drain().map(|(_, v)| v).collect();
    for tx in pending {
        let _ = tx.send(Err(CdpError::Closed));
    }
    inner.router.lock().await.clear();
    *inner.browser_tx.lock().await = None;
}
