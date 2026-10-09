//! Adapter: `lmux-browser`'s concrete `BrowserManager` behind lmux-core's
//! `BrowserBackend` trait.
//!
//! This lives in the binary on purpose. `lmux-core` must not depend on
//! `lmux-browser` (so the daemon builds and tests without Chromium), and
//! `lmux-browser` must not depend on `lmux-core` (so the engine stands alone).
//! The binary is the only crate that sees both.

use std::path::PathBuf;

use async_trait::async_trait;
use lmux_browser::{BrowserConfig, BrowserManager};
use lmux_core::browser::{BrowserBackend, BrowserEvent};
use lmux_proto::{BrowserInputEvent, RpcError};
use tokio::sync::{broadcast, mpsc};

/// A `BrowserBackend` backed by a real headless Chromium.
pub struct ChromiumBackend {
    manager: BrowserManager,
}

impl ChromiumBackend {
    /// Launch Chromium and wrap it. `chromium_path` overrides the default
    /// (`/usr/bin/chromium`).
    pub async fn launch(chromium_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let mut config = BrowserConfig::default();
        if let Some(path) = chromium_path {
            config.chromium_path = path;
        }
        let manager = BrowserManager::launch(config).await?;
        Ok(Self { manager })
    }

    /// Stop Chromium. Called on SIGINT/SIGTERM: without it the child would be
    /// orphaned, because a signal kills this process without dropping the child.
    pub async fn shutdown(&self) {
        self.manager.shutdown().await;
    }
}

#[async_trait]
impl BrowserBackend for ChromiumBackend {
    async fn create_surface(&self, surface: &str, url: Option<&str>) -> anyhow::Result<()> {
        self.manager.create_surface(surface, url).await
    }

    async fn close_surface(&self, surface: &str) -> anyhow::Result<()> {
        self.manager.close_surface(surface).await
    }

    async fn handle_rpc(
        &self,
        surface: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        self.manager.handle_rpc(surface, method, params).await
    }

    async fn start_screencast(
        &self,
        surface: &str,
        width: u32,
        height: u32,
        device_scale_factor: f32,
    ) -> anyhow::Result<()> {
        self.manager
            .start_screencast(surface, width, height, device_scale_factor)
            .await
    }

    async fn stop_screencast(&self, surface: &str) -> anyhow::Result<()> {
        self.manager.stop_screencast(surface).await
    }

    async fn ack_frame(&self, surface: &str, seq: u32) -> anyhow::Result<()> {
        self.manager.ack_frame(surface, seq).await
    }

    async fn input(&self, surface: &str, event: BrowserInputEvent) -> anyhow::Result<()> {
        self.manager.input(surface, event).await
    }

    /// The engine broadcasts to any number of listeners; the daemon takes one
    /// stream, once, at startup (see `daemon::run`). Forward it into a channel
    /// the daemon owns.
    async fn subscribe_events(&self) -> anyhow::Result<mpsc::UnboundedReceiver<BrowserEvent>> {
        let mut from_engine = self.manager.events();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match from_engine.recv().await {
                    Ok(event) => {
                        if tx.send(convert(event)).is_err() {
                            break; // the daemon is gone
                        }
                    }
                    // Frames are latest-wins, so a lagging consumer loses
                    // nothing that matters.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "browser event stream lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(rx)
    }
}

/// Both enums are foreign to this crate, so the conversion is a function rather
/// than a `From` impl (the orphan rule forbids the impl here).
fn convert(event: lmux_browser::BrowserEvent) -> BrowserEvent {
    match event {
        lmux_browser::BrowserEvent::Frame {
            surface,
            seq,
            width,
            height,
            jpeg,
        } => BrowserEvent::Frame {
            surface,
            seq,
            width,
            height,
            jpeg,
        },
        lmux_browser::BrowserEvent::Meta {
            surface,
            url,
            title,
            loading,
            can_go_back,
            can_go_forward,
        } => BrowserEvent::Meta {
            surface,
            url,
            title,
            loading,
            can_go_back,
            can_go_forward,
        },
        lmux_browser::BrowserEvent::Closed { surface } => BrowserEvent::Closed { surface },
    }
}
