//! The browser boundary. `crates/lmux-browser` implements a concrete backend and
//! the orchestrator wires it in; lmux-core only knows this trait so it can build
//! and test without Chromium.
//!
//! `async-trait` is used because `async fn` in traits is not dyn-compatible on
//! rustc 1.88. The daemon holds `Option<Arc<dyn BrowserBackend>>`.

use async_trait::async_trait;
use lmux_proto::{BrowserInputEvent, RpcError, RpcErrorCode};
use tokio::sync::mpsc;

/// Events a backend pushes back into the daemon. Defined in `lmux-proto` so the
/// engine and the daemon share one type without depending on each other.
pub use lmux_proto::BrowserEvent;

#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Create the page for `surface`. `url` is optional (defaults to about:blank).
    async fn create_surface(&self, surface: &str, url: Option<&str>) -> anyhow::Result<()>;

    /// Tear down the page for `surface`.
    async fn close_surface(&self, surface: &str) -> anyhow::Result<()>;

    /// Every `browser.*` RPC except `browser.open` (the daemon creates the
    /// surface/pane, then calls `create_surface`). `params` is the raw JSON.
    async fn handle_rpc(
        &self,
        surface: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError>;

    async fn start_screencast(
        &self,
        surface: &str,
        width: u32,
        height: u32,
        device_scale_factor: f32,
    ) -> anyhow::Result<()>;

    async fn stop_screencast(&self, surface: &str) -> anyhow::Result<()>;

    async fn ack_frame(&self, surface: &str, seq: u32) -> anyhow::Result<()>;

    async fn input(&self, surface: &str, event: BrowserInputEvent) -> anyhow::Result<()>;

    /// A receiver for backend-pushed events. Each call yields a fresh receiver.
    async fn subscribe_events(&self) -> anyhow::Result<mpsc::UnboundedReceiver<BrowserEvent>>;
}

pub fn unsupported(method: &str) -> RpcError {
    RpcError {
        code: RpcErrorCode::Unsupported,
        message: format!("browser backend not available for {method}"),
    }
}

/// A stand-in backend for tests and for running without Chromium.
#[derive(Default)]
pub struct FakeBrowser {
    tx: std::sync::Mutex<Option<mpsc::UnboundedSender<BrowserEvent>>>,
    pub calls: std::sync::Mutex<Vec<String>>,
}

impl FakeBrowser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_channel() -> (Self, mpsc::UnboundedReceiver<BrowserEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let f = FakeBrowser {
            tx: std::sync::Mutex::new(Some(tx)),
            calls: std::sync::Mutex::new(Vec::new()),
        };
        (f, rx)
    }

    pub fn emit(&self, ev: BrowserEvent) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(ev);
        }
    }

    fn log(&self, s: impl Into<String>) {
        self.calls.lock().unwrap().push(s.into());
    }
}

#[async_trait]
impl BrowserBackend for FakeBrowser {
    async fn create_surface(&self, surface: &str, url: Option<&str>) -> anyhow::Result<()> {
        self.log(format!("create {surface} {url:?}"));
        Ok(())
    }
    async fn close_surface(&self, surface: &str) -> anyhow::Result<()> {
        self.log(format!("close {surface}"));
        Ok(())
    }
    async fn handle_rpc(
        &self,
        _surface: &str,
        method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        self.log(format!("rpc {method}"));
        Ok(serde_json::json!({"fake": method}))
    }
    async fn start_screencast(&self, surface: &str, w: u32, h: u32, _d: f32) -> anyhow::Result<()> {
        self.log(format!("start {surface} {w}x{h}"));
        Ok(())
    }
    async fn stop_screencast(&self, surface: &str) -> anyhow::Result<()> {
        self.log(format!("stop {surface}"));
        Ok(())
    }
    async fn ack_frame(&self, surface: &str, seq: u32) -> anyhow::Result<()> {
        self.log(format!("ack {surface} {seq}"));
        Ok(())
    }
    async fn input(&self, surface: &str, _event: BrowserInputEvent) -> anyhow::Result<()> {
        self.log(format!("input {surface}"));
        Ok(())
    }
    async fn subscribe_events(&self) -> anyhow::Result<mpsc::UnboundedReceiver<BrowserEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.tx.lock().unwrap() = Some(tx);
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_records_calls() {
        let f = FakeBrowser::new();
        f.create_surface("surface:1", Some("http://x")).await.unwrap();
        f.close_surface("surface:1").await.unwrap();
        let calls = f.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains("create surface:1"));
    }
}
